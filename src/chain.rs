use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::config::ChainConfig;
use crate::error::Error;
use crate::features::FeatureSet;
use crate::midi::{self, MidiEvent, MidiSender};
use crate::plugin::Lv2PluginInstance;
use crate::ui;

const MIDI_RING_CAPACITY: usize = 1024;

// ─── ControlBridge ──────────────────────────────────────────────────

struct PluginControlBridge {
    name: String,
    symbol_to_idx: HashMap<String, usize>,
    symbols: Vec<String>,
    values: Vec<AtomicU32>,
}

pub(crate) struct ControlBridge {
    plugins: Vec<PluginControlBridge>,
}

impl ControlBridge {
    fn new(plugins: &[Lv2PluginInstance]) -> Self {
        let bridge_plugins = plugins
            .iter()
            .map(|plugin| {
                let symbols = plugin.control_port_symbols();
                let controls = plugin.current_controls();
                let values: Vec<AtomicU32> = symbols
                    .iter()
                    .map(|sym| {
                        let val = controls.get(sym).copied().unwrap_or(0.0);
                        AtomicU32::new(val.to_bits())
                    })
                    .collect();
                let symbol_to_idx: HashMap<String, usize> = symbols
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (s.clone(), i))
                    .collect();
                PluginControlBridge {
                    name: plugin.name.clone(),
                    symbol_to_idx,
                    symbols,
                    values,
                }
            })
            .collect();
        Self {
            plugins: bridge_plugins,
        }
    }

    fn set_control(&self, plugin_name: &str, port: &str, value: f32) -> Result<(), Error> {
        let plugin = self
            .plugins
            .iter()
            .find(|p| p.name == plugin_name)
            .ok_or_else(|| Error::PluginNotFound {
                name: plugin_name.into(),
            })?;
        let idx = plugin.symbol_to_idx.get(port).ok_or_else(|| {
            Error::ControlPortNotFound {
                name: plugin_name.into(),
                port: port.into(),
            }
        })?;
        plugin.values[*idx].store(value.to_bits(), Ordering::Relaxed);
        Ok(())
    }

    fn all_controls(&self) -> HashMap<String, HashMap<String, f32>> {
        self.plugins
            .iter()
            .map(|p| {
                let controls: HashMap<String, f32> = p
                    .symbols
                    .iter()
                    .enumerate()
                    .map(|(i, sym)| {
                        (sym.clone(), f32::from_bits(p.values[i].load(Ordering::Relaxed)))
                    })
                    .collect();
                (p.name.clone(), controls)
            })
            .collect()
    }

    pub(crate) fn set_control_by_bridge_index(&self, plugin_name: &str, idx: usize, value: f32) {
        if let Some(atomic) = self
            .plugins
            .iter()
            .find(|p| p.name == plugin_name)
            .and_then(|p| p.values.get(idx))
        {
            atomic.store(value.to_bits(), Ordering::Relaxed);
        }
    }

    fn sync_to_plugins(&self, plugins: &mut [Lv2PluginInstance]) {
        for (bridge, plugin) in self.plugins.iter().zip(plugins.iter_mut()) {
            for (i, _) in bridge.values.iter().enumerate() {
                let value = f32::from_bits(bridge.values[i].load(Ordering::Relaxed));
                plugin.set_control_by_index(i, value);
            }
        }
    }
}

// ─── ChainProcessHandler ───────────────────────────────────────────

struct ChainProcessHandler {
    plugins: Vec<Lv2PluginInstance>,
    jack_audio_inputs: Vec<jack::Port<jack::AudioIn>>,
    jack_audio_outputs: Vec<jack::Port<jack::AudioOut>>,
    midi_consumers: Vec<(usize, rtrb::Consumer<MidiEvent>)>,
    control_bridge: Arc<ControlBridge>,
}

impl jack::ProcessHandler for ChainProcessHandler {
    fn process(&mut self, _client: &jack::Client, ps: &jack::ProcessScope) -> jack::Control {
        let nframes = ps.n_frames() as usize;

        // Sync control atomics → plugin values
        self.control_bridge.sync_to_plugins(&mut self.plugins);

        // Clear atom buffers
        for plugin in &mut self.plugins {
            plugin.clear_atom_buffers();
        }

        // Drain MIDI ring buffers → atom inputs
        for (plugin_idx, consumer) in &mut self.midi_consumers {
            let mut events: Vec<(usize, [u8; 3])> = Vec::new();
            while let Ok(event) = consumer.pop() {
                events.push(event.to_bytes());
            }
            if !events.is_empty() {
                let refs: Vec<(i64, &[u8])> = events
                    .iter()
                    .map(|(len, data)| (0i64, &data[..*len]))
                    .collect();
                self.plugins[*plugin_idx].write_midi_to_atom_in(&refs);
            }
        }

        // Copy JACK audio input → first plugin
        if let Some(first) = self.plugins.first_mut() {
            let in_bufs = first.audio_in_bufs_mut();
            for (i, jack_port) in self.jack_audio_inputs.iter().enumerate() {
                if let Some(buf) = in_bufs.get_mut(i) {
                    buf[..nframes].copy_from_slice(&jack_port.as_slice(ps)[..nframes]);
                }
            }
        }

        // Run plugins in series, copy audio between adjacent plugins
        for i in 0..self.plugins.len() {
            self.plugins[i].run(nframes as u32);

            if i + 1 < self.plugins.len() {
                let (left, right) = self.plugins.split_at_mut(i + 1);
                let src = left[i].audio_out_bufs();
                let dst = right[0].audio_in_bufs_mut();
                let pairs = src.len().min(dst.len());
                for j in 0..pairs {
                    dst[j][..nframes].copy_from_slice(&src[j][..nframes]);
                }
            }
        }

        // Copy last plugin audio output → JACK output
        if let Some(last) = self.plugins.last() {
            let out_bufs = last.audio_out_bufs();
            for (i, jack_port) in self.jack_audio_outputs.iter_mut().enumerate() {
                let jack_buf = jack_port.as_mut_slice(ps);
                if let Some(buf) = out_bufs.get(i) {
                    jack_buf[..nframes].copy_from_slice(&buf[..nframes]);
                } else {
                    jack_buf[..nframes].fill(0.0);
                }
            }
        }

        jack::Control::Continue
    }
}

// ─── Chain ──────────────────────────────────────────────────────────

/// A running chain of LV2 plugins wired in series.
///
/// All plugins run in-process within a single JACK client. Audio is
/// routed through memory buffers, so no external wiring is needed.
///
/// Drop or call [`Chain::stop`] to tear down the JACK client.
pub struct Chain {
    config: ChainConfig,
    active_client: Option<jack::AsyncClient<(), ChainProcessHandler>>,
    control_bridge: Arc<ControlBridge>,
    midi_senders: HashMap<String, MidiSender>,
    jack_input_port_names: Vec<String>,
    jack_output_port_names: Vec<String>,
    ui_infos: HashMap<String, ui::PluginUiInfo>,
    #[cfg(feature = "ui")]
    ui_thread: Option<ui::UiThread>,
    _features: Box<FeatureSet>,
    _world: lilv::World,
}

impl Chain {
    /// Instantiate all plugins, create a single JACK client, and start processing.
    ///
    /// If saved state exists, control values are restored automatically.
    pub fn start(config: &ChainConfig) -> Result<Self, Error> {
        if config.plugins.is_empty() {
            return Err(Error::Config("chain has no plugins".into()));
        }

        // JACK client
        let (client, _status) = jack::Client::new(
            &config.jack_client_prefix,
            jack::ClientOptions::NO_START_SERVER,
        )
        .map_err(|e| Error::Jack(format!("failed to create JACK client: {e}")))?;

        let sample_rate = client.sample_rate() as f64;
        let buffer_size = config.buffer_size.unwrap_or(client.buffer_size());

        // LV2 world + features
        let world = lilv::World::with_load_all();
        let features = Box::new(FeatureSet::new(sample_rate, buffer_size));

        let state_dir = state_dir_for(&config.name);

        // Instantiate plugins
        let mut plugins = Vec::with_capacity(config.plugins.len());
        for plugin_cfg in &config.plugins {
            let mut inst = Lv2PluginInstance::new(
                &world,
                &plugin_cfg.uri,
                &plugin_cfg.name,
                sample_rate,
                buffer_size,
                &features,
            )
            .map_err(|e| {
                log::error!("failed to instantiate '{}': {e}", plugin_cfg.name);
                e
            })?;

            // Apply saved state
            let saved = load_controls(&state_dir, &plugin_cfg.name);
            for (sym, val) in &saved {
                let _ = inst.set_control(sym, *val);
            }

            // Apply config overrides (take precedence over saved state)
            for (sym, val) in &plugin_cfg.controls {
                let _ = inst.set_control(sym, *val);
            }

            plugins.push(inst);
        }

        // MIDI setup
        let mut midi_senders = HashMap::new();
        let mut midi_consumers = Vec::new();

        for (i, (inst, plugin_cfg)) in plugins.iter().zip(&config.plugins).enumerate() {
            let wants_midi = plugin_cfg.midi_in.unwrap_or_else(|| inst.has_midi_in());
            if wants_midi {
                let (sender, port) = midi::create_midi_channel(MIDI_RING_CAPACITY);
                midi_senders.insert(inst.name.clone(), sender);
                midi_consumers.push((i, port.consumer));
            }
        }

        // Discover UIs
        let mut ui_infos = HashMap::new();
        for (inst, plugin_cfg) in plugins.iter().zip(&config.plugins) {
            let port_pairs = inst.all_port_symbol_index_pairs();
            let ctrl_indices = inst.control_in_port_indices();
            if let Some(info) = ui::discover_ui(
                &world,
                &plugin_cfg.uri,
                &inst.name,
                &port_pairs,
                &ctrl_indices,
            ) {
                log::debug!("found UI for '{}'", inst.name);
                ui_infos.insert(inst.name.clone(), info);
            }
        }

        // Control bridge
        let control_bridge = Arc::new(ControlBridge::new(&plugins));

        // Register JACK audio ports
        let first_in_count = plugins.first().map_or(0, |p| p.audio_in_count());
        let last_out_count = plugins.last().map_or(0, |p| p.audio_out_count());

        let mut jack_audio_inputs = Vec::with_capacity(first_in_count);
        let mut jack_input_port_names = Vec::with_capacity(first_in_count);
        for i in 0..first_in_count {
            let name = format!("audio_in_{}", i + 1);
            let port = client
                .register_port(&name, jack::AudioIn::default())
                .map_err(|e| Error::Jack(format!("register input port: {e}")))?;
            let full_name = format!("{}:{name}", client.name());
            jack_input_port_names.push(full_name);
            jack_audio_inputs.push(port);
        }

        let mut jack_audio_outputs = Vec::with_capacity(last_out_count);
        let mut jack_output_port_names = Vec::with_capacity(last_out_count);
        for i in 0..last_out_count {
            let name = format!("audio_out_{}", i + 1);
            let port = client
                .register_port(&name, jack::AudioOut::default())
                .map_err(|e| Error::Jack(format!("register output port: {e}")))?;
            let full_name = format!("{}:{name}", client.name());
            jack_output_port_names.push(full_name);
            jack_audio_outputs.push(port);
        }

        // Activate
        let handler = ChainProcessHandler {
            plugins,
            jack_audio_inputs,
            jack_audio_outputs,
            midi_consumers,
            control_bridge: control_bridge.clone(),
        };

        let active_client = client.activate_async((), handler).map_err(|e| {
            Error::Jack(format!("JACK activate failed: {e}"))
        })?;

        // Auto-connect
        if config.auto_connect_input {
            let physical = active_client.as_client().ports(
                None,
                None,
                jack::PortFlags::IS_PHYSICAL | jack::PortFlags::IS_OUTPUT,
            );
            for (src, dst) in physical.iter().zip(jack_input_port_names.iter()) {
                if let Err(e) = active_client.as_client().connect_ports_by_name(src, dst) {
                    log::warn!("auto-connect input {src} → {dst}: {e}");
                } else {
                    log::debug!("connected {src} → {dst}");
                }
            }
        }

        if config.auto_connect_output {
            let physical = active_client.as_client().ports(
                None,
                None,
                jack::PortFlags::IS_PHYSICAL | jack::PortFlags::IS_INPUT,
            );
            for (src, dst) in jack_output_port_names.iter().zip(physical.iter()) {
                if let Err(e) = active_client.as_client().connect_ports_by_name(src, dst) {
                    log::warn!("auto-connect output {src} → {dst}: {e}");
                } else {
                    log::debug!("connected {src} → {dst}");
                }
            }
        }

        Ok(Self {
            config: config.clone(),
            active_client: Some(active_client),
            control_bridge,
            midi_senders,
            jack_input_port_names,
            jack_output_port_names,
            ui_infos,
            #[cfg(feature = "ui")]
            ui_thread: None,
            _features: features,
            _world: world,
        })
    }

    /// Set a control port value on a running plugin.
    pub fn set_control(
        &mut self,
        plugin_name: &str,
        port: &str,
        value: f32,
    ) -> Result<(), Error> {
        self.control_bridge.set_control(plugin_name, port, value)
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

    /// Show the plugin UI window (requires `ui` feature, no-op without it).
    pub fn show_ui(&mut self, _plugin_name: &str) -> Result<(), Error> {
        #[cfg(feature = "ui")]
        {
            let info = self
                .ui_infos
                .remove(_plugin_name)
                .ok_or_else(|| Error::PluginNotFound {
                    name: _plugin_name.into(),
                })?;

            let ui_thread = self.ui_thread.get_or_insert_with(ui::UiThread::start);
            let features_ptr = self._features.as_feature_ptrs();
            ui_thread.show(info, self.control_bridge.clone(), features_ptr);
        }
        Ok(())
    }

    /// Hide the plugin UI window (requires `ui` feature, no-op without it).
    pub fn hide_ui(&mut self, _plugin_name: &str) -> Result<(), Error> {
        #[cfg(feature = "ui")]
        if let Some(ui_thread) = &self.ui_thread {
            ui_thread.hide(_plugin_name);
        }
        Ok(())
    }

    /// Show UI windows for all plugins that have UIs.
    pub fn show_all_ui(&mut self) -> Result<(), Error> {
        #[cfg(feature = "ui")]
        {
            let names: Vec<String> = self.ui_infos.keys().cloned().collect();
            for name in names {
                self.show_ui(&name)?;
            }
        }
        Ok(())
    }

    /// Hide UI windows for all plugins.
    pub fn hide_all_ui(&mut self) -> Result<(), Error> {
        #[cfg(feature = "ui")]
        {
            let names: Vec<String> = self
                .config
                .plugins
                .iter()
                .map(|p| p.name.clone())
                .collect();
            for name in names {
                self.hide_ui(&name)?;
            }
        }
        Ok(())
    }

    /// In-process plugins cannot die independently. This is a no-op.
    pub fn check_health(&mut self) {}

    /// Tear down the chain: save state, drop JACK client and plugins.
    pub fn stop(&mut self) {
        self.save_all_state();
        self.midi_senders.clear();
        drop(self.active_client.take());
    }

    /// Returns `true` if the JACK client is still active.
    pub fn is_running(&mut self) -> bool {
        self.active_client.is_some()
    }

    /// Port names for the chain's JACK audio inputs.
    pub fn chain_input_ports(&self) -> Option<Vec<String>> {
        if self.jack_input_port_names.is_empty() {
            None
        } else {
            Some(self.jack_input_port_names.clone())
        }
    }

    /// Port names for the chain's JACK audio outputs.
    pub fn chain_output_ports(&self) -> Option<Vec<String>> {
        if self.jack_output_port_names.is_empty() {
            None
        } else {
            Some(self.jack_output_port_names.clone())
        }
    }

    /// Snapshot of control values for all plugins.
    pub fn all_controls(&self) -> HashMap<String, HashMap<String, f32>> {
        self.control_bridge.all_controls()
    }

    fn save_all_state(&self) {
        let state_dir = state_dir_for(&self.config.name);

        if let Err(e) = fs::create_dir_all(&state_dir) {
            log::error!("failed to create state dir {}: {e}", state_dir.display());
            return;
        }

        let all = self.control_bridge.all_controls();
        for (name, controls) in &all {
            if controls.is_empty() {
                continue;
            }
            let path = state_dir.join(format!("{name}.toml"));
            if let Err(e) = save_controls_file(&path, controls) {
                log::error!("failed to save state for '{name}': {e}");
            } else {
                log::debug!("saved state for '{name}' ({} controls)", controls.len());
            }
        }
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        self.stop();
    }
}

// ─── State I/O ──────────────────────────────────────────────────────

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn state_dir_ends_with_chain_name() {
        let dir = state_dir_for("mychain");
        assert!(dir.ends_with("broken_nest/mychain"));
    }

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
}
