use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct ChainConfig {
    pub plugins: Vec<PluginConfig>,
    #[serde(default = "default_prefix")]
    pub jack_client_prefix: String,
    pub buffer_size: Option<u32>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PluginConfig {
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub controls: HashMap<String, f32>,
    pub state_dir: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub show_ui: bool,
}

fn default_prefix() -> String {
    "bn".to_string()
}

fn default_true() -> bool {
    true
}
