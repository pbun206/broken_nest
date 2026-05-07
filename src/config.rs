use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Top-level configuration for a plugin chain.
///
/// Defines the ordered list of plugins and global settings that apply to every
/// plugin in the chain (e.g. JACK buffer size).
#[derive(Debug, Deserialize, Clone)]
pub struct ChainConfig {
    /// Plugins in signal-flow order — audio output of `plugins[i]` is wired to
    /// the audio input of `plugins[i+1]`.
    pub plugins: Vec<PluginConfig>,
    /// Prefix used for JACK client names created by the chain (default `"bn"`).
    #[serde(default = "default_prefix")]
    pub jack_client_prefix: String,
    /// JACK buffer size override passed to every `jalv` instance (`-b` flag).
    pub buffer_size: Option<u32>,
    /// Connect first plugin's audio input from system capture (microphone).
    #[serde(default)]
    pub auto_connect_input: bool,
    /// Connect last plugin's audio output to system playback (speakers).
    #[serde(default)]
    pub auto_connect_output: bool,
    /// Directory for persisting plugin control state across restarts.
    /// Each plugin's controls are saved to `<state_dir>/<plugin_name>.toml`.
    pub state_dir: Option<PathBuf>,
}

/// Configuration for a single LV2 plugin instance.
#[derive(Debug, Deserialize, Clone)]
pub struct PluginConfig {
    /// LV2 plugin URI (e.g. `"http://calf.sourceforge.net/plugins/Compressor"`).
    pub uri: String,
    /// JACK client name for this plugin — also used to identify it in the chain.
    pub name: String,
    /// Initial control port values, keyed by LV2 port symbol.
    #[serde(default)]
    pub controls: HashMap<String, f32>,
    /// Directory containing saved plugin state (passed to `jalv -l`).
    pub state_dir: Option<PathBuf>,
    /// Whether to show the plugin's native UI (default `true`).
    /// When `false`, jalv uses a generic fallback UI.
    #[serde(default = "default_true")]
    pub show_ui: bool,
    /// Override MIDI input detection. When `true`, the chain always creates a
    /// MIDI route to this plugin. When `false`, never. When absent, the chain
    /// auto-detects by querying JACK port types.
    ///
    /// Set this explicitly for plugins whose MIDI ports don't contain "midi" in
    /// the name (e.g. DrumGizmo exposes `control`).
    pub midi_in: Option<bool>,
}

fn default_prefix() -> String {
    "bn".to_string()
}

fn default_true() -> bool {
    true
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            plugins: Vec::new(),
            jack_client_prefix: default_prefix(),
            buffer_size: None,
            auto_connect_input: false,
            auto_connect_output: false,
            state_dir: None,
        }
    }
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            uri: String::new(),
            name: String::new(),
            controls: HashMap::new(),
            state_dir: None,
            show_ui: true,
            midi_in: None,
        }
    }
}
