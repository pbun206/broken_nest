/// Errors that can occur while building or running a plugin chain.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("plugin '{name}' not found in chain")]
    PluginNotFound { name: String },

    #[error("port wiring failed: {0}")]
    Wiring(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("expected exactly one MIDI plugin, found {count}")]
    AmbiguousMidi { count: usize },

    #[error("state I/O error for plugin '{name}': {source}")]
    StateIo {
        name: String,
        source: std::io::Error,
    },

    #[error("LV2 plugin not found: {uri}")]
    Lv2PluginNotFound { uri: String },

    #[error("failed to instantiate LV2 plugin '{name}' ({uri}): {reason}")]
    Lv2Instantiation {
        name: String,
        uri: String,
        reason: String,
    },

    #[error("JACK error: {0}")]
    Jack(String),

    #[error("control port '{port}' not found on plugin '{name}'")]
    ControlPortNotFound { name: String, port: String },
}
