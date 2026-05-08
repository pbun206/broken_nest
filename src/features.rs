use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::sync::Mutex;

pub use lv2_raw::LV2Feature;

// LV2 URI constants
pub const LV2_URID_MAP_URI: &str = "http://lv2plug.in/ns/ext/urid#map";
pub const LV2_URID_UNMAP_URI: &str = "http://lv2plug.in/ns/ext/urid#unmap";
pub const LV2_WORKER_SCHEDULE_URI: &str = "http://lv2plug.in/ns/ext/worker#schedule";
pub const LV2_LOG_LOG_URI: &str = "http://lv2plug.in/ns/ext/log#log";
pub const LV2_BUF_SIZE_BOUNDED_URI: &str =
    "http://lv2plug.in/ns/ext/buf-size#boundedBlockLength";
pub const LV2_BUF_SIZE_FIXED_URI: &str = "http://lv2plug.in/ns/ext/buf-size#fixedBlockLength";
pub const LV2_OPTIONS_URI: &str = "http://lv2plug.in/ns/ext/options#options";

// Well-known URIs to pre-map
pub const ATOM_SEQUENCE_URI: &str = "http://lv2plug.in/ns/ext/atom#Sequence";
pub const ATOM_CHUNK_URI: &str = "http://lv2plug.in/ns/ext/atom#Chunk";
pub const ATOM_FLOAT_URI: &str = "http://lv2plug.in/ns/ext/atom#Float";
pub const ATOM_INT_URI: &str = "http://lv2plug.in/ns/ext/atom#Int";
pub const ATOM_LONG_URI: &str = "http://lv2plug.in/ns/ext/atom#Long";
pub const ATOM_EVENT_TRANSFER_URI: &str = "http://lv2plug.in/ns/ext/atom#eventTransfer";
pub const MIDI_EVENT_URI: &str = "http://lv2plug.in/ns/ext/midi#MidiEvent";
pub const LOG_TRACE_URI: &str = "http://lv2plug.in/ns/ext/log#Trace";
pub const LOG_NOTE_URI: &str = "http://lv2plug.in/ns/ext/log#Note";
pub const LOG_WARNING_URI: &str = "http://lv2plug.in/ns/ext/log#Warning";
pub const LOG_ERROR_URI: &str = "http://lv2plug.in/ns/ext/log#Error";
pub const BUF_SIZE_MIN_URI: &str = "http://lv2plug.in/ns/ext/buf-size#minBlockLength";
pub const BUF_SIZE_MAX_URI: &str = "http://lv2plug.in/ns/ext/buf-size#maxBlockLength";
pub const BUF_SIZE_NOMINAL_URI: &str = "http://lv2plug.in/ns/ext/buf-size#nominalBlockLength";
pub const BUF_SIZE_SEQUENCE_SIZE_URI: &str = "http://lv2plug.in/ns/ext/buf-size#sequenceSize";

pub type LV2Urid = u32;

// ─── URID Map/Unmap ────────────────────────────────────────────────

#[repr(C)]
pub struct LV2UridMap {
    pub handle: *mut c_void,
    pub map: unsafe extern "C" fn(handle: *mut c_void, uri: *const c_char) -> LV2Urid,
}

#[repr(C)]
pub struct LV2UridUnmap {
    pub handle: *mut c_void,
    pub unmap: unsafe extern "C" fn(handle: *mut c_void, urid: LV2Urid) -> *const c_char,
}

pub struct UridMapper {
    uri_to_id: Mutex<HashMap<String, LV2Urid>>,
    id_to_uri: Mutex<Vec<CString>>,
}

impl UridMapper {
    pub fn new() -> Self {
        let mapper = Self {
            uri_to_id: Mutex::new(HashMap::new()),
            id_to_uri: Mutex::new(Vec::new()),
        };
        // Pre-map well-known URIs (IDs start at 1, 0 is invalid)
        for uri in [
            ATOM_SEQUENCE_URI,
            ATOM_CHUNK_URI,
            ATOM_FLOAT_URI,
            ATOM_INT_URI,
            ATOM_LONG_URI,
            ATOM_EVENT_TRANSFER_URI,
            MIDI_EVENT_URI,
            LOG_TRACE_URI,
            LOG_NOTE_URI,
            LOG_WARNING_URI,
            LOG_ERROR_URI,
            BUF_SIZE_MIN_URI,
            BUF_SIZE_MAX_URI,
            BUF_SIZE_NOMINAL_URI,
            BUF_SIZE_SEQUENCE_SIZE_URI,
        ] {
            mapper.map(uri);
        }
        mapper
    }

    pub fn map(&self, uri: &str) -> LV2Urid {
        let mut map = self.uri_to_id.lock().unwrap();
        if let Some(&id) = map.get(uri) {
            return id;
        }
        let mut vec = self.id_to_uri.lock().unwrap();
        let id = (vec.len() + 1) as LV2Urid;
        vec.push(CString::new(uri).unwrap());
        map.insert(uri.to_string(), id);
        id
    }

    pub fn unmap(&self, urid: LV2Urid) -> *const c_char {
        let vec = self.id_to_uri.lock().unwrap();
        let idx = urid as usize;
        if idx == 0 || idx > vec.len() {
            return std::ptr::null();
        }
        vec[idx - 1].as_ptr()
    }

    pub fn lookup(&self, uri: &str) -> Option<LV2Urid> {
        self.uri_to_id.lock().unwrap().get(uri).copied()
    }
}

unsafe extern "C" fn urid_map_callback(handle: *mut c_void, uri: *const c_char) -> LV2Urid {
    unsafe {
        let mapper = &*(handle as *const UridMapper);
        let uri = CStr::from_ptr(uri).to_str().unwrap_or("");
        mapper.map(uri)
    }
}

unsafe extern "C" fn urid_unmap_callback(handle: *mut c_void, urid: LV2Urid) -> *const c_char {
    unsafe {
        let mapper = &*(handle as *const UridMapper);
        mapper.unmap(urid)
    }
}

// ─── Log ────────────────────────────────────────────────────────────

#[repr(C)]
pub struct LV2LogLog {
    pub handle: *mut c_void,
    pub printf: *const c_void,
    pub vprintf: *const c_void,
}

unsafe extern "C" {
    fn lv2_log_printf(handle: *mut c_void, type_: u32, fmt: *const c_char, ...) -> i32;
    fn lv2_log_vprintf(handle: *mut c_void, type_: u32, fmt: *const c_char, ...) -> i32;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn lv2_log_shim_callback(
    handle: *mut c_void,
    type_: u32,
    msg: *const c_char,
) {
    unsafe {
        let mapper = &*(handle as *const UridMapper);
        let msg = CStr::from_ptr(msg).to_str().unwrap_or("<invalid utf8>");
        let msg = msg.trim();
        if msg.is_empty() {
            return;
        }

        let log_error = mapper.lookup(LOG_ERROR_URI).unwrap_or(0);
        let log_warning = mapper.lookup(LOG_WARNING_URI).unwrap_or(0);
        let log_note = mapper.lookup(LOG_NOTE_URI).unwrap_or(0);

        if type_ == log_error {
            log::error!("[lv2] {msg}");
        } else if type_ == log_warning {
            log::warn!("[lv2] {msg}");
        } else if type_ == log_note {
            log::debug!("[lv2] {msg}");
        } else {
            log::trace!("[lv2] {msg}");
        }
    }
}

// ─── Options ────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LV2OptionsOption {
    pub context: u32,
    pub subject: u32,
    pub key: LV2Urid,
    pub size: u32,
    pub type_: LV2Urid,
    pub value: *const c_void,
}

unsafe impl Send for LV2OptionsOption {}
unsafe impl Sync for LV2OptionsOption {}

const LV2_OPTIONS_INSTANCE: u32 = 0;

// ─── Worker ─────────────────────────────────────────────────────────

#[repr(C)]
pub struct LV2WorkerSchedule {
    pub handle: *mut c_void,
    pub schedule_work:
        unsafe extern "C" fn(handle: *mut c_void, size: u32, data: *const c_void) -> u32,
}

const LV2_WORKER_SUCCESS: u32 = 0;
const LV2_WORKER_ERR_UNKNOWN: u32 = 1;

pub struct WorkerHost {
    rt_to_worker_tx: rtrb::Producer<u8>,
    worker_to_rt_rx: rtrb::Consumer<u8>,
    thread: Option<std::thread::JoinHandle<()>>,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl WorkerHost {
    pub fn new(
        instance_handle: *mut c_void,
        work_fn: unsafe extern "C" fn(
            *mut c_void,
            unsafe extern "C" fn(*mut c_void, u32, *const c_void) -> u32,
            *mut c_void,
            u32,
            *const c_void,
        ) -> u32,
    ) -> Self {
        let (rt_to_worker_tx, mut rt_to_worker_rx) = rtrb::RingBuffer::new(65536);
        let (worker_to_rt_tx, worker_to_rt_rx) = rtrb::RingBuffer::new(65536);
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = instance_handle as usize;
        let tx = std::sync::Mutex::new(worker_to_rt_tx);

        let thread = std::thread::spawn(move || {
            let handle = handle as *mut c_void;
            while !shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
                if let Some((size, data)) = read_framed_message(&mut rt_to_worker_rx) {
                    let tx_ref = &tx;
                    unsafe {
                        work_fn(
                            handle,
                            worker_respond_callback,
                            tx_ref as *const _ as *mut c_void,
                            size,
                            data.as_ptr() as *const c_void,
                        );
                    }
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        });

        Self {
            rt_to_worker_tx,
            worker_to_rt_rx,
            thread: Some(thread),
            shutdown,
        }
    }

    pub fn schedule(&mut self, size: u32, data: &[u8]) -> bool {
        write_framed_message(&mut self.rt_to_worker_tx, size, data)
    }

    pub fn drain_responses(&mut self) -> Vec<(u32, Vec<u8>)> {
        let mut responses = Vec::new();
        while let Some((size, data)) = read_framed_message(&mut self.worker_to_rt_rx) {
            responses.push((size, data));
        }
        responses
    }
}

impl Drop for WorkerHost {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

pub(crate) unsafe extern "C" fn worker_respond_callback(
    handle: *mut c_void,
    size: u32,
    data: *const c_void,
) -> u32 {
    unsafe {
        let tx = &*(handle as *const std::sync::Mutex<rtrb::Producer<u8>>);
        let bytes = std::slice::from_raw_parts(data as *const u8, size as usize);
        let mut producer = tx.lock().unwrap();
        if write_framed_message(&mut producer, size, bytes) {
            LV2_WORKER_SUCCESS
        } else {
            LV2_WORKER_ERR_UNKNOWN
        }
    }
}

unsafe extern "C" fn worker_schedule_callback(
    handle: *mut c_void,
    size: u32,
    data: *const c_void,
) -> u32 {
    unsafe {
        let host = &mut *(handle as *mut WorkerHost);
        let bytes = std::slice::from_raw_parts(data as *const u8, size as usize);
        if host.schedule(size, bytes) {
            LV2_WORKER_SUCCESS
        } else {
            LV2_WORKER_ERR_UNKNOWN
        }
    }
}

pub(crate) fn write_framed_message(tx: &mut rtrb::Producer<u8>, size: u32, data: &[u8]) -> bool {
    let header = size.to_le_bytes();
    let total = 4 + data.len();
    if tx.slots() < total {
        return false;
    }
    let mut chunk = tx.write_chunk_uninit(total).unwrap();
    let out = chunk.as_mut_slices();
    let header_and_data: Vec<u8> = header.iter().chain(data.iter()).copied().collect();
    let (first, second) = out;
    let first_len = first.len().min(header_and_data.len());
    for (i, byte) in header_and_data[..first_len].iter().enumerate() {
        first[i].write(*byte);
    }
    if first_len < header_and_data.len() {
        for (i, byte) in header_and_data[first_len..].iter().enumerate() {
            second[i].write(*byte);
        }
    }
    unsafe { chunk.commit_all(); }
    true
}

pub(crate) fn read_framed_message(rx: &mut rtrb::Consumer<u8>) -> Option<(u32, Vec<u8>)> {
    if rx.slots() < 4 {
        return None;
    }
    let mut header = [0u8; 4];
    let chunk = rx.read_chunk(4).ok()?;
    let slices = chunk.as_slices();
    let mut i = 0;
    for &byte in slices.0.iter().chain(slices.1.iter()) {
        header[i] = byte;
        i += 1;
    }
    chunk.commit_all();

    let size = u32::from_le_bytes(header);
    if size == 0 {
        return Some((0, Vec::new()));
    }
    let size_usize = size as usize;
    // Wait briefly for data to arrive
    for _ in 0..100 {
        if rx.slots() >= size_usize {
            break;
        }
        std::thread::sleep(std::time::Duration::from_micros(100));
    }
    if rx.slots() < size_usize {
        return None;
    }
    let chunk = rx.read_chunk(size_usize).ok()?;
    let slices = chunk.as_slices();
    let mut data = Vec::with_capacity(size_usize);
    data.extend_from_slice(slices.0);
    data.extend_from_slice(slices.1);
    chunk.commit_all();
    Some((size, data))
}

// ─── FeatureSet ─────────────────────────────────────────────────────

pub struct FeatureSet {
    pub mapper: Box<UridMapper>,
    urid_map: Box<LV2UridMap>,
    urid_unmap: Box<LV2UridUnmap>,
    log_log: Box<LV2LogLog>,
    options: Vec<LV2OptionsOption>,
    option_values: Box<OptionValues>,
    features: Vec<LV2Feature>,
    feature_ptrs: Vec<*const LV2Feature>,
    feature_uris: Vec<CString>,
}

struct OptionValues {
    min_block: i32,
    max_block: i32,
    nominal_block: i32,
    sequence_size: i32,
}

impl FeatureSet {
    pub fn new(_sample_rate: f64, buffer_size: u32) -> Self {
        let mapper = Box::new(UridMapper::new());
        let mapper_ptr = &*mapper as *const UridMapper as *mut c_void;

        let urid_map = Box::new(LV2UridMap {
            handle: mapper_ptr,
            map: urid_map_callback,
        });

        let urid_unmap = Box::new(LV2UridUnmap {
            handle: mapper_ptr,
            unmap: urid_unmap_callback,
        });

        let log_log = Box::new(LV2LogLog {
            handle: mapper_ptr,
            printf: lv2_log_printf as *const c_void,
            vprintf: lv2_log_vprintf as *const c_void,
        });

        let option_values = Box::new(OptionValues {
            min_block: buffer_size as i32,
            max_block: buffer_size as i32,
            nominal_block: buffer_size as i32,
            sequence_size: 4096,
        });

        let atom_int = mapper.lookup(ATOM_INT_URI).unwrap();
        let min_key = mapper.lookup(BUF_SIZE_MIN_URI).unwrap();
        let max_key = mapper.lookup(BUF_SIZE_MAX_URI).unwrap();
        let nominal_key = mapper.lookup(BUF_SIZE_NOMINAL_URI).unwrap();
        let seq_key = mapper.lookup(BUF_SIZE_SEQUENCE_SIZE_URI).unwrap();

        let options = vec![
            LV2OptionsOption {
                context: LV2_OPTIONS_INSTANCE,
                subject: 0,
                key: min_key,
                size: 4,
                type_: atom_int,
                value: &option_values.min_block as *const i32 as *const c_void,
            },
            LV2OptionsOption {
                context: LV2_OPTIONS_INSTANCE,
                subject: 0,
                key: max_key,
                size: 4,
                type_: atom_int,
                value: &option_values.max_block as *const i32 as *const c_void,
            },
            LV2OptionsOption {
                context: LV2_OPTIONS_INSTANCE,
                subject: 0,
                key: nominal_key,
                size: 4,
                type_: atom_int,
                value: &option_values.nominal_block as *const i32 as *const c_void,
            },
            LV2OptionsOption {
                context: LV2_OPTIONS_INSTANCE,
                subject: 0,
                key: seq_key,
                size: 4,
                type_: atom_int,
                value: &option_values.sequence_size as *const i32 as *const c_void,
            },
            // Terminator
            LV2OptionsOption {
                context: 0,
                subject: 0,
                key: 0,
                size: 0,
                type_: 0,
                value: std::ptr::null(),
            },
        ];

        let mut feature_uris = Vec::new();
        let mut features = Vec::new();

        let pairs: Vec<(&str, *const c_void)> = vec![
            (LV2_URID_MAP_URI, &*urid_map as *const LV2UridMap as *const c_void),
            (LV2_URID_UNMAP_URI, &*urid_unmap as *const LV2UridUnmap as *const c_void),
            (LV2_LOG_LOG_URI, &*log_log as *const LV2LogLog as *const c_void),
            (LV2_OPTIONS_URI, options.as_ptr() as *const c_void),
            (LV2_BUF_SIZE_BOUNDED_URI, std::ptr::null()),
            (LV2_BUF_SIZE_FIXED_URI, std::ptr::null()),
        ];

        for (uri, data) in pairs {
            let c_uri = CString::new(uri).unwrap();
            features.push(LV2Feature {
                uri: c_uri.as_ptr(),
                data: data as *mut c_void,
            });
            feature_uris.push(c_uri);
        }

        // Fix URI pointers to point to owned CStrings
        for (i, feature) in features.iter_mut().enumerate() {
            feature.uri = feature_uris[i].as_ptr();
        }

        let mut feature_ptrs: Vec<*const LV2Feature> =
            features.iter().map(|f| f as *const LV2Feature).collect();
        feature_ptrs.push(std::ptr::null());

        Self {
            mapper,
            urid_map,
            urid_unmap,
            log_log,
            options,
            option_values,
            features,
            feature_ptrs,
            feature_uris,
        }
    }

    pub fn as_features(&self) -> Vec<&LV2Feature> {
        self.features.iter().collect()
    }

    pub fn as_feature_ptrs(&self) -> *const *const LV2Feature {
        self.feature_ptrs.as_ptr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urid_map_roundtrip() {
        let mapper = UridMapper::new();
        let id = mapper.map("http://example.org/test");
        assert!(id > 0);
        let ptr = mapper.unmap(id);
        assert!(!ptr.is_null());
        let uri = unsafe { CStr::from_ptr(ptr) }.to_str().unwrap();
        assert_eq!(uri, "http://example.org/test");
    }

    #[test]
    fn urid_map_deterministic() {
        let mapper = UridMapper::new();
        let id1 = mapper.map("http://example.org/a");
        let id2 = mapper.map("http://example.org/a");
        assert_eq!(id1, id2);
    }

    #[test]
    fn urid_map_different_uris_different_ids() {
        let mapper = UridMapper::new();
        let id1 = mapper.map("http://example.org/a");
        let id2 = mapper.map("http://example.org/b");
        assert_ne!(id1, id2);
    }

    #[test]
    fn urid_unmap_invalid_returns_null() {
        let mapper = UridMapper::new();
        assert!(mapper.unmap(0).is_null());
        assert!(mapper.unmap(99999).is_null());
    }

    #[test]
    fn pre_mapped_uris_exist() {
        let mapper = UridMapper::new();
        assert!(mapper.lookup(MIDI_EVENT_URI).is_some());
        assert!(mapper.lookup(ATOM_SEQUENCE_URI).is_some());
        assert!(mapper.lookup(ATOM_FLOAT_URI).is_some());
    }

    #[test]
    fn framed_message_roundtrip() {
        let (mut tx, mut rx) = rtrb::RingBuffer::new(1024);
        let data = b"hello world";
        assert!(write_framed_message(&mut tx, data.len() as u32, data));
        let (size, received) = read_framed_message(&mut rx).unwrap();
        assert_eq!(size, data.len() as u32);
        assert_eq!(&received, data);
    }

    #[test]
    fn feature_set_has_features() {
        let fs = FeatureSet::new(48000.0, 1024);
        let features = fs.as_features();
        assert!(features.len() >= 4);
        // Check URID map is present
        let uris: Vec<&str> = features
            .iter()
            .map(|f| unsafe { CStr::from_ptr(f.uri) }.to_str().unwrap())
            .collect();
        assert!(uris.contains(&LV2_URID_MAP_URI));
        assert!(uris.contains(&LV2_URID_UNMAP_URI));
        assert!(uris.contains(&LV2_OPTIONS_URI));
    }
}
