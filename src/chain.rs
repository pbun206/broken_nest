use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

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

    /// Show the plugin UI window. Respawns as `jalv.gtk3` if currently headless.
    /// Brief audio gap (~1s) during respawn.
    pub fn show_ui(&mut self, plugin_name: &str) -> Result<(), Error> {
        let idx = self.find_plugin_idx(plugin_name)?;

        if self.instances[idx].ui_mode() == UiMode::Gtk && self.instances[idx].is_running() {
            return Ok(());
        }

        self.respawn_plugin(idx, UiMode::Gtk)
    }

    /// Hide the plugin UI window. Respawns as headless `jalv`.
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

    /// JACK port names for the first plugin's audio inputs.
    pub fn chain_input_ports(&self) -> Option<Vec<String>> {
        let name = self.instances.first()?.jack_client_name();
        let (jc, _) = jack::Client::new("bn-query", jack::ClientOptions::NO_START_SERVER).ok()?;
        let ports = jc.ports(
            Some(&format!("^{name}:")),
            None,
            jack::PortFlags::IS_INPUT,
        );
        if ports.is_empty() { None } else { Some(ports) }
    }

    /// JACK port names for the last plugin's audio outputs.
    pub fn chain_output_ports(&self) -> Option<Vec<String>> {
        let name = self.instances.last()?.jack_client_name();
        let (jc, _) = jack::Client::new("bn-query", jack::ClientOptions::NO_START_SERVER).ok()?;
        let ports = jc.ports(
            Some(&format!("^{name}:")),
            None,
            jack::PortFlags::IS_OUTPUT,
        );
        if ports.is_empty() { None } else { Some(ports) }
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

// --- Auto-connect ---

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
    let (jc, _) = jack::Client::new("bn-autoconnect", jack::ClientOptions::NO_START_SERVER)
        .map_err(|e| Error::Wiring(format!("JACK client for auto-connect failed: {e}")))?;

    log::debug!("auto-connect {chain_client} direction={direction:?}");

    match direction {
        PortDirection::Input => {
            let capture_ports = jc.ports(
                None,
                Some("32 bit float mono audio"),
                jack::PortFlags::IS_OUTPUT | jack::PortFlags::IS_PHYSICAL,
            );
            log::debug!("physical capture ports: {capture_ports:?}");
            let chain_ins = wait_for_client_ports(
                &jc, chain_client, None, jack::PortFlags::IS_INPUT,
            )?;
            log::debug!("chain input ports: {chain_ins:?}");
            try_link_pairs_jack(&jc, &capture_ports, &chain_ins)
        }
        PortDirection::Output => {
            let playback_ports = jc.ports(
                None,
                Some("32 bit float mono audio"),
                jack::PortFlags::IS_INPUT | jack::PortFlags::IS_PHYSICAL,
            );
            log::debug!("physical playback ports: {playback_ports:?}");
            let chain_outs = wait_for_client_ports(
                &jc, chain_client, None, jack::PortFlags::IS_OUTPUT,
            )?;
            log::debug!("chain output ports: {chain_outs:?}");
            try_link_pairs_jack(&jc, &chain_outs, &playback_ports)
        }
    }
}

fn try_link_pairs_jack(
    jc: &jack::Client,
    sources: &[String],
    destinations: &[String],
) -> Result<(), Error> {
    if sources.is_empty() || destinations.is_empty() {
        return Err(Error::Wiring(format!(
            "auto-connect found no linkable ports (sources: {}, destinations: {})",
            sources.len(),
            destinations.len(),
        )));
    }

    for (src, dst) in sources.iter().zip(destinations.iter()) {
        match jc.connect_ports_by_name(src, dst) {
            Ok(()) => {}
            Err(jack::Error::PortAlreadyConnected(_, _)) => {}
            Err(e) => {
                return Err(Error::Wiring(format!(
                    "jack connect {src} → {dst} failed: {e}"
                )));
            }
        }
    }

    Ok(())
}

// --- Port wiring ---

const PORT_WAIT_TIMEOUT_MS: u64 = 3000;
const PORT_WAIT_POLL_MS: u64 = 50;

fn wait_for_client_ports(
    jc: &jack::Client,
    client_name: &str,
    port_type: Option<&str>,
    flags: jack::PortFlags,
) -> Result<Vec<String>, Error> {
    let pattern = format!("^{client_name}:");
    let deadline = Instant::now() + Duration::from_millis(PORT_WAIT_TIMEOUT_MS);
    let type_label = port_type.unwrap_or("(any)");

    log::debug!("waiting for ports: client={client_name} type={type_label} flags={flags:?}");

    loop {
        let ports = jc.ports(Some(&pattern), port_type, flags);
        if !ports.is_empty() {
            log::debug!("found {} ports from {client_name}: {ports:?}", ports.len());
            return Ok(ports);
        }
        if Instant::now() >= deadline {
            let all_ports = jc.ports(Some(&pattern), None, jack::PortFlags::empty());
            log::error!(
                "timeout waiting for {type_label} ports from {client_name} \
                 (all ports with that name: {all_ports:?})"
            );
            return Err(Error::Wiring(format!(
                "timeout waiting for {type_label} ports from {client_name}",
            )));
        }
        thread::sleep(Duration::from_millis(PORT_WAIT_POLL_MS));
    }
}

fn wire_plugins(from: &str, to: &str) -> Result<(), Error> {
    log::debug!("wiring plugins: {from} → {to}");

    let (jc, _) = jack::Client::new("bn-wire-audio", jack::ClientOptions::NO_START_SERVER)
        .map_err(|e| Error::Wiring(format!("JACK client for wiring failed: {e}")))?;

    let audio_outs = wait_for_client_ports(
        &jc, from, None, jack::PortFlags::IS_OUTPUT,
    )?;
    let audio_ins = wait_for_client_ports(
        &jc, to, None, jack::PortFlags::IS_INPUT,
    )?;

    log::debug!("wire_plugins: {} outs × {} ins", audio_outs.len(), audio_ins.len());

    for (src, dst) in audio_outs.iter().zip(audio_ins.iter()) {
        match jc.connect_ports_by_name(src, dst) {
            Ok(()) => log::debug!("linked: {src} → {dst}"),
            Err(jack::Error::PortAlreadyConnected(_, _)) => {
                log::debug!("already linked: {src} → {dst}");
            }
            Err(e) => {
                return Err(Error::Wiring(format!("connect {src} → {dst} failed: {e}")));
            }
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum PortDirection {
    Output,
    Input,
}

fn is_midi_port(port: &str) -> bool {
    let lower = port.to_lowercase();
    lower.contains("midi") || lower.contains("event")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // --- is_midi_port ---

    #[test]
    fn midi_port_detection() {
        assert!(is_midi_port("synth:midi_in"));
        assert!(is_midi_port("synth:MIDI_IN"));
        assert!(is_midi_port("synth:event-in"));
        assert!(!is_midi_port("synth:audio_out_1"));
        assert!(!is_midi_port("synth:control"));
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

    // --- try_link_pairs_jack error on empty ---
    // Can't test success without JACK, but can test empty-port rejection

    #[test]
    fn try_link_empty_sources_errors() {
        // Need a JACK client — skip if JACK unavailable
        let client = jack::Client::new("bn-test", jack::ClientOptions::NO_START_SERVER);
        let Ok((jc, _)) = client else { return };
        let result = try_link_pairs_jack(&jc, &[], &["dest:port".into()]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("sources: 0"));
    }

    #[test]
    fn try_link_empty_destinations_errors() {
        let client = jack::Client::new("bn-test2", jack::ClientOptions::NO_START_SERVER);
        let Ok((jc, _)) = client else { return };
        let result = try_link_pairs_jack(&jc, &["src:port".into()], &[]);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("destinations: 0"));
    }
}

fn has_midi_input(client: &str) -> bool {
    let probe = jack::Client::new("bn-probe", jack::ClientOptions::NO_START_SERVER);
    let (jc, _) = match probe {
        Ok(c) => c,
        Err(_) => {
            log::warn!("JACK probe failed for MIDI detection on {client}");
            return false;
        }
    };
    let prefix = format!("{client}:");
    let midi_ins = jc.ports(None, Some("8 bit raw midi"), jack::PortFlags::IS_INPUT);
    midi_ins.iter().any(|p| p.starts_with(&prefix))
}

fn wire_midi(router_client: &str, target_client: &str) -> Result<(), Error> {
    let (jc, _) = jack::Client::new("bn-wire-midi", jack::ClientOptions::NO_START_SERVER)
        .map_err(|e| Error::Wiring(format!("JACK client for MIDI wiring failed: {e}")))?;

    let router_prefix = format!("{router_client}:");
    let target_prefix = format!("{target_client}:");

    let all_midi_outs = jc.ports(None, Some("8 bit raw midi"), jack::PortFlags::IS_OUTPUT);
    let all_midi_ins = jc.ports(None, Some("8 bit raw midi"), jack::PortFlags::IS_INPUT);

    let router_port = all_midi_outs
        .iter()
        .find(|p| p.starts_with(&router_prefix) && p.contains(target_client));
    let target_port = all_midi_ins
        .iter()
        .find(|p| p.starts_with(&target_prefix));

    if let (Some(out), Some(inp)) = (router_port, target_port) {
        match jc.connect_ports_by_name(out, inp) {
            Ok(()) => log::debug!("linked midi: {out} → {inp}"),
            Err(jack::Error::PortAlreadyConnected(_, _)) => {}
            Err(e) => {
                return Err(Error::Wiring(format!(
                    "midi connect {out} → {inp} failed: {e}"
                )));
            }
        }
    }

    Ok(())
}
