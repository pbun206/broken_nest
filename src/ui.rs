#![allow(unused)]

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::error::Error;

const LV2_UI_GTK_URI: &str = "http://lv2plug.in/ns/extensions/ui#GtkUI";
const LV2_UI_GTK3_URI: &str = "http://lv2plug.in/ns/extensions/ui#Gtk3UI";
const LV2_UI_X11_URI: &str = "http://lv2plug.in/ns/extensions/ui#X11UI";
const LV2_INSTANCE_ACCESS_URI: &str = "http://lv2plug.in/ns/ext/instance-access";
const LV2_UI_RESIZE_URI: &str = "http://lv2plug.in/ns/extensions/ui#resize";
const LV2_UI_IDLE_INTERFACE_URI: &str = "http://lv2plug.in/ns/extensions/ui#idleInterface";

// ─── GTK2 / GLib raw FFI ──────────────────────────────────────────

unsafe extern "C" {
    fn gtk_init(argc: *mut c_int, argv: *mut *mut *mut c_char);
    fn gtk_main();
    fn gtk_main_quit();
    fn gtk_window_new(window_type: c_int) -> *mut c_void;
    fn gtk_window_set_title(window: *mut c_void, title: *const c_char);
    fn gtk_window_set_default_size(window: *mut c_void, width: c_int, height: c_int);
    fn gtk_window_resize(window: *mut c_void, width: c_int, height: c_int);
    fn gtk_window_present(window: *mut c_void);
    fn gtk_container_add(container: *mut c_void, widget: *mut c_void);
    fn gtk_widget_show_all(widget: *mut c_void);
    fn gtk_widget_hide(widget: *mut c_void);
    fn gtk_widget_destroy(widget: *mut c_void);
    fn gtk_widget_get_visible(widget: *mut c_void) -> c_int;
    fn g_object_ref_sink(object: *mut c_void) -> *mut c_void;
    fn g_signal_connect_data(
        instance: *mut c_void,
        signal: *const c_char,
        handler: *const c_void,
        data: *mut c_void,
        destroy: *const c_void,
        flags: c_uint,
    ) -> c_ulong;
    fn g_idle_add(function: unsafe extern "C" fn(*mut c_void) -> c_int, data: *mut c_void) -> c_uint;
    fn g_timeout_add(
        interval: c_uint,
        function: unsafe extern "C" fn(*mut c_void) -> c_int,
        data: *mut c_void,
    ) -> c_uint;
}

unsafe extern "C" {
    fn gtk_vbox_new(homogeneous: c_int, spacing: c_int) -> *mut c_void;
    fn gtk_hbox_new(homogeneous: c_int, spacing: c_int) -> *mut c_void;
    fn gtk_label_new(text: *const c_char) -> *mut c_void;
    fn gtk_hscale_new_with_range(min: f64, max: f64, step: f64) -> *mut c_void;
    fn gtk_range_set_value(range: *mut c_void, value: f64);
    fn gtk_range_get_value(range: *mut c_void) -> f64;
    fn gtk_box_pack_start(
        box_: *mut c_void,
        child: *mut c_void,
        expand: c_int,
        fill: c_int,
        padding: c_uint,
    );
    fn gtk_scrolled_window_new(
        hadjustment: *mut c_void,
        vadjustment: *mut c_void,
    ) -> *mut c_void;
    fn gtk_scrolled_window_set_policy(
        scrolled_window: *mut c_void,
        hscrollbar_policy: c_int,
        vscrollbar_policy: c_int,
    );
    fn gtk_scrolled_window_add_with_viewport(
        scrolled_window: *mut c_void,
        child: *mut c_void,
    );
    fn gtk_widget_set_size_request(widget: *mut c_void, width: c_int, height: c_int);
    fn gtk_scale_set_digits(scale: *mut c_void, digits: c_int);
    fn gtk_scale_set_value_pos(scale: *mut c_void, pos: c_int);
}

const GTK_WINDOW_TOPLEVEL: c_int = 0;
const GTK_POLICY_NEVER: c_int = 2;
const GTK_POLICY_AUTOMATIC: c_int = 1;

// ─── suil FFI ──────────────────────────────────────────────────────

#[repr(C)]
struct SuilHost {
    _opaque: [u8; 0],
}

#[repr(C)]
struct SuilInstance {
    _opaque: [u8; 0],
}

type SuilPortWriteFunc = unsafe extern "C" fn(
    controller: *mut c_void,
    port_index: u32,
    buffer_size: u32,
    protocol: u32,
    buffer: *const c_void,
);

type SuilPortIndexFunc =
    unsafe extern "C" fn(controller: *mut c_void, port_symbol: *const c_char) -> u32;

unsafe extern "C" {
    fn suil_host_new(
        write_func: SuilPortWriteFunc,
        index_func: SuilPortIndexFunc,
        subscribe_func: *const c_void,
        unsubscribe_func: *const c_void,
    ) -> *mut SuilHost;

    fn suil_host_free(host: *mut SuilHost);

    fn suil_instance_new(
        host: *mut SuilHost,
        controller: *mut c_void,
        container_type_uri: *const c_char,
        plugin_uri: *const c_char,
        ui_uri: *const c_char,
        ui_type_uri: *const c_char,
        ui_bundle_path: *const c_char,
        ui_binary_path: *const c_char,
        features: *const *const crate::features::LV2Feature,
    ) -> *mut SuilInstance;

    fn suil_instance_free(instance: *mut SuilInstance);

    fn suil_instance_get_widget(instance: *mut SuilInstance) -> *mut c_void;

    fn suil_instance_get_handle(instance: *mut SuilInstance) -> *mut c_void;

    fn suil_instance_extension_data(
        instance: *mut SuilInstance,
        uri: *const c_char,
    ) -> *const c_void;

    fn suil_instance_port_event(
        instance: *mut SuilInstance,
        port_index: u32,
        buffer_size: u32,
        format: u32,
        buffer: *const c_void,
    );
}

// ─── LV2 UI idle interface ─────────────────────────────────────────

#[repr(C)]
struct LV2UIIdleInterface {
    idle: Option<unsafe extern "C" fn(handle: *mut c_void) -> c_int>,
}

// ─── LV2 UI resize host-side struct ────────────────────────────────

#[repr(C)]
struct LV2UIResize {
    handle: *mut c_void,
    ui_resize: unsafe extern "C" fn(handle: *mut c_void, width: i32, height: i32) -> i32,
}

unsafe extern "C" fn ui_resize_callback(handle: *mut c_void, width: i32, height: i32) -> i32 {
    #[cfg(feature = "ui")]
    unsafe {
        gtk_window_resize(handle, width as c_int, height as c_int);
    }
    0
}

// ─── UI metadata ───────────────────────────────────────────────────

pub(crate) struct PluginUiInfo {
    pub plugin_name: String,
    pub bridge_name: String,
    pub plugin_uri: CString,
    pub ui_uri: CString,
    pub ui_type_uri: CString,
    pub ui_bundle_path: CString,
    pub ui_binary_path: CString,
    pub port_symbol_to_index: HashMap<String, u32>,
    pub control_port_indices: Vec<u32>,
}

pub(crate) struct GenericUiInfo {
    pub plugin_name: String,
    pub bridge_name: String,
    pub control_ports: Vec<crate::plugin::ControlPortMeta>,
    pub port_symbol_to_index: HashMap<String, u32>,
    pub control_port_indices: Vec<u32>,
}

pub(crate) fn discover_ui(
    world: &lilv::World,
    plugin_uri_str: &str,
    plugin_name: &str,
    port_infos: &[(String, u32)],
    control_port_indices: &[u32],
) -> Option<PluginUiInfo> {
    let uri_node = world.new_uri(plugin_uri_str);
    let plugins = world.plugins();
    let plugin = plugins.plugin(&uri_node)?;
    let uis = plugin.uis()?;

    let gtk2_node = world.new_uri(LV2_UI_GTK_URI);
    let gtk3_node = world.new_uri(LV2_UI_GTK3_URI);
    let x11_node = world.new_uri(LV2_UI_X11_URI);

    // Prefer GtkUI (GTK2, native) > X11UI (wrapped via suil) > Gtk3UI (no wrapper available)
    let mut best: Option<(lilv::ui::UI, &str)> = None;
    let mut best_priority = 0u8;

    for ui in uis.iter() {
        let (ui_type, priority) = if ui.is_a(&gtk2_node) {
            (LV2_UI_GTK_URI, 3)
        } else if ui.is_a(&x11_node) {
            (LV2_UI_X11_URI, 2)
        } else if ui.is_a(&gtk3_node) {
            (LV2_UI_GTK3_URI, 1)
        } else {
            continue;
        };

        if priority > best_priority {
            best = Some((ui, ui_type));
            best_priority = priority;
        }
    }

    let (ui, ui_type) = best?;
    let ui_uri_str = ui.uri().as_str()?.to_owned();
    let bundle = ui.bundle_uri()?.as_str()?.to_owned();
    let binary = ui.binary_uri()?.as_str()?.to_owned();

    let bundle_path = uri_to_path(&bundle)?;
    let binary_path = uri_to_path(&binary)?;

    let mut sym_map = HashMap::new();
    for (sym, idx) in port_infos {
        sym_map.insert(sym.clone(), *idx);
    }

    log::debug!(
        "UI for '{plugin_name}': type={ui_type}, uri={ui_uri_str}"
    );

    let bridge_name = plugin_name.split('/').last().unwrap_or(plugin_name).to_owned();
    Some(PluginUiInfo {
        plugin_name: plugin_name.to_owned(),
        bridge_name,
        plugin_uri: CString::new(plugin_uri_str).ok()?,
        ui_uri: CString::new(ui_uri_str).ok()?,
        ui_type_uri: CString::new(ui_type).ok()?,
        ui_bundle_path: CString::new(bundle_path).ok()?,
        ui_binary_path: CString::new(binary_path).ok()?,
        port_symbol_to_index: sym_map,
        control_port_indices: control_port_indices.to_vec(),
    })
}

fn uri_to_path(uri: &str) -> Option<String> {
    if let Some(rest) = uri.strip_prefix("file://") {
        Some(rest.to_owned())
    } else {
        Some(uri.to_owned())
    }
}

// ─── UI controller (shared with GTK thread) ────────────────────────

struct UiController {
    info: PluginUiInfo,
    control_bridge: Arc<crate::chain::ControlBridge>,
    atom_ui_queue: crate::chain::AtomUiQueue,
    plugin_idx: usize,
}

unsafe extern "C" fn port_write_callback(
    controller: *mut c_void,
    port_index: u32,
    buffer_size: u32,
    protocol: u32,
    buffer: *const c_void,
) {
    let ctrl = unsafe { &*(controller as *const UiController) };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("/tmp/pb.log") {
        use std::io::Write;
        let _ = writeln!(f, "[port_write_callback] port={port_index} size={buffer_size} protocol={protocol}");
    }

    if protocol == 0 && buffer_size == 4 {
        let value = unsafe { *(buffer as *const f32) };
        for (i, &ci) in ctrl.info.control_port_indices.iter().enumerate() {
            if ci == port_index {
                ctrl.control_bridge
                    .set_control_by_bridge_index(&ctrl.info.bridge_name, i, value);
                return;
            }
        }
    } else if protocol != 0 && buffer_size > 0 {
        let data = unsafe { std::slice::from_raw_parts(buffer as *const u8, buffer_size as usize) };
        if let Ok(mut queue) = ctrl.atom_ui_queue.lock() {
            queue.push(crate::chain::AtomUiEvent {
                plugin_idx: ctrl.plugin_idx,
                port_index: port_index as usize,
                protocol,
                data: data.to_vec(),
            });
        }
    }
}

unsafe extern "C" fn port_index_callback(
    controller: *mut c_void,
    port_symbol: *const c_char,
) -> u32 {
    let ctrl = unsafe { &*(controller as *const UiController) };
    let symbol = unsafe { CStr::from_ptr(port_symbol) };
    if let Ok(s) = symbol.to_str() {
        ctrl.info
            .port_symbol_to_index
            .get(s)
            .copied()
            .unwrap_or(u32::MAX)
    } else {
        u32::MAX
    }
}

// ─── GTK2 UI thread ───────────────────────────────────────────────

#[cfg(feature = "ui")]
mod gtk_thread {
    use super::*;
    use std::sync::mpsc;

    pub(crate) enum UiCommand {
        Show {
            info: PluginUiInfo,
            control_bridge: Arc<crate::chain::ControlBridge>,
            features_ptr: *const *const crate::features::LV2Feature,
            instance_handle: Option<*mut c_void>,
            atom_ui_queue: crate::chain::AtomUiQueue,
            atom_ui_notify_queue: crate::chain::AtomUiQueue,
            plugin_idx: usize,
        },
        ShowGeneric {
            info: GenericUiInfo,
            control_bridge: Arc<crate::chain::ControlBridge>,
        },
        Toggle {
            plugin_name: String,
        },
        Hide {
            plugin_name: String,
        },
        Shutdown,
    }

    unsafe impl Send for UiCommand {}

    enum LiveUi {
        Native {
            suil_host: *mut SuilHost,
            suil_instance: *mut SuilInstance,
            window: *mut c_void,
            idle_iface: Option<*const LV2UIIdleInterface>,
            ui_handle: *mut c_void,
            plugin_idx: usize,
            _controller: Box<UiController>,
            _resize_data: Box<LV2UIResize>,
        },
        Generic {
            window: *mut c_void,
            _slider_data: Vec<Box<SliderCallbackData>>,
        },
    }

    impl LiveUi {
        fn window(&self) -> *mut c_void {
            match self {
                LiveUi::Native { window, .. } => *window,
                LiveUi::Generic { window, .. } => *window,
            }
        }
    }

    struct SliderCallbackData {
        control_bridge: Arc<crate::chain::ControlBridge>,
        plugin_name: String,
        bridge_index: usize,
    }

    struct UiThreadState {
        rx: Option<mpsc::Receiver<UiCommand>>,
        live_uis: HashMap<String, LiveUi>,
        atom_ui_notify_queue: Option<crate::chain::AtomUiQueue>,
    }

    static UI_SENDER: std::sync::OnceLock<mpsc::Sender<UiCommand>> = std::sync::OnceLock::new();

    unsafe extern "C" fn delete_event_callback(
        widget: *mut c_void,
        _event: *mut c_void,
        _data: *mut c_void,
    ) -> c_int {
        unsafe { gtk_widget_hide(widget); }
        1 // TRUE = don't destroy window
    }

    unsafe extern "C" fn idle_callback(data: *mut c_void) -> c_int {
        let state = unsafe { &mut *(data as *mut UiThreadState) };
        let receiver = match state.rx.as_ref() {
            Some(r) => r,
            None => return 0,
        };

        match receiver.try_recv() {
            Ok(cmd) => {
                match cmd {
                    UiCommand::Show {
                        info,
                        control_bridge,
                        features_ptr,
                        instance_handle,
                        atom_ui_queue,
                        atom_ui_notify_queue,
                        plugin_idx,
                    } => {
                        if state.atom_ui_notify_queue.is_none() {
                            state.atom_ui_notify_queue = Some(atom_ui_notify_queue);
                        }
                        handle_show(
                            &mut state.live_uis,
                            info,
                            control_bridge,
                            features_ptr,
                            instance_handle,
                            atom_ui_queue,
                            plugin_idx,
                        );
                    }
                    UiCommand::Toggle { plugin_name } => {
                        if let Some(ui) = state.live_uis.get(&plugin_name) {
                            let w = ui.window();
                            unsafe {
                                if gtk_widget_get_visible(w) != 0 {
                                    gtk_widget_hide(w);
                                } else {
                                    gtk_widget_show_all(w);
                                    gtk_window_present(w);
                                }
                            }
                        }
                    }
                    UiCommand::Hide { plugin_name } => {
                        if let Some(ui) = state.live_uis.get(&plugin_name) {
                            unsafe { gtk_widget_hide(ui.window()); }
                        }
                    }
                    UiCommand::ShowGeneric {
                        info,
                        control_bridge,
                    } => {
                        handle_show_generic(
                            &mut state.live_uis,
                            info,
                            control_bridge,
                        );
                    }
                    UiCommand::Shutdown => {
                        for (_, ui) in state.live_uis.drain() {
                            unsafe {
                                gtk_widget_destroy(ui.window());
                                if let LiveUi::Native { suil_instance, suil_host, .. } = ui {
                                    suil_instance_free(suil_instance);
                                    suil_host_free(suil_host);
                                }
                            }
                        }
                        state.rx = None;
                        unsafe { gtk_main_quit(); }
                        return 0; // FALSE = stop idle
                    }
                }
                1 // TRUE = continue
            }
            Err(mpsc::TryRecvError::Empty) => 1,
            Err(mpsc::TryRecvError::Disconnected) => {
                unsafe { gtk_main_quit(); }
                0
            }
        }
    }

    unsafe extern "C" fn ui_idle_pump(data: *mut c_void) -> c_int {
        let state = unsafe { &mut *(data as *mut UiThreadState) };
        // Forward atom notify events from process thread → native UIs
        if let Some(ref notify_queue) = state.atom_ui_notify_queue {
            if let Ok(mut queue) = notify_queue.try_lock() {
                for event in queue.drain(..) {
                    for (_name, ui) in &state.live_uis {
                        if let LiveUi::Native { suil_instance, plugin_idx, window, .. } = ui {
                            if *plugin_idx == event.plugin_idx {
                                unsafe {
                                    if gtk_widget_get_visible(*window) != 0 {
                                        suil_instance_port_event(
                                            *suil_instance,
                                            event.port_index as u32,
                                            event.data.len() as u32,
                                            event.protocol,
                                            event.data.as_ptr() as *const c_void,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for (name, ui) in &state.live_uis {
            if let LiveUi::Native { idle_iface: Some(iface_ptr), window, ui_handle, .. } = ui {
                unsafe {
                    if gtk_widget_get_visible(*window) != 0 {
                        let iface = &**iface_ptr;
                        if let Some(idle_fn) = iface.idle {
                            let result = idle_fn(*ui_handle);
                            if result != 0 {
                                log::debug!("idle() returned non-zero for '{name}', hiding");
                                gtk_widget_hide(*window);
                            }
                        }
                    }
                }
            }
        }
        1
    }

    fn ensure_ui_thread() -> &'static mpsc::Sender<UiCommand> {
        UI_SENDER.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<UiCommand>();

            std::thread::Builder::new()
                .name("broken_nest-gtk".into())
                .spawn(move || {
                    log::debug!("UI thread started");
                    unsafe { gtk_init(ptr::null_mut(), ptr::null_mut()); }
                    log::debug!("gtk_init() done");

                    let state = Box::into_raw(Box::new(UiThreadState {
                        rx: Some(rx),
                        live_uis: HashMap::new(),
                        atom_ui_notify_queue: None,
                    }));

                    unsafe {
                        g_idle_add(idle_callback, state as *mut c_void);
                        g_timeout_add(33, ui_idle_pump, state as *mut c_void);
                    }

                    log::debug!("entering gtk_main()");
                    unsafe { gtk_main(); }
                    log::debug!("gtk_main() returned");

                    unsafe { drop(Box::from_raw(state)); }
                })
                .expect("failed to spawn GTK UI thread");

            tx
        })
    }

    pub(crate) fn ui_show(
        info: PluginUiInfo,
        control_bridge: Arc<crate::chain::ControlBridge>,
        features_ptr: *const *const crate::features::LV2Feature,
        instance_handle: Option<*mut c_void>,
        atom_ui_queue: crate::chain::AtomUiQueue,
        atom_ui_notify_queue: crate::chain::AtomUiQueue,
        plugin_idx: usize,
    ) {
        let tx = ensure_ui_thread();
        let _ = tx.send(UiCommand::Show {
            info,
            control_bridge,
            features_ptr,
            instance_handle,
            atom_ui_queue,
            atom_ui_notify_queue,
            plugin_idx,
        });
    }

    pub(crate) fn ui_toggle(plugin_name: &str) {
        if let Some(tx) = UI_SENDER.get() {
            let _ = tx.send(UiCommand::Toggle {
                plugin_name: plugin_name.to_owned(),
            });
        }
    }

    pub(crate) fn ui_hide(plugin_name: &str) {
        if let Some(tx) = UI_SENDER.get() {
            let _ = tx.send(UiCommand::Hide {
                plugin_name: plugin_name.to_owned(),
            });
        }
    }

    pub fn ui_shutdown() {
        if let Some(tx) = UI_SENDER.get() {
            let _ = tx.send(UiCommand::Shutdown);
        }
    }

    fn handle_show(
        live_uis: &mut HashMap<String, LiveUi>,
        info: PluginUiInfo,
        control_bridge: Arc<crate::chain::ControlBridge>,
        features_ptr: *const *const crate::features::LV2Feature,
        instance_handle: Option<*mut c_void>,
        atom_ui_queue: crate::chain::AtomUiQueue,
        plugin_idx: usize,
    ) {
        if let Some(ui) = live_uis.get(&info.plugin_name) {
            unsafe { gtk_window_present(ui.window()); }
            return;
        }

        let name = info.plugin_name.clone();
        let controller = Box::new(UiController {
            info,
            control_bridge,
            atom_ui_queue,
            plugin_idx,
        });

        let host = unsafe {
            suil_host_new(
                port_write_callback,
                port_index_callback,
                ptr::null(),
                ptr::null(),
            )
        };

        if host.is_null() {
            log::error!("suil_host_new failed for '{name}'");
            return;
        }

        let ctrl_ptr = &*controller as *const UiController as *mut c_void;

        let window = unsafe {
            let w = gtk_window_new(GTK_WINDOW_TOPLEVEL);
            g_object_ref_sink(w);
            w
        };

        let title = CString::new(name.as_str()).unwrap();
        unsafe { gtk_window_set_title(window, title.as_ptr()); }

        let instance_access_uri = CString::new(LV2_INSTANCE_ACCESS_URI).unwrap();
        let resize_uri = CString::new(LV2_UI_RESIZE_URI).unwrap();

        let resize_data = Box::new(LV2UIResize {
            handle: window,
            ui_resize: ui_resize_callback,
        });

        // GTK2 container
        let container_uri = CString::new(LV2_UI_GTK_URI).unwrap();

        let mut ui_feature_ptrs: Vec<*const crate::features::LV2Feature> = Vec::new();
        unsafe {
            let mut i = 0;
            loop {
                let p = *features_ptr.add(i);
                if p.is_null() { break; }
                ui_feature_ptrs.push(p);
                i += 1;
            }
        }

        let instance_access_feature;
        if let Some(handle) = instance_handle {
            instance_access_feature = crate::features::LV2Feature {
                uri: instance_access_uri.as_ptr(),
                data: handle,
            };
            ui_feature_ptrs.push(&instance_access_feature as *const _);
        }

        let resize_feature = crate::features::LV2Feature {
            uri: resize_uri.as_ptr(),
            data: &*resize_data as *const LV2UIResize as *mut c_void,
        };
        ui_feature_ptrs.push(&resize_feature as *const _);
        ui_feature_ptrs.push(ptr::null());

        log::debug!(
            "suil_instance_new for '{name}' (container=GtkUI, ui_type={})",
            controller.info.ui_type_uri.to_str().unwrap_or("?")
        );

        let instance = unsafe {
            suil_instance_new(
                host,
                ctrl_ptr,
                container_uri.as_ptr(),
                controller.info.plugin_uri.as_ptr(),
                controller.info.ui_uri.as_ptr(),
                controller.info.ui_type_uri.as_ptr(),
                controller.info.ui_bundle_path.as_ptr(),
                controller.info.ui_binary_path.as_ptr(),
                ui_feature_ptrs.as_ptr(),
            )
        };

        if instance.is_null() {
            log::error!(
                "suil_instance_new failed for '{name}' (ui_type={})",
                controller.info.ui_type_uri.to_str().unwrap_or("?")
            );
            unsafe { suil_host_free(host); }
            return;
        }

        let widget_ptr = unsafe { suil_instance_get_widget(instance) };

        if widget_ptr.is_null() {
            log::error!("suil widget is null for '{name}'");
            unsafe {
                suil_instance_free(instance);
                suil_host_free(host);
            }
            return;
        }

        unsafe {
            let vbox = gtk_vbox_new(0, 0);
            let label_text = CString::new(name.as_str()).unwrap();
            let label = gtk_label_new(label_text.as_ptr());
            gtk_box_pack_start(vbox, label, 0, 0, 4);
            gtk_box_pack_start(vbox, widget_ptr, 1, 1, 0);
            gtk_container_add(window, vbox);
            gtk_window_set_default_size(window, 750, 740);
            gtk_widget_show_all(window);
            gtk_window_present(window);

            let signal = CString::new("delete-event").unwrap();
            g_signal_connect_data(
                window,
                signal.as_ptr(),
                delete_event_callback as *const c_void,
                ptr::null_mut(),
                ptr::null(),
                0,
            );
        }

        let idle_uri = CString::new(LV2_UI_IDLE_INTERFACE_URI).unwrap();
        let idle_ext = unsafe { suil_instance_extension_data(instance, idle_uri.as_ptr()) };
        let idle_iface = if !idle_ext.is_null() {
            log::debug!("idleInterface found for '{name}'");
            Some(idle_ext as *const LV2UIIdleInterface)
        } else {
            None
        };
        let ui_handle = unsafe { suil_instance_get_handle(instance) };

        log::debug!("UI window shown for '{name}'");

        live_uis.insert(name, LiveUi::Native {
            suil_host: host,
            suil_instance: instance,
            window,
            idle_iface,
            ui_handle,
            plugin_idx,
            _controller: controller,
            _resize_data: resize_data,
        });
    }

    unsafe extern "C" fn slider_value_changed(
        range: *mut c_void,
        data: *mut c_void,
    ) {
        let cb = unsafe { &*(data as *const SliderCallbackData) };
        let value = unsafe { gtk_range_get_value(range) } as f32;
        cb.control_bridge.set_control_by_bridge_index(
            &cb.plugin_name,
            cb.bridge_index,
            value,
        );
    }

    fn handle_show_generic(
        live_uis: &mut HashMap<String, LiveUi>,
        info: GenericUiInfo,
        control_bridge: Arc<crate::chain::ControlBridge>,
    ) {
        if let Some(ui) = live_uis.get(&info.plugin_name) {
            unsafe { gtk_window_present(ui.window()); }
            return;
        }

        let name = info.plugin_name.clone();
        let bridge_name = info.bridge_name.clone();

        let window = unsafe {
            let w = gtk_window_new(GTK_WINDOW_TOPLEVEL);
            g_object_ref_sink(w);
            w
        };

        let title = CString::new(format!("{} (generic)", &name)).unwrap();
        unsafe { gtk_window_set_title(window, title.as_ptr()); }

        let vbox = unsafe { gtk_vbox_new(0, 4) };
        unsafe {
            let label_text = CString::new(name.as_str()).unwrap();
            let label = gtk_label_new(label_text.as_ptr());
            gtk_box_pack_start(vbox, label, 0, 0, 4);
        }
        let mut slider_data: Vec<Box<SliderCallbackData>> = Vec::new();

        for (i, meta) in info.control_ports.iter().enumerate() {
            let hbox = unsafe { gtk_hbox_new(0, 8) };

            let label_text = CString::new(meta.symbol.as_str()).unwrap();
            let label = unsafe { gtk_label_new(label_text.as_ptr()) };
            unsafe { gtk_widget_set_size_request(label, 180, -1); }
            unsafe { gtk_box_pack_start(hbox, label, 0, 0, 4); }

            let range = meta.max - meta.min;
            let step = if range > 0.0 { (range / 1000.0) as f64 } else { 0.01 };
            let slider = unsafe {
                gtk_hscale_new_with_range(meta.min as f64, meta.max as f64, step)
            };
            unsafe {
                let initial = control_bridge
                    .get_control_by_bridge_index(&bridge_name, i)
                    .unwrap_or(meta.default);
                gtk_range_set_value(slider, initial as f64);
                gtk_scale_set_digits(slider, 4);
                gtk_scale_set_value_pos(slider, 2); // GTK_POS_RIGHT
                gtk_widget_set_size_request(slider, 300, -1);
            }

            let cb_data = Box::new(SliderCallbackData {
                control_bridge: control_bridge.clone(),
                plugin_name: bridge_name.clone(),
                bridge_index: i,
            });
            let cb_ptr = &*cb_data as *const SliderCallbackData as *mut c_void;

            let signal = CString::new("value-changed").unwrap();
            unsafe {
                g_signal_connect_data(
                    slider,
                    signal.as_ptr(),
                    slider_value_changed as *const c_void,
                    cb_ptr,
                    ptr::null(),
                    0,
                );
            }

            slider_data.push(cb_data);

            unsafe { gtk_box_pack_start(hbox, slider, 1, 1, 4); }
            unsafe { gtk_box_pack_start(vbox, hbox, 0, 0, 2); }
        }

        let scrolled = unsafe { gtk_scrolled_window_new(ptr::null_mut(), ptr::null_mut()) };
        unsafe {
            gtk_scrolled_window_set_policy(scrolled, GTK_POLICY_NEVER, GTK_POLICY_AUTOMATIC);
            gtk_scrolled_window_add_with_viewport(scrolled, vbox);
        }

        unsafe {
            gtk_container_add(window, scrolled);
            gtk_window_set_default_size(window, 550, 600);
            gtk_widget_show_all(window);
            gtk_window_present(window);

            let signal = CString::new("delete-event").unwrap();
            g_signal_connect_data(
                window,
                signal.as_ptr(),
                delete_event_callback as *const c_void,
                ptr::null_mut(),
                ptr::null(),
                0,
            );
        }

        log::debug!("Generic UI shown for '{name}' ({} controls)", info.control_ports.len());

        live_uis.insert(name, LiveUi::Generic {
            window,
            _slider_data: slider_data,
        });
    }

    pub(crate) fn ui_show_generic(
        info: GenericUiInfo,
        control_bridge: Arc<crate::chain::ControlBridge>,
    ) {
        let tx = ensure_ui_thread();
        let _ = tx.send(UiCommand::ShowGeneric {
            info,
            control_bridge,
        });
    }
}

#[cfg(feature = "ui")]
pub(crate) use gtk_thread::{ui_show, ui_show_generic, ui_toggle, ui_hide};
#[cfg(feature = "ui")]
pub use gtk_thread::ui_shutdown;
