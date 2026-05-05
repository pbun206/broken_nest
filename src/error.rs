use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to spawn jalv for plugin '{name}': {source}")]
    Spawn {
        name: String,
        source: std::io::Error,
    },

    #[error("plugin '{name}' exited unexpectedly with status {status}")]
    PluginDied { name: String, status: String },

    #[error("failed to write control to plugin '{name}': {source}")]
    ControlWrite {
        name: String,
        source: std::io::Error,
    },

    #[error("plugin '{name}' not found in chain")]
    PluginNotFound { name: String },

    #[error("port wiring failed: {0}")]
    Wiring(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("state directory does not exist: {0}")]
    StateDirMissing(PathBuf),

    #[error("pw-link command failed: {0}")]
    PwLink(String),
}
