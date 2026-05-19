use std::collections::HashMap;
use std::env;
use std::ffi::c_void;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::config::ChainConfig;
use crate::error::Error;
use crate::features::FeatureSet;
use crate::midi::{self, MidiEvent, MidiSender};
use crate::plugin::Lv2PluginInstance;
use crate::ui;

const MIDI_RING_CAPACITY: usize = 1024;

pub(crate) struct AtomUiEvent {
    pub plugin_idx: usize,
    pub port_index: usize,
    pub protocol: u32,
    pub data: Vec<u8>,
}

pub(crate) type AtomUiQueue = Arc<Mutex<Vec<AtomUiEvent>>>;
const MAX_ROUTE_CHANNELS: usize = 2;
const MAX_BUF_FRAMES: usize = 8192;

// ─── ChainSlot ──────────────────────────────────────────────────────

#[derive(Clone)]
enum ChainSlot {
    Single(usize),
    DualMono { left: usize, right: usize },
}

impl ChainSlot {
    fn primary_idx(&self) -> usize {
        match self {
            ChainSlot::Single(i) => *i,
            ChainSlot::DualMono { left, .. } => *left,
        }
    }
}

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
    fn new(plugins: &[Lv2PluginInstance], primary_indices: &[usize]) -> Self {
        let bridge_plugins = primary_indices
            .iter()
            .map(|&i| {
                let plugin = &plugins[i];
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

    pub(crate) fn get_control_by_bridge_index(&self, plugin_name: &str, idx: usize) -> Option<f32> {
        self.plugins
            .iter()
            .find(|p| p.name == plugin_name)
            .and_then(|p| p.values.get(idx))
            .map(|v| f32::from_bits(v.load(Ordering::Relaxed)))
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

    fn sync_to_plugins(&self, plugins: &mut [Lv2PluginInstance], slots: &[ChainSlot]) {
        for (bridge, slot) in self.plugins.iter().zip(slots.iter()) {
            let indices: [Option<usize>; 2] = match slot {
                ChainSlot::Single(i) => [Some(*i), None],
                ChainSlot::DualMono { left, right } => [Some(*left), Some(*right)],
            };
            for idx in indices.iter().flatten() {
                for (i, val_atomic) in bridge.values.iter().enumerate() {
                    let value = f32::from_bits(val_atomic.load(Ordering::Relaxed));
                    plugins[*idx].set_control_by_index(i, value);
                }
            }
        }
    }
}

// ─── ChainProcessHandler ───────────────────────────────────────────

struct ChainProcessHandler {
    plugins: Vec<Lv2PluginInstance>,
    slots: Vec<ChainSlot>,
    slot_mixes: Vec<Option<Vec<[f32; 2]>>>,
    jack_audio_inputs: Vec<jack::Port<jack::AudioIn>>,
    jack_audio_outputs: Vec<jack::Port<jack::AudioOut>>,
    midi_consumers: Vec<(usize, rtrb::Consumer<MidiEvent>)>,
    control_bridge: Arc<ControlBridge>,
    muted: Arc<AtomicBool>,
    atom_ui_queue: AtomUiQueue,
    atom_ui_notify_queue: AtomUiQueue,
    route_buf: [Box<[f32]>; MAX_ROUTE_CHANNELS],
}

impl ChainProcessHandler {
    fn read_slot_output(&mut self, slot: &ChainSlot, slot_idx: usize, nframes: usize) {
        if let Some(mix) = &self.slot_mixes[slot_idx] {
            let out = self.plugins[slot.primary_idx()].audio_out_bufs();
            self.route_buf[0][..nframes].fill(0.0);
            self.route_buf[1][..nframes].fill(0.0);
            for (ch, buf) in out.iter().enumerate() {
                if let Some(&[l_gain, r_gain]) = mix.get(ch) {
                    for s in 0..nframes {
                        self.route_buf[0][s] += buf[s] * l_gain;
                        self.route_buf[1][s] += buf[s] * r_gain;
                    }
                }
            }
            return;
        }
        match slot {
            ChainSlot::Single(i) => {
                let out = self.plugins[*i].audio_out_bufs();
                for (ch, buf) in out.iter().enumerate().take(MAX_ROUTE_CHANNELS) {
                    self.route_buf[ch][..nframes].copy_from_slice(&buf[..nframes]);
                }
            }
            ChainSlot::DualMono { left, right } => {
                let out_l = self.plugins[*left].audio_out_bufs();
                if let Some(buf) = out_l.get(0) {
                    self.route_buf[0][..nframes].copy_from_slice(&buf[..nframes]);
                }
                let out_r = self.plugins[*right].audio_out_bufs();
                if let Some(buf) = out_r.get(0) {
                    self.route_buf[1][..nframes].copy_from_slice(&buf[..nframes]);
                }
            }
        }
    }

    fn write_slot_input(&mut self, slot: &ChainSlot, nframes: usize) {
        match slot {
            ChainSlot::Single(i) => {
                let in_bufs = self.plugins[*i].audio_in_bufs_mut();
                for (ch, buf) in in_bufs.iter_mut().enumerate().take(MAX_ROUTE_CHANNELS) {
                    buf[..nframes].copy_from_slice(&self.route_buf[ch][..nframes]);
                }
            }
            ChainSlot::DualMono { left, right } => {
                {
                    let in_l = self.plugins[*left].audio_in_bufs_mut();
                    if let Some(buf) = in_l.get_mut(0) {
                        buf[..nframes].copy_from_slice(&self.route_buf[0][..nframes]);
                    }
                }
                {
                    let in_r = self.plugins[*right].audio_in_bufs_mut();
                    if let Some(buf) = in_r.get_mut(0) {
                        buf[..nframes].copy_from_slice(&self.route_buf[1][..nframes]);
                    }
                }
            }
        }
    }

    fn run_slot(&mut self, slot: &ChainSlot, nframes: u32) {
        match slot {
            ChainSlot::Single(i) => self.plugins[*i].run(nframes),
            ChainSlot::DualMono { left, right } => {
                self.plugins[*left].run(nframes);
                self.plugins[*right].run(nframes);
            }
        }
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let c = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if c % 1000 == 0 {
            let idx = slot.primary_idx();
            let peak_out: f32 = self.plugins[idx].audio_out_bufs().get(0).map(|b| b[..nframes as usize].iter().map(|s| s.abs()).fold(0.0f32, f32::max)).unwrap_or(0.0);
            let name = &self.plugins[idx].name;
            log::debug!("[slot] '{name}' peak_out={peak_out:.6}");
        }
    }
}

impl jack::ProcessHandler for ChainProcessHandler {
    fn process(&mut self, _client: &jack::Client, ps: &jack::ProcessScope) -> jack::Control {
        let nframes = ps.n_frames() as usize;
        let slots = self.slots.clone();

        // Sync control atomics → plugin values
        self.control_bridge.sync_to_plugins(&mut self.plugins, &slots);

        // Clear atom buffers
        for plugin in &mut self.plugins {
            plugin.clear_atom_buffers();
        }

        // Drain atom UI events → atom inputs
        if let Ok(mut queue) = self.atom_ui_queue.try_lock() {
            for event in queue.drain(..) {
                log::debug!("[atom_ui_drain] plugin={} port={} proto={} len={} data={:?}", event.plugin_idx, event.port_index, event.protocol, event.data.len(), &event.data[..event.data.len().min(64)]);
                if event.plugin_idx < self.plugins.len() {
                    self.plugins[event.plugin_idx]
                        .write_atom_to_port(event.port_index, event.protocol, &event.data);
                }
            }
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

        // Copy JACK audio input → first slot
        let first_slot = &slots[0];
        match first_slot {
            ChainSlot::Single(idx) => {
                let in_bufs = self.plugins[*idx].audio_in_bufs_mut();
                for (i, jack_port) in self.jack_audio_inputs.iter().enumerate() {
                    if let Some(buf) = in_bufs.get_mut(i) {
                        buf[..nframes].copy_from_slice(&jack_port.as_slice(ps)[..nframes]);
                    }
                }
            }
            ChainSlot::DualMono { left, right } => {
                if let Some(jack_port) = self.jack_audio_inputs.get(0) {
                    let in_bufs = self.plugins[*left].audio_in_bufs_mut();
                    if let Some(buf) = in_bufs.get_mut(0) {
                        buf[..nframes].copy_from_slice(&jack_port.as_slice(ps)[..nframes]);
                    }
                }
                if let Some(jack_port) = self.jack_audio_inputs.get(1) {
                    let in_bufs = self.plugins[*right].audio_in_bufs_mut();
                    if let Some(buf) = in_bufs.get_mut(0) {
                        buf[..nframes].copy_from_slice(&jack_port.as_slice(ps)[..nframes]);
                    }
                }
            }
        }

        // Run slots in series, copy audio between slots via route_buf
        self.run_slot(&slots[0], nframes as u32);
        for s in 1..slots.len() {
            self.read_slot_output(&slots[s - 1], s - 1, nframes);
            self.write_slot_input(&slots[s], nframes);
            self.run_slot(&slots[s], nframes as u32);
        }

        // Forward plugin atom outputs → UI notify queue
        if let Ok(mut notify) = self.atom_ui_notify_queue.try_lock() {
            for (plugin_idx, plugin) in self.plugins.iter().enumerate() {
                for (port_index, data) in plugin.drain_atom_output_events() {
                    notify.push(AtomUiEvent {
                        plugin_idx,
                        port_index,
                        protocol: 6,
                        data,
                    });
                }
            }
        }

        // Copy last slot output → JACK output
        let last_idx = slots.len() - 1;
        let last_slot = slots.last().unwrap();
        if let Some(mix) = &self.slot_mixes[last_idx] {
            let out = self.plugins[last_slot.primary_idx()].audio_out_bufs();
            for jack_port in self.jack_audio_outputs.iter_mut() {
                jack_port.as_mut_slice(ps)[..nframes].fill(0.0);
            }
            for (ch, buf) in out.iter().enumerate() {
                if let Some(&[l_gain, r_gain]) = mix.get(ch) {
                    if let Some(jp) = self.jack_audio_outputs.get_mut(0) {
                        let jb = jp.as_mut_slice(ps);
                        for s in 0..nframes { jb[s] += buf[s] * l_gain; }
                    }
                    if let Some(jp) = self.jack_audio_outputs.get_mut(1) {
                        let jb = jp.as_mut_slice(ps);
                        for s in 0..nframes { jb[s] += buf[s] * r_gain; }
                    }
                }
            }
        } else {
        match last_slot {
            ChainSlot::Single(idx) => {
                let out_bufs = self.plugins[*idx].audio_out_bufs();
                for (i, jack_port) in self.jack_audio_outputs.iter_mut().enumerate() {
                    let jack_buf = jack_port.as_mut_slice(ps);
                    if let Some(buf) = out_bufs.get(i) {
                        jack_buf[..nframes].copy_from_slice(&buf[..nframes]);
                    } else {
                        jack_buf[..nframes].fill(0.0);
                    }
                }
            }
            ChainSlot::DualMono { left, right } => {
                if let Some(jack_port) = self.jack_audio_outputs.get_mut(0) {
                    let jack_buf = jack_port.as_mut_slice(ps);
                    let out = self.plugins[*left].audio_out_bufs();
                    if let Some(buf) = out.get(0) {
                        jack_buf[..nframes].copy_from_slice(&buf[..nframes]);
                    } else {
                        jack_buf[..nframes].fill(0.0);
                    }
                }
                if let Some(jack_port) = self.jack_audio_outputs.get_mut(1) {
                    let jack_buf = jack_port.as_mut_slice(ps);
                    let out = self.plugins[*right].audio_out_bufs();
                    if let Some(buf) = out.get(0) {
                        jack_buf[..nframes].copy_from_slice(&buf[..nframes]);
                    } else {
                        jack_buf[..nframes].fill(0.0);
                    }
                }
            }
        }
        }

        if self.muted.load(Ordering::Relaxed) {
            for jack_port in self.jack_audio_outputs.iter_mut() {
                jack_port.as_mut_slice(ps)[..nframes].fill(0.0);
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
    muted: Arc<AtomicBool>,
    midi_senders: HashMap<String, MidiSender>,
    atom_ui_queue: AtomUiQueue,
    atom_ui_notify_queue: AtomUiQueue,
    jack_input_port_names: Vec<String>,
    jack_output_port_names: Vec<String>,
    ui_infos: HashMap<String, ui::PluginUiInfo>,
    generic_ui_infos: HashMap<String, ui::GenericUiInfo>,
    ui_shown: Vec<String>,
    plugin_primary_indices: HashMap<String, usize>,
    instance_handles: HashMap<String, *mut c_void>,
    _features: Box<FeatureSet>,
    _world: Option<lilv::World>,
}

impl Chain {
    /// Instantiate all plugins, create a single JACK client, and start processing.
    ///
    /// If saved state exists, control values are restored automatically.
    pub fn start(config: &ChainConfig) -> Result<Self, Error> {
        let world = lilv::World::with_load_all();
        let mut chain = Self::start_with_world(config, &world)?;
        chain._world = Some(world);
        Ok(chain)
    }

    pub fn start_with_world(config: &ChainConfig, world: &lilv::World) -> Result<Self, Error> {
        if config.plugins.is_empty() {
            return Err(Error::Config("chain has no plugins".into()));
        }

        #[cfg(feature = "ui")]
        {
            use std::sync::Once;
            static X11_INIT: Once = Once::new();
            X11_INIT.call_once(|| unsafe {
                unsafe extern "C" {
                    fn XInitThreads() -> std::ffi::c_int;
                }
                XInitThreads();
            });
        }

        // JACK client
        let (client, _status) = jack::Client::new(
            &config.jack_client_prefix,
            jack::ClientOptions::NO_START_SERVER,
        )
        .map_err(|e| Error::Jack(format!("failed to create JACK client: {e}")))?;

        let sample_rate = client.sample_rate() as f64;
        let buffer_size = config.buffer_size.unwrap_or(client.buffer_size());

        let features = Box::new(FeatureSet::new(sample_rate, buffer_size));

        let state_dir = state_dir_for(&config.name);

        // Instantiate plugins and build slots
        let mut plugins: Vec<Lv2PluginInstance> = Vec::new();
        let mut slots: Vec<ChainSlot> = Vec::new();
        let mut slot_mixes: Vec<Option<Vec<[f32; 2]>>> = Vec::new();
        let mut primary_indices: Vec<usize> = Vec::new();

        for plugin_cfg in &config.plugins {
            let mut inst = Lv2PluginInstance::new(
                &world,
                &plugin_cfg.uri,
                &plugin_cfg.name,
                sample_rate,
                buffer_size,
                &features,
                Some(&state_dir),
            )
            .map_err(|e| {
                log::error!("failed to instantiate '{}': {e}", plugin_cfg.name);
                e
            })?;

            let saved = load_controls(&state_dir, &plugin_cfg.name);
            for (sym, val) in &saved {
                let _ = inst.set_control(sym, *val);
            }

            if plugin_cfg.dual_mono {
                let idx_l = plugins.len();
                plugins.push(inst);

                let right_name = format!("{}_R", plugin_cfg.name);
                let mut inst_r = Lv2PluginInstance::new(
                    &world,
                    &plugin_cfg.uri,
                    &right_name,
                    sample_rate,
                    buffer_size,
                    &features,
                    Some(&state_dir),
                )
                .map_err(|e| {
                    log::error!("failed to instantiate '{}': {e}", right_name);
                    e
                })?;

                for (sym, val) in &saved {
                    let _ = inst_r.set_control(sym, *val);
                }

                let idx_r = plugins.len();
                plugins.push(inst_r);

                slots.push(ChainSlot::DualMono { left: idx_l, right: idx_r });
                slot_mixes.push(plugin_cfg.stereo_mix.clone());
                primary_indices.push(idx_l);
                log::debug!("dual-mono slot for '{}' (L={}, R={})", plugin_cfg.name, idx_l, idx_r);
            } else {
                let idx = plugins.len();
                plugins.push(inst);
                slots.push(ChainSlot::Single(idx));
                slot_mixes.push(plugin_cfg.stereo_mix.clone());
                primary_indices.push(idx);
            }
        }

        // MIDI setup (primary instances only)
        let mut midi_senders = HashMap::new();
        let mut midi_consumers = Vec::new();

        for (slot, plugin_cfg) in slots.iter().zip(&config.plugins) {
            let primary = slot.primary_idx();
            let wants_midi = plugin_cfg.midi_in.unwrap_or_else(|| plugins[primary].has_midi_in());
            if wants_midi {
                let (sender, port) = midi::create_midi_channel(MIDI_RING_CAPACITY);
                midi_senders.insert(plugin_cfg.name.clone(), sender);
                midi_consumers.push((primary, port.consumer));
            }
        }

        // Discover UIs (primary instances only)
        let mut ui_infos = HashMap::new();
        let mut generic_ui_infos = HashMap::new();
        for (slot, plugin_cfg) in slots.iter().zip(&config.plugins) {
            let primary = slot.primary_idx();
            let inst = &plugins[primary];
            let ui_key = format!("{}/{}", config.name, plugin_cfg.name);
            if plugin_cfg.generic_ui {
                let meta = inst.control_port_meta().to_vec();
                let port_pairs = inst.all_port_symbol_index_pairs();
                let ctrl_indices = inst.control_in_port_indices();
                let sym_map: HashMap<String, u32> =
                    port_pairs.into_iter().collect();
                generic_ui_infos.insert(
                    plugin_cfg.name.clone(),
                    ui::GenericUiInfo {
                        plugin_name: ui_key.clone(),
                        bridge_name: plugin_cfg.name.clone(),
                        control_ports: meta,
                        port_symbol_to_index: sym_map,
                        control_port_indices: ctrl_indices,
                    },
                );
                log::debug!("generic UI prepared for '{}'", ui_key);
            } else {
                let port_pairs = inst.all_port_symbol_index_pairs();
                let ctrl_indices = inst.control_in_port_indices();
                if let Some(info) = ui::discover_ui(
                    &world,
                    &plugin_cfg.uri,
                    &ui_key,
                    &port_pairs,
                    &ctrl_indices,
                ) {
                    log::debug!("found native UI for '{}'", ui_key);
                    ui_infos.insert(plugin_cfg.name.clone(), info);
                }
            }
        }

        // Plugin name → primary index map
        let plugin_primary_indices: HashMap<String, usize> = slots
            .iter()
            .zip(&config.plugins)
            .map(|(slot, cfg)| (cfg.name.clone(), slot.primary_idx()))
            .collect();

        // Control bridge (primary instances only)
        let control_bridge = Arc::new(ControlBridge::new(&plugins, &primary_indices));

        // Register JACK audio ports
        let first_in_count = match &slots[0] {
            ChainSlot::Single(i) => plugins[*i].audio_in_count(),
            ChainSlot::DualMono { .. } => 2,
        };
        let last_out_count = if slot_mixes.last().map_or(false, |m| m.is_some()) {
            2
        } else {
            match slots.last().unwrap() {
                ChainSlot::Single(i) => plugins[*i].audio_out_count(),
                ChainSlot::DualMono { .. } => 2,
            }
        };

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

        // Capture instance handles (primary instances only)
        let instance_handles: HashMap<String, *mut c_void> = primary_indices
            .iter()
            .map(|&i| (plugins[i].name.clone(), plugins[i].instance_handle()))
            .collect();

        // Activate
        let route_buf = [
            vec![0.0f32; MAX_BUF_FRAMES].into_boxed_slice(),
            vec![0.0f32; MAX_BUF_FRAMES].into_boxed_slice(),
        ];

        let muted = Arc::new(AtomicBool::new(false));
        let atom_ui_queue: AtomUiQueue = Arc::new(Mutex::new(Vec::new()));
        let atom_ui_notify_queue: AtomUiQueue = Arc::new(Mutex::new(Vec::new()));

        let handler = ChainProcessHandler {
            plugins,
            slots,
            slot_mixes,
            jack_audio_inputs,
            jack_audio_outputs,
            midi_consumers,
            control_bridge: control_bridge.clone(),
            muted: muted.clone(),
            atom_ui_queue: atom_ui_queue.clone(),
            atom_ui_notify_queue: atom_ui_notify_queue.clone(),
            route_buf,
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
            muted,
            midi_senders,
            atom_ui_queue,
            atom_ui_notify_queue,
            plugin_primary_indices,
            jack_input_port_names,
            jack_output_port_names,
            ui_infos,
            generic_ui_infos,
            ui_shown: Vec::new(),
            instance_handles,
            _features: features,
            _world: None,
        })
    }

    pub fn toggle_mute(&self) -> bool {
        let was = self.muted.fetch_xor(true, Ordering::Relaxed);
        !was
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    pub fn output_port_names(&self) -> &[String] {
        &self.jack_output_port_names
    }

    pub fn input_port_names(&self) -> &[String] {
        &self.jack_input_port_names
    }

    pub fn connect_to(&self, target: &Chain) -> Result<(), Error> {
        let client = self
            .active_client
            .as_ref()
            .ok_or_else(|| Error::Config("chain not active".into()))?;
        for (src, dst) in self
            .jack_output_port_names
            .iter()
            .zip(target.jack_input_port_names.iter())
        {
            client
                .as_client()
                .connect_ports_by_name(src, dst)
                .map_err(|e| Error::Config(format!("connect {src} → {dst}: {e}")))?;
        }
        Ok(())
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
            let ui_key = format!("{}/{}", self.config.name, _plugin_name);
            if self.ui_shown.iter().any(|n| n == &ui_key) {
                ui::ui_toggle(&ui_key);
                return Ok(());
            }

            if let Some(info) = self.generic_ui_infos.remove(_plugin_name) {
                ui::ui_show_generic(info, self.control_bridge.clone());
                self.ui_shown.push(ui_key);
                return Ok(());
            }

            let info = self
                .ui_infos
                .remove(_plugin_name)
                .ok_or_else(|| Error::PluginNotFound {
                    name: _plugin_name.into(),
                })?;

            let instance_handle = self.instance_handles.get(_plugin_name).copied();
            let features_ptr = self._features.as_feature_ptrs();
            let plugin_idx = self.plugin_primary_indices.get(_plugin_name).copied().unwrap_or(0);
            ui::ui_show(
                info,
                self.control_bridge.clone(),
                features_ptr,
                instance_handle,
                self.atom_ui_queue.clone(),
                self.atom_ui_notify_queue.clone(),
                plugin_idx,
            );
            self.ui_shown.push(ui_key);
        }
        Ok(())
    }

    /// Hide the plugin UI window (requires `ui` feature, no-op without it).
    pub fn hide_ui(&mut self, _plugin_name: &str) -> Result<(), Error> {
        #[cfg(feature = "ui")]
        {
            let ui_key = format!("{}/{}", self.config.name, _plugin_name);
            ui::ui_hide(&ui_key);
        }
        Ok(())
    }

    /// Show UI windows for all plugins that have UIs.
    pub fn show_all_ui(&mut self) -> Result<(), Error> {
        #[cfg(feature = "ui")]
        {
            let prefix = format!("{}/", self.config.name);
            let mut names: Vec<String> = self.ui_infos.keys().cloned().collect();
            for name in self.generic_ui_infos.keys() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            for name in &self.ui_shown {
                let plain = name.strip_prefix(&prefix).unwrap_or(name);
                if !names.iter().any(|n| n == plain) {
                    names.push(plain.to_owned());
                }
            }
            for name in names {
                let _ = self.show_ui(&name);
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

    /// Toggle UI windows for all plugins.
    pub fn toggle_all_ui(&mut self) -> Result<(), Error> {
        self.show_all_ui()
    }

    /// In-process plugins cannot die independently. This is a no-op.
    pub fn check_health(&mut self) {}

    /// Tear down the chain: save state, drop JACK client and plugins.
    pub fn stop(&mut self) {
        self.save_all_state();
        self.midi_senders.clear();

        if let Some(client) = self.active_client.take() {
            match client.deactivate() {
                Ok((_client, _, handler)) => {
                    let state_dir = state_dir_for(&self.config.name);
                    let features_ptr = self._features.as_feature_ptrs();
                    for plugin in &handler.plugins {
                        if let Some(iface) = plugin.state_interface() {
                            let iface = unsafe { &*iface };
                            crate::state::save_plugin_state(
                                iface,
                                plugin.instance_handle(),
                                &self._features.mapper,
                                features_ptr,
                                &state_dir,
                                &plugin.name,
                            );
                        }
                    }
                }
                Err(e) => {
                    log::error!("JACK deactivation failed: {e}");
                }
            }
        }
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
