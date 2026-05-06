use std::collections::HashMap;
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
/// left-to-right via `pw-link`. If any plugin exposes MIDI input, a dedicated
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

        let mut instances = Vec::with_capacity(config.plugins.len());

        for plugin in &config.plugins {
            let saved = load_controls(&config.state_dir, &plugin.name);
            let mode = if plugin.show_ui { UiMode::Gtk } else { UiMode::Headless };
            let instance = JalvInstance::spawn(plugin, config.buffer_size, mode, &saved)?;
            instances.push(instance);
        }

        thread::sleep(Duration::from_millis(500));

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

    /// JACK port names for the first plugin's stereo audio input.
    pub fn chain_input_ports(&self) -> Option<(String, String)> {
        self.instances.first().map(|i| {
            let name = i.jack_client_name();
            (format!("{name}:in_l"), format!("{name}:in_r"))
        })
    }

    /// JACK port names for the last plugin's stereo audio output.
    pub fn chain_output_ports(&self) -> Option<(String, String)> {
        self.instances.last().map(|i| {
            let name = i.jack_client_name();
            (format!("{name}:out_l"), format!("{name}:out_r"))
        })
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

        thread::sleep(Duration::from_millis(500));

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
            auto_connect_with_retry(
                self.instances[0].jack_client_name(),
                PortDirection::Input,
            )?;
        }
        if idx == self.instances.len() - 1 && self.config.auto_connect_output {
            auto_connect_with_retry(
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
        let state_dir = match &self.config.state_dir {
            Some(d) => d,
            None => return,
        };

        if let Err(e) = fs::create_dir_all(state_dir) {
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

fn save_controls_file(path: &Path, controls: &HashMap<String, f32>) -> Result<(), std::io::Error> {
    let state = ControlState {
        controls: controls.clone(),
    };
    let toml_str = toml::to_string_pretty(&state)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    fs::write(path, toml_str)
}

fn load_controls(state_dir: &Option<PathBuf>, plugin_name: &str) -> HashMap<String, f32> {
    let dir = match state_dir {
        Some(d) => d,
        None => return HashMap::new(),
    };
    let path = dir.join(format!("{plugin_name}.toml"));
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

const AUTO_CONNECT_RETRIES: u32 = 5;
const AUTO_CONNECT_RETRY_MS: u64 = 300;

fn auto_connect(instances: &[JalvInstance], config: &ChainConfig) -> Result<(), Error> {
    if config.auto_connect_input {
        if let Some(first) = instances.first() {
            auto_connect_with_retry(first.jack_client_name(), PortDirection::Input)?;
        }
    }

    if config.auto_connect_output {
        if let Some(last) = instances.last() {
            auto_connect_with_retry(last.jack_client_name(), PortDirection::Output)?;
        }
    }

    Ok(())
}

fn auto_connect_with_retry(chain_client: &str, direction: PortDirection) -> Result<(), Error> {
    let mut last_err = None;

    for attempt in 0..AUTO_CONNECT_RETRIES {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(AUTO_CONNECT_RETRY_MS));
        }

        let (jc, _) = jack::Client::new("bn-autoconnect", jack::ClientOptions::NO_START_SERVER)
            .map_err(|e| Error::Wiring(format!("JACK client for auto-connect failed: {e}")))?;

        match direction {
            PortDirection::Input => {
                let capture_ports = jc.ports(
                    None,
                    Some("32 bit float mono audio"),
                    jack::PortFlags::IS_OUTPUT | jack::PortFlags::IS_PHYSICAL,
                );
                let chain_ins = jc.ports(
                    Some(&format!("^{chain_client}:")),
                    Some("32 bit float mono audio"),
                    jack::PortFlags::IS_INPUT,
                );
                match try_link_pairs_jack(&jc, &capture_ports, &chain_ins) {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = Some(e),
                }
            }
            PortDirection::Output => {
                let playback_ports = jc.ports(
                    None,
                    Some("32 bit float mono audio"),
                    jack::PortFlags::IS_INPUT | jack::PortFlags::IS_PHYSICAL,
                );
                let chain_outs = jc.ports(
                    Some(&format!("^{chain_client}:")),
                    Some("32 bit float mono audio"),
                    jack::PortFlags::IS_OUTPUT,
                );
                match try_link_pairs_jack(&jc, &chain_outs, &playback_ports) {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = Some(e),
                }
            }
        }

        log::warn!(
            "auto-connect attempt {}/{AUTO_CONNECT_RETRIES} failed for {chain_client}, retrying",
            attempt + 1,
        );
    }

    Err(last_err.unwrap())
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

fn wire_plugins(from: &str, to: &str) -> Result<(), Error> {
    let out_ports = get_ports(from, PortDirection::Output)?;
    let in_ports = get_ports(to, PortDirection::Input)?;

    let audio_outs: Vec<_> = out_ports.iter().filter(|p| is_audio_port(p)).collect();
    let audio_ins: Vec<_> = in_ports.iter().filter(|p| is_audio_port(p)).collect();

    for (out, inp) in audio_outs.iter().zip(audio_ins.iter()) {
        pw_link(out, inp)?;
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum PortDirection {
    Output,
    Input,
}

fn get_ports(client: &str, direction: PortDirection) -> Result<Vec<String>, Error> {
    let flag = match direction {
        PortDirection::Output => "-o",
        PortDirection::Input => "-i",
    };

    let mut attempts = 0;
    loop {
        let output = Command::new("pw-link")
            .arg(flag)
            .output()
            .map_err(|e| Error::PwLink(format!("failed to run pw-link: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let ports: Vec<String> = stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with(client))
            .collect();

        if !ports.is_empty() || attempts >= 10 {
            return Ok(ports);
        }

        attempts += 1;
        thread::sleep(Duration::from_millis(200));
    }
}

fn pw_link(from: &str, to: &str) -> Result<(), Error> {
    let output = Command::new("pw-link")
        .arg(from)
        .arg(to)
        .output()
        .map_err(|e| Error::PwLink(format!("failed to run pw-link: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::PwLink(format!(
            "pw-link {from} → {to} failed: {stderr}"
        )));
    }

    log::debug!("linked: {from} → {to}");
    Ok(())
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
    let probe = jack::Client::new("bn-probe", jack::ClientOptions::NO_START_SERVER);
    let (jc, _) = match probe {
        Ok(c) => c,
        Err(_) => {
            log::warn!("JACK probe failed, falling back to port-name heuristic for {client}");
            return has_midi_input_heuristic(client);
        }
    };
    let prefix = format!("{client}:");
    let midi_ins = jc.ports(None, Some("8 bit raw midi"), jack::PortFlags::IS_INPUT);
    midi_ins.iter().any(|p| p.starts_with(&prefix))
}

fn has_midi_input_heuristic(client: &str) -> bool {
    get_ports(client, PortDirection::Input)
        .map(|ports| ports.iter().any(|p| is_midi_port(p)))
        .unwrap_or(false)
}

fn wire_midi(router_client: &str, target_client: &str) -> Result<(), Error> {
    let (jc, _) = jack::Client::new("bn-wire", jack::ClientOptions::NO_START_SERVER)
        .map_err(|e| Error::Wiring(format!("JACK probe for MIDI wiring failed: {e}")))?;

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
        pw_link(out, inp)?;
    }

    Ok(())
}
