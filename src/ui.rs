#![allow(unused)]

use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crate::error::Error;

const GTK3_UI_URI: &str = "http://lv2plug.in/ns/extensions/ui#GtkUI";
const GTK3_CONTAINER_URI: &str = "http://lv2plug.in/ns/extensions/ui#Gtk3UI";
const X11_UI_URI: &str = "http://lv2plug.in/ns/extensions/ui#X11UI";

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

    fn suil_instance_port_event(
        instance: *mut SuilInstance,
        port_index: u32,
        buffer_size: u32,
        format: u32,
        buffer: *const c_void,
    );
}

// ─── UI metadata ───────────────────────────────────────────────────

pub(crate) struct PluginUiInfo {
    pub plugin_name: String,
    pub plugin_uri: CString,
    pub ui_uri: CString,
    pub ui_type_uri: CString,
    pub ui_bundle_path: CString,
    pub ui_binary_path: CString,
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

    let gtk3_node = world.new_uri(GTK3_UI_URI);
    let x11_node = world.new_uri(X11_UI_URI);

    for ui in uis.iter() {
        let is_gtk3 = ui.is_a(&gtk3_node);
        let is_x11 = ui.is_a(&x11_node);

        if !is_gtk3 && !is_x11 {
            continue;
        }

        let ui_type = if is_gtk3 { GTK3_UI_URI } else { X11_UI_URI };

        let ui_uri_str = ui.uri().as_str()?.to_owned();
        let bundle = ui.bundle_uri()?.as_str()?.to_owned();
        let binary = ui.binary_uri()?.as_str()?.to_owned();

        let bundle_path = uri_to_path(&bundle)?;
        let binary_path = uri_to_path(&binary)?;

        let mut sym_map = HashMap::new();
        for (sym, idx) in port_infos {
            sym_map.insert(sym.clone(), *idx);
        }

        return Some(PluginUiInfo {
            plugin_name: plugin_name.to_owned(),
            plugin_uri: CString::new(plugin_uri_str).ok()?,
            ui_uri: CString::new(ui_uri_str).ok()?,
            ui_type_uri: CString::new(ui_type).ok()?,
            ui_bundle_path: CString::new(bundle_path).ok()?,
            ui_binary_path: CString::new(binary_path).ok()?,
            port_symbol_to_index: sym_map,
            control_port_indices: control_port_indices.to_vec(),
        });
    }
    None
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
}

unsafe extern "C" fn port_write_callback(
    controller: *mut c_void,
    port_index: u32,
    buffer_size: u32,
    protocol: u32,
    buffer: *const c_void,
) {
    if protocol != 0 || buffer_size != 4 {
        return;
    }
    let ctrl = unsafe { &*(controller as *const UiController) };
    let value = unsafe { *(buffer as *const f32) };

    for (i, &ci) in ctrl.info.control_port_indices.iter().enumerate() {
        if ci == port_index {
            ctrl.control_bridge
                .set_control_by_bridge_index(&ctrl.info.plugin_name, i, value);
            return;
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

// ─── GTK UI thread ─────────────────────────────────────────────────

#[cfg(feature = "ui")]
#[allow(deprecated)]
mod gtk_thread {
    use super::*;

    pub(crate) enum UiCommand {
        Show {
            info: PluginUiInfo,
            control_bridge: Arc<crate::chain::ControlBridge>,
            features_ptr: *const *const crate::features::LV2Feature,
        },
        Hide {
            plugin_name: String,
        },
        Shutdown,
    }

    unsafe impl Send for UiCommand {}

    pub(crate) struct UiThread {
        tx: glib::Sender<UiCommand>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    struct LiveUi {
        suil_host: *mut SuilHost,
        suil_instance: *mut SuilInstance,
        window: gtk::Window,
        _controller: Box<UiController>,
    }

    impl UiThread {
        pub fn start() -> Self {
            let (tx, rx) = glib::MainContext::channel(glib::Priority::DEFAULT);

            let thread = std::thread::spawn(move || {
                if gtk::init().is_err() {
                    log::error!("GTK init failed");
                    return;
                }

                let mut live_uis: HashMap<String, LiveUi> = HashMap::new();

                rx.attach(None, move |cmd: UiCommand| {
                    match cmd {
                        UiCommand::Show {
                            info,
                            control_bridge,
                            features_ptr,
                        } => {
                            if live_uis.contains_key(&info.plugin_name) {
                                if let Some(ui) = live_uis.get(&info.plugin_name) {
                                    ui.window.present();
                                }
                                return glib::ControlFlow::Continue;
                            }

                            let name = info.plugin_name.clone();
                            let controller = Box::new(UiController {
                                info,
                                control_bridge,
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
                                return glib::ControlFlow::Continue;
                            }

                            let ctrl_ptr =
                                &*controller as *const UiController as *mut c_void;

                            let container_uri =
                                CString::new(GTK3_CONTAINER_URI).unwrap();

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
                                    features_ptr,
                                )
                            };

                            if instance.is_null() {
                                log::error!("suil_instance_new failed for '{name}'");
                                unsafe { suil_host_free(host) };
                                return glib::ControlFlow::Continue;
                            }

                            let widget_ptr =
                                unsafe { suil_instance_get_widget(instance) };

                            if widget_ptr.is_null() {
                                log::error!("suil widget is null for '{name}'");
                                unsafe {
                                    suil_instance_free(instance);
                                    suil_host_free(host);
                                }
                                return glib::ControlFlow::Continue;
                            }

                            use gtk::prelude::*;

                            let widget: gtk::Widget =
                                unsafe { gtk::glib::translate::from_glib_none(widget_ptr as *mut _) };

                            let window = gtk::Window::new(gtk::WindowType::Toplevel);
                            window.set_title(&name);
                            window.add(&widget);
                            window.show_all();

                            let name_clone = name.clone();
                            window.connect_delete_event(move |w, _| {
                                w.hide();
                                glib::Propagation::Stop
                            });

                            live_uis.insert(
                                name,
                                LiveUi {
                                    suil_host: host,
                                    suil_instance: instance,
                                    window,
                                    _controller: controller,
                                },
                            );
                        }
                        UiCommand::Hide { plugin_name } => {
                            if let Some(ui) = live_uis.remove(&plugin_name) {
                                use gtk::prelude::*;
                                ui.window.hide();
                                unsafe {
                                    suil_instance_free(ui.suil_instance);
                                    suil_host_free(ui.suil_host);
                                }
                            }
                        }
                        UiCommand::Shutdown => {
                            for (_, ui) in live_uis.drain() {
                                unsafe {
                                    suil_instance_free(ui.suil_instance);
                                    suil_host_free(ui.suil_host);
                                }
                            }
                            gtk::main_quit();
                            return glib::ControlFlow::Break;
                        }
                    }
                    glib::ControlFlow::Continue
                });

                gtk::main();
            });

            Self {
                tx,
                thread: Some(thread),
            }
        }

        pub fn show(
            &self,
            info: PluginUiInfo,
            control_bridge: Arc<crate::chain::ControlBridge>,
            features_ptr: *const *const crate::features::LV2Feature,
        ) {
            let _ = self.tx.send(UiCommand::Show {
                info,
                control_bridge,
                features_ptr,
            });
        }

        pub fn hide(&self, plugin_name: &str) {
            let _ = self.tx.send(UiCommand::Hide {
                plugin_name: plugin_name.to_owned(),
            });
        }

        pub fn shutdown(&self) {
            let _ = self.tx.send(UiCommand::Shutdown);
        }
    }

    impl Drop for UiThread {
        fn drop(&mut self) {
            self.shutdown();
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}

#[cfg(feature = "ui")]
pub(crate) use gtk_thread::UiThread;
