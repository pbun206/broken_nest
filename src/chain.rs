use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::config::ChainConfig;
use crate::error::Error;
use crate::jalv::{JalvInstance, UiMode};
use crate::midi::{self, MidiRouter, MidiSender};

const MIDI_RING_CAPACITY: usize = 1024;

/// A running chain of LV2 plugins wired in series.
///
/// Each plugin runs as a separate `jalv` process. Audio ports are connected
/// left-to-right via the JACK API. If any plugin exposes MIDI input, a dedicated
/// JACK client is created to route [`MidiEvent`](crate::MidiEvent)s from
/// user code into the corresponding plugin.
///
/// Drop or call [`Chain::stop`] to tear down all child processes and JACK
/// connections.
pub struct Chain {
    config: ChainConfig,
    instances: Vec<JalvInstance>,
    midi_router: Option<MidiRouter>,
    midi_senders: HashMap<String, MidiSender>,
}

impl Chain {
    /// Spawn all plugins, wire audio ports in series, and set up MIDI routing.
    ///
    /// Plugins start headless (no GUI window) by default. Call [`show_ui`] or
    /// [`show_all_ui`] to open plugin windows.
    ///
    /// If `ChainConfig::state_dir` is set, saved control values are restored
    /// automatically.
    pub fn start(config: &ChainConfig) -> Result<Self, Error> {
        if config.plugins.is_empty() {
            return Err(Error::Config("chain has no plugins".into()));
        }

        let state_dir = state_dir_for(&config.name);
        let mut instances = Vec::with_capacity(config.plugins.len());

        for plugin in &config.plugins {
            let saved = load_controls(&state_dir, &plugin.name);
            let mode = if plugin.show_ui { UiMode::Gtk } else { UiMode::Headless };
            let instance = JalvInstance::spawn(plugin, config.buffer_size, mode, &saved)?;
            instances.push(instance);
        }

        // Wire adjacent plugins (audio)
        for i in 0..instances.len() - 1 {
            let from = instances[i].jack_client_name().to_string();
            let to = instances[i + 1].jack_client_name().to_string();
            wire_plugins(&from, &to)?;
        }

        if config.auto_connect_input || config.auto_connect_output {
            auto_connect(&instances, config)?;
        }

        // MIDI routing
        let mut midi_senders = HashMap::new();
        let mut midi_ports = Vec::new();

        for (instance, plugin_cfg) in instances.iter().zip(&config.plugins) {
            let name = instance.jack_client_name();
            let wants_midi = plugin_cfg
                .midi_in
                .unwrap_or_else(|| has_midi_input(name));
            if wants_midi {
                let (sender, port) = midi::create_midi_channel(name, MIDI_RING_CAPACITY);
                midi_senders.insert(name.to_string(), sender);
                midi_ports.push(port);
            }
        }

        let midi_router = if !midi_ports.is_empty() {
            let router_name = format!("{}-midi", config.jack_client_prefix);
            let router = MidiRouter::new(&router_name, midi_ports)?;

            thread::sleep(Duration::from_millis(200));
            for instance in &instances {
                let name = instance.jack_client_name();
                if midi_senders.contains_key(name) {
                    wire_midi(&format!("{}-midi", config.jack_client_prefix), name)?;
                }
            }

            Some(router)
        } else {
            None
        };

        Ok(Self {
            config: config.clone(),
            instances,
            midi_router,
            midi_senders,
        })
    }

    /// Set a control port value on a running plugin.
    pub fn set_control(
        &mut self,
        plugin_name: &str,
        port: &str,
        value: f32,
    ) -> Result<(), Error> {
        let instance = self
            .instances
            .iter_mut()
            .find(|i| i.jack_client_name() == plugin_name)
            .ok_or_else(|| Error::PluginNotFound {
                name: plugin_name.into(),
            })?;
        instance.set_control(port, value)
    }

    /// Get a [`MidiSender`] for pushing MIDI events to the named plugin.
    pub fn midi_sender(&mut self, plugin_name: &str) -> Result<&mut MidiSender, Error> {
        self.midi_senders
            .get_mut(plugin_name)
            .ok_or_else(|| Error::PluginNotFound {
                name: plugin_name.into(),
            })
    }

    /// Get the [`MidiSender`] when exactly one plugin in the chain accepts MIDI.
    pub fn sole_midi_sender(&mut self) -> Result<&mut MidiSender, Error> {
        if self.midi_senders.len() != 1 {
            return Err(Error::AmbiguousMidi {
                count: self.midi_senders.len(),
            });
        }
        Ok(self.midi_senders.values_mut().next().unwrap())
    }

    /// Show the plugin UI window. Respawns without xvfb-run if currently headless.
    /// Brief audio gap (~1s) during respawn.
    pub fn show_ui(&mut self, plugin_name: &str) -> Result<(), Error> {
        let idx = self.find_plugin_idx(plugin_name)?;

        if self.instances[idx].ui_mode() == UiMode::Gtk && self.instances[idx].is_running() {
            return Ok(());
        }

        self.respawn_plugin(idx, UiMode::Gtk)
    }

    /// Hide the plugin UI window. Respawns under xvfb-run.
    /// Brief audio gap (~1s) during respawn.
    pub fn hide_ui(&mut self, plugin_name: &str) -> Result<(), Error> {
        let idx = self.find_plugin_idx(plugin_name)?;

        if self.instances[idx].ui_mode() == UiMode::Headless && self.instances[idx].is_running() {
            return Ok(());
        }

        self.respawn_plugin(idx, UiMode::Headless)
    }

    /// Show UI windows for all plugins.
    pub fn show_all_ui(&mut self) -> Result<(), Error> {
        let names: Vec<String> = self.instances.iter().map(|i| i.name.clone()).collect();
        for name in names {
            self.show_ui(&name)?;
        }
        Ok(())
    }

    /// Hide UI windows for all plugins.
    pub fn hide_all_ui(&mut self) -> Result<(), Error> {
        let names: Vec<String> = self.instances.iter().map(|i| i.name.clone()).collect();
        for name in names {
            self.hide_ui(&name)?;
        }
        Ok(())
    }

    /// Check all plugins and auto-respawn any that died (headless).
    pub fn check_health(&mut self) {
        for idx in 0..self.instances.len() {
            if !self.instances[idx].is_running() {
                log::warn!("plugin '{}' died, respawning headless", self.instances[idx].name);
                if let Err(e) = self.respawn_plugin(idx, UiMode::Headless) {
                    log::error!("failed to respawn '{}': {e}", self.config.plugins[idx].name);
                }
            }
        }
    }

    /// Tear down the chain: save state, drop MIDI resources, kill all jalv processes.
    pub fn stop(&mut self) {
        self.save_all_state();
        self.midi_senders.clear();
        drop(self.midi_router.take());
        for instance in &mut self.instances {
            instance.kill();
        }
        self.instances.clear();
    }

    /// Returns `true` if every plugin in the chain is still alive.
    pub fn is_running(&mut self) -> bool {
        self.instances.iter_mut().all(|i| i.is_running())
    }

    /// Port names for the first plugin's audio inputs.
    pub fn chain_input_ports(&self) -> Option<Vec<String>> {
        let name = self.instances.first()?.jack_client_name();
        let ports = get_ports(name, PortDirection::Input).ok()?;
        let audio: Vec<_> = ports.into_iter().filter(|p| is_audio_port(p)).collect();
        if audio.is_empty() { None } else { Some(audio) }
    }

    /// Port names for the last plugin's audio outputs.
    pub fn chain_output_ports(&self) -> Option<Vec<String>> {
        let name = self.instances.last()?.jack_client_name();
        let ports = get_ports(name, PortDirection::Output).ok()?;
        let audio: Vec<_> = ports.into_iter().filter(|p| is_audio_port(p)).collect();
        if audio.is_empty() { None } else { Some(audio) }
    }

    /// Snapshot of control values for all plugins.
    pub fn all_controls(&self) -> HashMap<String, HashMap<String, f32>> {
        self.instances
            .iter()
            .map(|i| (i.name.clone(), i.current_controls()))
            .collect()
    }

    fn find_plugin_idx(&self, name: &str) -> Result<usize, Error> {
        self.instances
            .iter()
            .position(|i| i.jack_client_name() == name)
            .ok_or_else(|| Error::PluginNotFound { name: name.into() })
    }

    fn respawn_plugin(&mut self, idx: usize, mode: UiMode) -> Result<(), Error> {
        let controls = self.instances[idx].current_controls();
        self.instances[idx].kill();

        let plugin = &self.config.plugins[idx];
        let instance = JalvInstance::spawn(plugin, self.config.buffer_size, mode, &controls)?;
        self.instances[idx] = instance;

        // Rewire audio: previous plugin → this → next plugin
        if idx > 0 {
            let from = self.instances[idx - 1].jack_client_name().to_string();
            let to = self.instances[idx].jack_client_name().to_string();
            wire_plugins(&from, &to)?;
        }
        if idx + 1 < self.instances.len() {
            let from = self.instances[idx].jack_client_name().to_string();
            let to = self.instances[idx + 1].jack_client_name().to_string();
            wire_plugins(&from, &to)?;
        }

        // Rewire auto-connect if this is first/last
        if idx == 0 && self.config.auto_connect_input {
            auto_connect_chain(
                self.instances[0].jack_client_name(),
                PortDirection::Input,
            )?;
        }
        if idx == self.instances.len() - 1 && self.config.auto_connect_output {
            auto_connect_chain(
                self.instances.last().unwrap().jack_client_name(),
                PortDirection::Output,
            )?;
        }

        // Rewire MIDI if applicable
        let name = self.instances[idx].jack_client_name().to_string();
        if self.midi_senders.contains_key(&name) {
            wire_midi(&format!("{}-midi", self.config.jack_client_prefix), &name)?;
        }

        Ok(())
    }

    fn save_all_state(&self) {
        let state_dir = state_dir_for(&self.config.name);

        if let Err(e) = fs::create_dir_all(&state_dir) {
            log::error!("failed to create state dir {}: {e}", state_dir.display());
            return;
        }

        for instance in &self.instances {
            let controls = instance.current_controls();
            if controls.is_empty() {
                continue;
            }
            let path = state_dir.join(format!("{}.toml", instance.name));
            if let Err(e) = save_controls_file(&path, &controls) {
                log::error!("failed to save state for '{}': {e}", instance.name);
            } else {
                log::debug!("saved state for '{}' ({} controls)", instance.name, controls.len());
            }
        }
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- State I/O ---

#[derive(serde::Serialize, serde::Deserialize)]
struct ControlState {
    controls: HashMap<String, f32>,
}

fn state_dir_for(chain_name: &str) -> PathBuf {
    let base = env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".local/share")
        });
    base.join("broken_nest").join(chain_name)
}

fn save_controls_file(path: &Path, controls: &HashMap<String, f32>) -> Result<(), std::io::Error> {
    let state = ControlState {
        controls: controls.clone(),
    };
    let toml_str = toml::to_string_pretty(&state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(path, toml_str)
}

fn load_controls(state_dir: &Path, plugin_name: &str) -> HashMap<String, f32> {
    let path = state_dir.join(format!("{plugin_name}.toml"));
    if !path.exists() {
        return HashMap::new();
    }
    match fs::read_to_string(&path) {
        Ok(contents) => match toml::from_str::<ControlState>(&contents) {
            Ok(state) => {
                log::debug!(
                    "restored {} controls for '{plugin_name}'",
                    state.controls.len()
                );
                state.controls
            }
            Err(e) => {
                log::warn!("invalid state file {}: {e}", path.display());
                HashMap::new()
            }
        },
        Err(e) => {
            log::warn!("failed to read state file {}: {e}", path.display());
            HashMap::new()
        }
    }
}

// --- Auto-connect (pw-link) ---

fn auto_connect(instances: &[JalvInstance], config: &ChainConfig) -> Result<(), Error> {
    if config.auto_connect_input {
        if let Some(first) = instances.first() {
            auto_connect_chain(first.jack_client_name(), PortDirection::Input)?;
        }
    }

    if config.auto_connect_output {
        if let Some(last) = instances.last() {
            auto_connect_chain(last.jack_client_name(), PortDirection::Output)?;
        }
    }

    Ok(())
}

fn auto_connect_chain(chain_client: &str, direction: PortDirection) -> Result<(), Error> {
    log::debug!("auto-connect {chain_client} direction={direction:?}");

    match direction {
        PortDirection::Input => {
            let capture_ports = get_physical_ports(PortDirection::Output)?;
            log::debug!("physical capture ports: {capture_ports:?}");
            let chain_ins = get_ports(chain_client, PortDirection::Input)?;
            let audio_ins: Vec<_> = chain_ins.into_iter().filter(|p| is_audio_port(p)).collect();
            log::debug!("chain audio input ports: {audio_ins:?}");
            for (src, dst) in capture_ports.iter().zip(audio_ins.iter()) {
                pw_link(src, dst)?;
            }
            Ok(())
        }
        PortDirection::Output => {
            let playback_ports = get_physical_ports(PortDirection::Input)?;
            log::debug!("physical playback ports: {playback_ports:?}");
            let chain_outs = get_ports(chain_client, PortDirection::Output)?;
            let audio_outs: Vec<_> = chain_outs.into_iter().filter(|p| is_audio_port(p)).collect();
            log::debug!("chain audio output ports: {audio_outs:?}");
            for (src, dst) in audio_outs.iter().zip(playback_ports.iter()) {
                pw_link(src, dst)?;
            }
            Ok(())
        }
    }
}

// --- Port wiring (pw-link) ---

const PORT_POLL_ATTEMPTS: u32 = 15;
const PORT_POLL_INTERVAL_MS: u64 = 200;

fn get_ports(client: &str, direction: PortDirection) -> Result<Vec<String>, Error> {
    let flag = match direction {
        PortDirection::Output => "-o",
        PortDirection::Input => "-i",
    };

    for attempt in 0..PORT_POLL_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(PORT_POLL_INTERVAL_MS));
        }

        let output = Command::new("pw-link")
            .arg(flag)
            .output()
            .map_err(|e| Error::Wiring(format!("failed to run pw-link: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let ports: Vec<String> = stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with(client))
            .collect();

        if !ports.is_empty() {
            log::debug!("get_ports({client}, {direction:?}): found {ports:?}");
            return Ok(ports);
        }

        log::debug!(
            "get_ports({client}, {direction:?}): attempt {}/{PORT_POLL_ATTEMPTS} — no ports yet",
            attempt + 1,
        );
    }

    Err(Error::Wiring(format!(
        "timeout waiting for ports from {client} (direction={direction:?})"
    )))
}

fn get_physical_ports(direction: PortDirection) -> Result<Vec<String>, Error> {
    let flag = match direction {
        PortDirection::Output => "-o",
        PortDirection::Input => "-i",
    };

    let output = Command::new("pw-link")
        .arg(flag)
        .output()
        .map_err(|e| Error::Wiring(format!("failed to run pw-link: {e}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let ports: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    log::debug!("get_physical_ports({direction:?}): {ports:?}");
    Ok(ports)
}

fn pw_link(from: &str, to: &str) -> Result<(), Error> {
    let output = Command::new("pw-link")
        .arg(from)
        .arg(to)
        .output()
        .map_err(|e| Error::Wiring(format!("failed to run pw-link: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = stderr.trim();
        if msg.contains("already linked") {
            log::debug!("already linked: {from} → {to}");
            return Ok(());
        }
        return Err(Error::Wiring(format!(
            "pw-link {from} → {to} failed: {msg}"
        )));
    }

    log::debug!("linked: {from} → {to}");
    Ok(())
}

fn wire_plugins(from: &str, to: &str) -> Result<(), Error> {
    log::debug!("wiring plugins: {from} → {to}");

    let out_ports = get_ports(from, PortDirection::Output)?;
    let in_ports = get_ports(to, PortDirection::Input)?;

    let audio_outs: Vec<_> = out_ports.iter().filter(|p| is_audio_port(p)).collect();
    let audio_ins: Vec<_> = in_ports.iter().filter(|p| is_audio_port(p)).collect();

    log::debug!("wire_plugins: {} outs × {} ins", audio_outs.len(), audio_ins.len());

    for (src, dst) in audio_outs.iter().zip(audio_ins.iter()) {
        pw_link(src, dst)?;
    }

    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum PortDirection {
    Output,
    Input,
}

fn is_audio_port(port: &str) -> bool {
    let lower = port.to_lowercase();
    !lower.contains("midi") && !lower.contains("event") && !lower.contains("control")
}

fn is_midi_port(port: &str) -> bool {
    let lower = port.to_lowercase();
    lower.contains("midi") || lower.contains("event")
}

fn has_midi_input(client: &str) -> bool {
    get_ports(client, PortDirection::Input)
        .map(|ports| ports.iter().any(|p| is_midi_port(p)))
        .unwrap_or(false)
}

fn wire_midi(router_client: &str, target_client: &str) -> Result<(), Error> {
    let out_ports = get_ports(router_client, PortDirection::Output)?;
    let in_ports = get_ports(target_client, PortDirection::Input)?;

    let midi_outs: Vec<_> = out_ports.iter().filter(|p| is_midi_port(p)).collect();
    let midi_ins: Vec<_> = in_ports.iter().filter(|p| is_midi_port(p)).collect();

    let router_port = midi_outs
        .iter()
        .find(|p| p.contains(target_client))
        .or(midi_outs.first());
    let target_port = midi_ins.first();

    if let (Some(out), Some(inp)) = (router_port, target_port) {
        pw_link(out, inp)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // --- is_audio_port / is_midi_port ---

    #[test]
    fn audio_port_detection() {
        assert!(is_audio_port("synth:audio_out_1"));
        assert!(is_audio_port("comp:out_l"));
        assert!(!is_audio_port("synth:midi_in"));
        assert!(!is_audio_port("synth:event-in"));
        assert!(!is_audio_port("synth:control_port"));
    }

    #[test]
    fn midi_port_detection() {
        assert!(is_midi_port("synth:midi_in"));
        assert!(is_midi_port("synth:MIDI_IN"));
        assert!(is_midi_port("synth:event-in"));
        assert!(!is_midi_port("synth:audio_out_1"));
    }

    // --- state_dir_for ---

    #[test]
    fn state_dir_ends_with_chain_name() {
        let dir = state_dir_for("mychain");
        assert!(dir.ends_with("broken_nest/mychain"));
    }

    // --- save / load controls round-trip ---

    #[test]
    fn controls_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut controls = HashMap::new();
        controls.insert("gain".to_string(), 0.75f32);
        controls.insert("freq".to_string(), 440.0f32);

        let path = dir.path().join("plugin.toml");
        save_controls_file(&path, &controls).unwrap();

        let loaded = load_controls(dir.path(), "plugin");
        assert_eq!(loaded.len(), 2);
        assert!((loaded["gain"] - 0.75).abs() < f32::EPSILON);
        assert!((loaded["freq"] - 440.0).abs() < f32::EPSILON);
    }

    #[test]
    fn load_controls_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_controls(dir.path(), "nonexistent");
        assert!(loaded.is_empty());
    }

    #[test]
    fn load_controls_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "not valid toml {{{{").unwrap();
        let loaded = load_controls(dir.path(), "bad");
        assert!(loaded.is_empty());
    }

    // --- pw_link error handling ---

    #[test]
    fn pw_link_rejects_empty_args() {
        let result = pw_link("", "");
        assert!(result.is_err());
    }
}
