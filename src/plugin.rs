use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::error::Error;
use crate::features::{self, FeatureSet, LV2WorkerSchedule, ATOM_SEQUENCE_URI, MIDI_EVENT_URI};

const MAX_BUF_FRAMES: usize = 8192;
const ATOM_BUF_SIZE: usize = 8192;

const LV2_AUDIO_PORT: &str = "http://lv2plug.in/ns/lv2core#AudioPort";
const LV2_CV_PORT: &str = "http://lv2plug.in/ns/lv2core#CVPort";
const LV2_CONTROL_PORT: &str = "http://lv2plug.in/ns/lv2core#ControlPort";
const LV2_INPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#InputPort";
const LV2_OUTPUT_PORT: &str = "http://lv2plug.in/ns/lv2core#OutputPort";
const LV2_ATOM_PORT: &str = "http://lv2plug.in/ns/ext/atom#AtomPort";
const LV2_WORKER_SCHEDULE: &str = "http://lv2plug.in/ns/ext/worker#schedule";
const LV2_WORKER_IFACE: &str = "http://lv2plug.in/ns/ext/worker#interface";

#[derive(Debug, Clone, Copy, PartialEq)]
enum PortKind {
    AudioIn,
    AudioOut,
    ControlIn,
    ControlOut,
    AtomIn,
    AtomOut,
}

#[derive(Debug)]
struct PortInfo {
    port_index: usize,
    kind: PortKind,
    symbol: String,
    buf_index: usize,
    has_midi: bool,
}

#[derive(Debug, Clone)]
pub struct ControlPortMeta {
    pub symbol: String,
    pub port_index: u32,
    pub min: f32,
    pub max: f32,
    pub default: f32,
}

#[repr(C)]
struct LV2WorkerInterface {
    work: Option<
        unsafe extern "C" fn(
            *mut c_void,
            unsafe extern "C" fn(*mut c_void, u32, *const c_void) -> u32,
            *mut c_void,
            u32,
            *const c_void,
        ) -> u32,
    >,
    work_response: Option<unsafe extern "C" fn(*mut c_void, u32, *const c_void) -> u32>,
    end_run: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
}

struct PluginWorker {
    _schedule_tx: Box<UnsafeCell<rtrb::Producer<u8>>>,
    response_rx: rtrb::Consumer<u8>,
    thread: Option<std::thread::JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
    work_response_fn: unsafe extern "C" fn(*mut c_void, u32, *const c_void) -> u32,
    end_run_fn: Option<unsafe extern "C" fn(*mut c_void) -> u32>,
    instance_handle: *mut c_void,
}

unsafe impl Send for PluginWorker {}

pub struct Lv2PluginInstance {
    pub name: String,
    pub uri: String,
    worker: Option<PluginWorker>,
    instance: lilv::instance::ActiveInstance,
    port_infos: Vec<PortInfo>,
    control_in_symbol_map: HashMap<String, usize>,
    audio_in_bufs: Vec<Box<[f32]>>,
    audio_out_bufs: Vec<Box<[f32]>>,
    control_in_vals: Box<[f32]>,
    control_out_vals: Box<[f32]>,
    atom_in_bufs: Vec<Box<[u8]>>,
    atom_out_bufs: Vec<Box<[u8]>>,
    _dummy_buf: Box<[u8]>,
    _worker_schedule: Option<Box<LV2WorkerSchedule>>,
    _worker_schedule_uri: Option<CString>,
    atom_sequence_urid: u32,
    midi_event_urid: u32,
    midi_in_buf_indices: Vec<usize>,
    instance_handle: *mut c_void,
    state_iface: Option<*const crate::state::LV2StateInterface>,
    control_port_meta: Vec<ControlPortMeta>,
}

unsafe impl Send for Lv2PluginInstance {}

impl Lv2PluginInstance {
    pub fn new(
        world: &lilv::World,
        plugin_uri: &str,
        name: &str,
        sample_rate: f64,
        buffer_size: u32,
        feature_set: &FeatureSet,
        state_dir: Option<&std::path::Path>,
    ) -> Result<Self, Error> {
        let uri_node = world.new_uri(plugin_uri);
        let plugin = world
            .plugins()
            .plugin(&uri_node)
            .ok_or_else(|| Error::Lv2PluginNotFound {
                uri: plugin_uri.to_string(),
            })?;

        let audio_class = world.new_uri(LV2_AUDIO_PORT);
        let cv_class = world.new_uri(LV2_CV_PORT);
        let control_class = world.new_uri(LV2_CONTROL_PORT);
        let input_class = world.new_uri(LV2_INPUT_PORT);
        let output_class = world.new_uri(LV2_OUTPUT_PORT);
        let atom_class = world.new_uri(LV2_ATOM_PORT);
        let midi_event_node = world.new_uri("http://lv2plug.in/ns/ext/midi#MidiEvent");

        let mut port_infos = Vec::new();
        let mut unknown_port_indices = Vec::new();
        let mut audio_in_count = 0usize;
        let mut audio_out_count = 0usize;
        let mut ctrl_in_count = 0usize;
        let mut ctrl_out_count = 0usize;
        let mut atom_in_count = 0usize;
        let mut atom_out_count = 0usize;

        let ranges = plugin.port_ranges_float();

        for port in plugin.iter_ports() {
            let idx = port.index();
            let symbol = port
                .symbol()
                .and_then(|n| n.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| format!("port_{idx}"));

            let is_input = port.is_a(&input_class);
            let is_output = port.is_a(&output_class);
            let is_audio = port.is_a(&audio_class) || port.is_a(&cv_class);
            let is_control = port.is_a(&control_class);
            let is_atom = port.is_a(&atom_class);

            let classified = if is_audio && is_input {
                let bi = audio_in_count;
                audio_in_count += 1;
                Some((PortKind::AudioIn, bi, false))
            } else if is_audio && is_output {
                let bi = audio_out_count;
                audio_out_count += 1;
                Some((PortKind::AudioOut, bi, false))
            } else if is_control && is_input {
                let bi = ctrl_in_count;
                ctrl_in_count += 1;
                Some((PortKind::ControlIn, bi, false))
            } else if is_control && is_output {
                let bi = ctrl_out_count;
                ctrl_out_count += 1;
                Some((PortKind::ControlOut, bi, false))
            } else if is_atom && is_input {
                let bi = atom_in_count;
                atom_in_count += 1;
                Some((PortKind::AtomIn, bi, port.supports_event(&midi_event_node)))
            } else if is_atom && is_output {
                let bi = atom_out_count;
                atom_out_count += 1;
                Some((PortKind::AtomOut, bi, port.supports_event(&midi_event_node)))
            } else {
                None
            };

            match classified {
                Some((kind, buf_index, has_midi)) => {
                    port_infos.push(PortInfo {
                        port_index: idx,
                        kind,
                        symbol,
                        buf_index,
                        has_midi,
                    });
                }
                None => {
                    log::debug!(
                        "plugin '{name}': port {idx} ({symbol}) has unknown type, connecting to dummy"
                    );
                    unknown_port_indices.push(idx);
                }
            }
        }

        let buf_frames = (buffer_size as usize).max(MAX_BUF_FRAMES);

        let audio_in_bufs: Vec<Box<[f32]>> = (0..audio_in_count)
            .map(|_| vec![0.0f32; buf_frames].into_boxed_slice())
            .collect();
        let audio_out_bufs: Vec<Box<[f32]>> = (0..audio_out_count)
            .map(|_| vec![0.0f32; buf_frames].into_boxed_slice())
            .collect();

        let mut ctrl_in_defaults = vec![0.0f32; ctrl_in_count];
        let mut ctrl_out_defaults = vec![0.0f32; ctrl_out_count];
        for info in &port_infos {
            if info.port_index < ranges.len() {
                let default = ranges[info.port_index].default;
                if default.is_finite() {
                    match info.kind {
                        PortKind::ControlIn => ctrl_in_defaults[info.buf_index] = default,
                        PortKind::ControlOut => ctrl_out_defaults[info.buf_index] = default,
                        _ => {}
                    }
                }
            }
        }
        let control_in_vals: Box<[f32]> = ctrl_in_defaults.into_boxed_slice();
        let control_out_vals: Box<[f32]> = ctrl_out_defaults.into_boxed_slice();

        let atom_in_bufs: Vec<Box<[u8]>> = (0..atom_in_count)
            .map(|_| vec![0u8; ATOM_BUF_SIZE].into_boxed_slice())
            .collect();
        let atom_out_bufs: Vec<Box<[u8]>> = (0..atom_out_count)
            .map(|_| vec![0u8; ATOM_BUF_SIZE].into_boxed_slice())
            .collect();

        let dummy_buf: Box<[u8]> = vec![0u8; buf_frames * 4].into_boxed_slice();

        let mut control_in_symbol_map = HashMap::new();
        for info in &port_infos {
            if info.kind == PortKind::ControlIn {
                control_in_symbol_map.insert(info.symbol.clone(), info.buf_index);
            }
        }

        let midi_in_buf_indices: Vec<usize> = port_infos
            .iter()
            .filter(|p| p.kind == PortKind::AtomIn && p.has_midi)
            .map(|p| p.buf_index)
            .collect();

        let atom_sequence_urid = feature_set.mapper.lookup(ATOM_SEQUENCE_URI).unwrap_or(0);
        let midi_event_urid = feature_set.mapper.lookup(MIDI_EVENT_URI).unwrap_or(0);

        // Check if plugin needs worker
        let needs_worker = plugin
            .supported_features()
            .iter()
            .any(|f| f.as_uri() == Some(LV2_WORKER_SCHEDULE));

        let mut worker_schedule: Option<Box<LV2WorkerSchedule>> = None;
        let mut worker_schedule_uri: Option<CString> = None;
        let mut schedule_tx_box: Option<Box<UnsafeCell<rtrb::Producer<u8>>>> = None;
        let mut rt_to_worker_rx: Option<rtrb::Consumer<u8>> = None;
        let mut worker_to_rt_tx: Option<rtrb::Producer<u8>> = None;
        let mut worker_to_rt_rx: Option<rtrb::Consumer<u8>> = None;

        if needs_worker {
            let (tx, rx) = rtrb::RingBuffer::new(65536);
            let (resp_tx, resp_rx) = rtrb::RingBuffer::new(65536);

            let tx_box = Box::new(UnsafeCell::new(tx));
            let handle = tx_box.get() as *mut c_void;

            worker_schedule = Some(Box::new(LV2WorkerSchedule {
                handle,
                schedule_work: schedule_work_callback,
            }));
            worker_schedule_uri =
                Some(CString::new(LV2_WORKER_SCHEDULE).unwrap());
            schedule_tx_box = Some(tx_box);
            rt_to_worker_rx = Some(rx);
            worker_to_rt_tx = Some(resp_tx);
            worker_to_rt_rx = Some(resp_rx);
        }

        // Build feature list: base features + optional worker schedule
        let base_features = feature_set.as_features();
        let worker_lv2_feature;
        let all_features: Vec<&lv2_raw::LV2Feature>;

        if let (Some(sched), Some(uri)) = (&worker_schedule, &worker_schedule_uri) {
            worker_lv2_feature = Some(lv2_raw::LV2Feature {
                uri: uri.as_ptr(),
                data: &**sched as *const LV2WorkerSchedule as *mut c_void,
            });
            let mut feats = base_features;
            feats.push(worker_lv2_feature.as_ref().unwrap());
            all_features = feats;
        } else {
            all_features = base_features;
        }

        // Instantiate
        let mut instance = unsafe { plugin.instantiate(sample_rate, all_features) }.ok_or_else(
            || Error::Lv2Instantiation {
                name: name.to_string(),
                uri: plugin_uri.to_string(),
                reason: "lilv_plugin_instantiate returned null".into(),
            },
        )?;

        // Get worker interface before activation
        let worker = if needs_worker {
            let iface =
                unsafe { instance.extension_data::<LV2WorkerInterface>(LV2_WORKER_IFACE) };

            if let Some(iface_ptr) = iface {
                let iface = unsafe { iface_ptr.as_ref() };
                match (iface.work, iface.work_response) {
                    (Some(work_fn), Some(work_response_fn)) => {
                        let inst_handle = instance.handle();
                        let mut rx = rt_to_worker_rx.take().unwrap();
                        let resp_tx = worker_to_rt_tx.take().unwrap();
                        let shutdown = Arc::new(AtomicBool::new(false));
                        let shutdown_clone = shutdown.clone();

                        let handle_usize = inst_handle as usize;
                        let tx_mutex = std::sync::Mutex::new(resp_tx);

                        let thread = std::thread::spawn(move || {
                            let handle = handle_usize as *mut c_void;
                            while !shutdown_clone.load(Ordering::Relaxed) {
                                if let Some((size, data)) =
                                    features::read_framed_message(&mut rx)
                                {
                                    let tx_ref = &tx_mutex;
                                    let status = unsafe {
                                        work_fn(
                                            handle,
                                            features::worker_respond_callback,
                                            tx_ref as *const _ as *mut c_void,
                                            size,
                                            data.as_ptr() as *const c_void,
                                        )
                                    };
                                    if status != 0 {
                                        log::warn!("[worker] work() returned {status} for size={size}");
                                    }
                                } else {
                                    std::thread::sleep(std::time::Duration::from_millis(1));
                                }
                            }
                        });

                        Some(PluginWorker {
                            _schedule_tx: schedule_tx_box.take().unwrap(),
                            response_rx: worker_to_rt_rx.take().unwrap(),
                            thread: Some(thread),
                            shutdown,
                            work_response_fn,
                            end_run_fn: iface.end_run,
                            instance_handle: inst_handle,
                        })
                    }
                    _ => {
                        log::warn!("plugin '{name}' declares worker but has no work/work_response");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // Connect ports
        for info in &port_infos {
            unsafe {
                match info.kind {
                    PortKind::AudioIn => {
                        instance.connect_port_mut(
                            info.port_index,
                            audio_in_bufs[info.buf_index].as_ptr() as *mut f32,
                        );
                    }
                    PortKind::AudioOut => {
                        instance.connect_port_mut(
                            info.port_index,
                            audio_out_bufs[info.buf_index].as_ptr() as *mut f32,
                        );
                    }
                    PortKind::ControlIn => {
                        instance.connect_port(
                            info.port_index,
                            &control_in_vals[info.buf_index] as *const f32,
                        );
                    }
                    PortKind::ControlOut => {
                        instance.connect_port_mut(
                            info.port_index,
                            &control_out_vals[info.buf_index] as *const f32 as *mut f32,
                        );
                    }
                    PortKind::AtomIn | PortKind::AtomOut => {
                        let bufs = if info.kind == PortKind::AtomIn {
                            &atom_in_bufs
                        } else {
                            &atom_out_bufs
                        };
                        instance.connect_port_mut(
                            info.port_index,
                            bufs[info.buf_index].as_ptr() as *mut u8,
                        );
                    }
                }
            }
        }

        for &idx in &unknown_port_indices {
            unsafe {
                instance.connect_port_mut(idx, dummy_buf.as_ptr() as *mut u8);
            }
        }

        let state_iface = crate::state::get_state_interface(&instance);
        let instance_handle = instance.handle();

        let control_port_meta: Vec<ControlPortMeta> = port_infos
            .iter()
            .filter(|p| p.kind == PortKind::ControlIn)
            .map(|p| {
                let r = &ranges[p.port_index];
                ControlPortMeta {
                    symbol: p.symbol.clone(),
                    port_index: p.port_index as u32,
                    min: if r.min.is_finite() { r.min } else { 0.0 },
                    max: if r.max.is_finite() { r.max } else { 1.0 },
                    default: if r.default.is_finite() { r.default } else { 0.0 },
                }
            })
            .collect();

        let active_instance = unsafe { instance.activate() };

        if let (Some(iface_ptr), Some(dir)) = (state_iface, state_dir) {
            let iface = unsafe { &*iface_ptr };
            let features_ptr = feature_set.as_feature_ptrs();
            crate::state::restore_plugin_state(
                iface,
                instance_handle,
                &feature_set.mapper,
                features_ptr,
                dir,
                name,
            );
        }

        log::info!(
            "plugin '{name}' ({plugin_uri}): {audio_in_count} audio in, {audio_out_count} audio out, \
             {} ctrl in, {} ctrl out, {atom_in_count} atom in, {atom_out_count} atom out{}",
            ctrl_in_count,
            ctrl_out_count,
            if needs_worker { ", worker" } else { "" },
        );

        Ok(Self {
            name: name.to_string(),
            uri: plugin_uri.to_string(),
            instance: active_instance,
            port_infos,
            control_in_symbol_map,
            audio_in_bufs,
            audio_out_bufs,
            control_in_vals,
            control_out_vals,
            atom_in_bufs,
            atom_out_bufs,
            _dummy_buf: dummy_buf,
            worker,
            _worker_schedule: worker_schedule,
            _worker_schedule_uri: worker_schedule_uri,
            atom_sequence_urid,
            midi_event_urid,
            midi_in_buf_indices,
            instance_handle,
            state_iface,
            control_port_meta,
        })
    }

    pub fn audio_in_count(&self) -> usize {
        self.audio_in_bufs.len()
    }
    pub fn audio_out_count(&self) -> usize {
        self.audio_out_bufs.len()
    }
    pub fn has_midi_in(&self) -> bool {
        !self.midi_in_buf_indices.is_empty()
    }

    pub fn instance_handle(&self) -> *mut c_void {
        self.instance_handle
    }

    pub fn state_interface(&self) -> Option<*const crate::state::LV2StateInterface> {
        self.state_iface
    }

    pub fn audio_in_bufs_mut(&mut self) -> &mut [Box<[f32]>] {
        &mut self.audio_in_bufs
    }

    pub fn audio_out_bufs(&self) -> &[Box<[f32]>] {
        &self.audio_out_bufs
    }

    pub fn set_control(&mut self, symbol: &str, value: f32) -> Result<(), Error> {
        let idx = self
            .control_in_symbol_map
            .get(symbol)
            .ok_or_else(|| Error::ControlPortNotFound {
                name: self.name.clone(),
                port: symbol.to_string(),
            })?;
        self.control_in_vals[*idx] = value;
        Ok(())
    }

    pub fn current_controls(&self) -> HashMap<String, f32> {
        self.port_infos
            .iter()
            .filter(|p| p.kind == PortKind::ControlIn)
            .map(|p| (p.symbol.clone(), self.control_in_vals[p.buf_index]))
            .collect()
    }

    pub fn set_control_by_index(&mut self, idx: usize, value: f32) {
        if idx < self.control_in_vals.len() {
            self.control_in_vals[idx] = value;
        }
    }

    pub fn control_port_symbols(&self) -> Vec<String> {
        self.port_infos
            .iter()
            .filter(|p| p.kind == PortKind::ControlIn)
            .map(|p| p.symbol.clone())
            .collect()
    }

    pub fn all_port_symbol_index_pairs(&self) -> Vec<(String, u32)> {
        self.port_infos
            .iter()
            .map(|p| (p.symbol.clone(), p.port_index as u32))
            .collect()
    }

    pub fn control_port_meta(&self) -> &[ControlPortMeta] {
        &self.control_port_meta
    }

    pub fn control_in_port_indices(&self) -> Vec<u32> {
        self.port_infos
            .iter()
            .filter(|p| p.kind == PortKind::ControlIn)
            .map(|p| p.port_index as u32)
            .collect()
    }

    pub fn clear_atom_buffers(&mut self) {
        for buf in &mut self.atom_in_bufs {
            write_empty_atom_sequence(buf, self.atom_sequence_urid);
        }
        for buf in &mut self.atom_out_bufs {
            write_atom_out_capacity(buf, self.atom_sequence_urid);
        }
    }

    pub fn write_atom_to_port(&mut self, port_index: usize, protocol: u32, data: &[u8]) {
        for info in &self.port_infos {
            if info.port_index == port_index && info.kind == PortKind::AtomIn {
                let buf = &mut self.atom_in_bufs[info.buf_index];
                let seq_urid = self.atom_sequence_urid;
                append_atom_event(buf, seq_urid, protocol, data);
                return;
            }
        }
    }

    pub fn drain_atom_output_events(&self) -> Vec<(usize, Vec<u8>)> {
        let mut result = Vec::new();
        for info in &self.port_infos {
            if info.kind == PortKind::AtomOut {
                let buf = &self.atom_out_bufs[info.buf_index];
                if buf.len() < 16 {
                    continue;
                }
                let body_size = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if body_size <= 8 {
                    continue;
                }
                log::debug!("[atom_out] plugin='{}' port={} body_size={}", self.name, info.port_index, body_size);
                let mut pos = 16;
                while pos + 8 < 8 + body_size {
                    let _time = i64::from_ne_bytes(buf[pos..pos + 8].try_into().unwrap());
                    let atom_size = u32::from_ne_bytes(buf[pos + 8..pos + 12].try_into().unwrap()) as usize;
                    let atom_total = 8 + atom_size;
                    if pos + 8 + atom_total > buf.len() {
                        break;
                    }
                    result.push((info.port_index, buf[pos + 8..pos + 8 + atom_total].to_vec()));
                    pos += 8 + ((atom_total + 7) & !7);
                }
            }
        }
        result
    }

    pub fn write_midi_to_atom_in(&mut self, events: &[(i64, &[u8])]) {
        if events.is_empty() {
            return;
        }
        for &buf_idx in &self.midi_in_buf_indices {
            write_atom_sequence_midi(
                &mut self.atom_in_bufs[buf_idx],
                self.atom_sequence_urid,
                self.midi_event_urid,
                events,
            );
        }
    }

    pub fn run(&mut self, nframes: u32) {
        for info in &self.port_infos {
            if info.kind == PortKind::AtomIn {
                let buf = &self.atom_in_bufs[info.buf_index];
                let body_size = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
                if body_size > 8 {
                    log::debug!("[pre_run] plugin='{}' port={} atom_in body_size={}", self.name, info.port_index, body_size);
                }
            }
        }
        if self.name == "nam" {
            static RUN_LOG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let c = RUN_LOG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if c == 0 {
                log::debug!("[run] plugin='{}' instance_handle={:?}", self.name, self.instance_handle);
            }
            if c % 200 == 0 {
                // NAM::Plugin layout: Ports(48) + sampleRate(8) + map(8) + logger(32) + schedule(8) = offset 104
                let ptr = unsafe { (self.instance_handle as *const u8).add(104) };
                let model_ptr = unsafe { *(ptr as *const u64) };
                log::debug!("[nam_model] cycle={c} currentModel=0x{model_ptr:016X}");
            }
        }
        unsafe {
            self.instance.run(nframes as usize);
        }
        if let Some(ref mut worker) = self.worker {
            while let Some((size, data)) =
                features::read_framed_message(&mut worker.response_rx)
            {
                log::debug!("[work_response] plugin='{}' size={size} handle={:?} first_4={:?} last_8={:?}", self.name, worker.instance_handle, &data[..4.min(data.len())], &data[data.len().saturating_sub(8)..]);
                unsafe {
                    (worker.work_response_fn)(
                        worker.instance_handle,
                        size,
                        data.as_ptr() as *const c_void,
                    );
                }
                if self.name == "nam" {
                    let ptr = unsafe { (worker.instance_handle as *const u8).add(104) };
                    let model_ptr = unsafe { *(ptr as *const u64) };
                    log::debug!("[nam_after_work_response] currentModel=0x{model_ptr:016X}");
                }
            }
            if let Some(end_run) = worker.end_run_fn {
                unsafe {
                    end_run(worker.instance_handle);
                }
            }
        }
    }
}

impl Drop for PluginWorker {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

unsafe extern "C" fn schedule_work_callback(
    handle: *mut c_void,
    size: u32,
    data: *const c_void,
) -> u32 {
    unsafe {
        let tx_cell = &*(handle as *const UnsafeCell<rtrb::Producer<u8>>);
        let tx = &mut *tx_cell.get();
        let bytes = std::slice::from_raw_parts(data as *const u8, size as usize);
        log::debug!("[schedule_work] size={size}");
        if features::write_framed_message(tx, size, bytes) {
            0
        } else {
            log::warn!("[schedule_work] ring buffer full, dropped work");
            1
        }
    }
}

fn append_atom_event(buf: &mut [u8], sequence_urid: u32, _protocol: u32, atom_data: &[u8]) {
    if buf.len() < 16 || atom_data.len() < 8 {
        return;
    }
    // Read current sequence body size from atom header
    let body_size = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    // Events start after 16-byte header (8 atom + 8 seq body)
    let offset = 8 + body_size;
    // Event: 8 bytes time (0) + atom_data (which is LV2_Atom: size+type+body)
    let event_size = 8 + atom_data.len();
    let padded = (event_size + 7) & !7;
    if offset + padded > buf.len() {
        return;
    }
    // time.frames = 0
    buf[offset..offset + 8].copy_from_slice(&0i64.to_ne_bytes());
    // atom (size + type + body)
    buf[offset + 8..offset + 8 + atom_data.len()].copy_from_slice(atom_data);
    // pad
    let end = offset + 8 + atom_data.len();
    let aligned = (end + 7) & !7;
    if aligned > end {
        buf[end..aligned].fill(0);
    }
    // Update atom header size
    let new_body_size = (body_size + padded) as u32;
    buf[0..4].copy_from_slice(&new_body_size.to_ne_bytes());
    buf[4..8].copy_from_slice(&sequence_urid.to_ne_bytes());
}

fn write_empty_atom_sequence(buf: &mut [u8], sequence_urid: u32) {
    if buf.len() < 16 {
        return;
    }
    buf[0..4].copy_from_slice(&8u32.to_ne_bytes());
    buf[4..8].copy_from_slice(&sequence_urid.to_ne_bytes());
    buf[8..16].fill(0);
}

fn write_atom_out_capacity(buf: &mut [u8], sequence_urid: u32) {
    if buf.len() < 16 {
        return;
    }
    // LV2 spec: for output atom ports, atom.size = available buffer capacity (excluding atom header)
    let capacity = (buf.len() - 8) as u32;
    buf[0..4].copy_from_slice(&capacity.to_ne_bytes());
    buf[4..8].copy_from_slice(&sequence_urid.to_ne_bytes());
    buf[8..16].fill(0);
}

fn write_atom_sequence_midi(
    buf: &mut [u8],
    sequence_urid: u32,
    midi_event_urid: u32,
    events: &[(i64, &[u8])],
) {
    let mut offset = 8usize; // skip atom header, fill later

    // Sequence body: unit=0 (frames), pad=0
    buf[offset..offset + 8].fill(0);
    offset += 8;

    for &(frame, midi_bytes) in events {
        let event_total = 16 + midi_bytes.len();
        let padded = (event_total + 7) & !7;
        if offset + padded > buf.len() {
            break;
        }
        // time.frames (i64 LE/NE)
        buf[offset..offset + 8].copy_from_slice(&frame.to_ne_bytes());
        offset += 8;
        // body.size
        buf[offset..offset + 4].copy_from_slice(&(midi_bytes.len() as u32).to_ne_bytes());
        offset += 4;
        // body.type
        buf[offset..offset + 4].copy_from_slice(&midi_event_urid.to_ne_bytes());
        offset += 4;
        // MIDI data
        buf[offset..offset + midi_bytes.len()].copy_from_slice(midi_bytes);
        offset += midi_bytes.len();
        // Pad to 8-byte alignment
        let aligned = (offset + 7) & !7;
        if aligned > offset {
            buf[offset..aligned].fill(0);
        }
        offset = aligned;
    }

    // Fill atom header: size = everything after the 8-byte atom header
    let atom_size = (offset - 8) as u32;
    buf[0..4].copy_from_slice(&atom_size.to_ne_bytes());
    buf[4..8].copy_from_slice(&sequence_urid.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_atom_sequence_layout() {
        let mut buf = vec![0u8; 64];
        write_empty_atom_sequence(&mut buf, 42);
        assert_eq!(&buf[0..4], &8u32.to_ne_bytes()); // size = 8
        assert_eq!(&buf[4..8], &42u32.to_ne_bytes()); // type = sequence
        assert_eq!(&buf[8..16], &[0u8; 8]); // body zeroed
    }

    #[test]
    fn midi_atom_sequence_layout() {
        let mut buf = vec![0u8; 128];
        let events = [(0i64, &[0x90u8, 60, 100][..])];
        write_atom_sequence_midi(&mut buf, 42, 7, &events);

        // atom.size = 8 (body header) + 24 (one event padded) = 32
        assert_eq!(&buf[0..4], &32u32.to_ne_bytes());
        // atom.type = sequence
        assert_eq!(&buf[4..8], &42u32.to_ne_bytes());
        // body.unit = 0
        assert_eq!(&buf[8..12], &0u32.to_ne_bytes());
        // event.time = 0
        assert_eq!(&buf[16..24], &0i64.to_ne_bytes());
        // event.body.size = 3
        assert_eq!(&buf[24..28], &3u32.to_ne_bytes());
        // event.body.type = midi
        assert_eq!(&buf[28..32], &7u32.to_ne_bytes());
        // MIDI data
        assert_eq!(&buf[32..35], &[0x90, 60, 100]);
    }

    #[test]
    fn multiple_midi_events_padded() {
        let mut buf = vec![0u8; 256];
        let events = [
            (0i64, &[0x90u8, 60, 100][..]),
            (128i64, &[0x80u8, 60, 0][..]),
        ];
        write_atom_sequence_midi(&mut buf, 42, 7, &events);

        // atom.size = 8 (body) + 24 (event1) + 24 (event2) = 56
        assert_eq!(&buf[0..4], &56u32.to_ne_bytes());

        // Second event starts at offset 40 (8 header + 8 body + 24 first event)
        assert_eq!(&buf[40..48], &128i64.to_ne_bytes());
        assert_eq!(&buf[48..52], &3u32.to_ne_bytes());
        assert_eq!(&buf[56..59], &[0x80, 60, 0]);
    }

    #[test]
    fn instantiate_eg_amp() {
        let world = lilv::World::with_load_all();
        let uri = "http://lv2plug.in/plugins/eg-amp";
        let test_uri = world.new_uri(uri);
        if world.plugins().plugin(&test_uri).is_none() {
            eprintln!("eg-amp not installed, skipping");
            return;
        }

        let features = FeatureSet::new(48000.0, 1024);
        let inst = Lv2PluginInstance::new(&world, uri, "test-amp", 48000.0, 1024, &features, None);
        let inst = inst.expect("failed to instantiate eg-amp");

        assert_eq!(inst.audio_in_count(), 1);
        assert_eq!(inst.audio_out_count(), 1);
        assert!(!inst.has_midi_in());

        let controls = inst.current_controls();
        assert!(controls.contains_key("gain"));
    }
}
