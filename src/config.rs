use std::collections::HashMap;

/// Top-level configuration for a plugin chain.
#[derive(Debug, Clone)]
pub struct ChainConfig {
    pub name: String,
    pub plugins: Vec<PluginConfig>,
    pub jack_client_prefix: String,
    pub buffer_size: Option<u32>,
    pub auto_connect_input: bool,
    pub auto_connect_output: bool,
}

/// Configuration for a single LV2 plugin instance.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    pub uri: String,
    pub name: String,
    pub controls: HashMap<String, f32>,
    pub show_ui: bool,
    /// Override MIDI input detection. When `Some(true)`, the chain always
    /// creates a MIDI route to this plugin. When `Some(false)`, never.
    /// When `None`, auto-detects via JACK port types.
    pub midi_in: Option<bool>,
}

pub struct ChainBuilder {
    name: String,
    plugins: Vec<PluginConfig>,
    jack_client_prefix: String,
    buffer_size: Option<u32>,
    auto_connect_input: bool,
    auto_connect_output: bool,
}

impl ChainBuilder {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            plugins: Vec::new(),
            jack_client_prefix: "bn".into(),
            buffer_size: None,
            auto_connect_input: false,
            auto_connect_output: false,
        }
    }

    pub fn plugin(mut self, plugin: PluginConfig) -> Self {
        self.plugins.push(plugin);
        self
    }

    pub fn jack_client_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.jack_client_prefix = prefix.into();
        self
    }

    pub fn buffer_size(mut self, size: u32) -> Self {
        self.buffer_size = Some(size);
        self
    }

    pub fn auto_connect_input(mut self) -> Self {
        self.auto_connect_input = true;
        self
    }

    pub fn auto_connect_output(mut self) -> Self {
        self.auto_connect_output = true;
        self
    }

    pub fn build(self) -> ChainConfig {
        ChainConfig {
            name: self.name,
            plugins: self.plugins,
            jack_client_prefix: self.jack_client_prefix,
            buffer_size: self.buffer_size,
            auto_connect_input: self.auto_connect_input,
            auto_connect_output: self.auto_connect_output,
        }
    }
}

pub struct PluginBuilder {
    uri: String,
    name: String,
    controls: HashMap<String, f32>,
    show_ui: bool,
    midi_in: Option<bool>,
}

impl PluginBuilder {
    pub fn new(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: name.into(),
            controls: HashMap::new(),
            show_ui: true,
            midi_in: None,
        }
    }

    pub fn control(mut self, symbol: impl Into<String>, value: f32) -> Self {
        self.controls.insert(symbol.into(), value);
        self
    }

    pub fn hide_ui(mut self) -> Self {
        self.show_ui = false;
        self
    }

    pub fn midi_in(mut self, enabled: bool) -> Self {
        self.midi_in = Some(enabled);
        self
    }

    pub fn build(self) -> PluginConfig {
        PluginConfig {
            uri: self.uri,
            name: self.name,
            controls: self.controls,
            show_ui: self.show_ui,
            midi_in: self.midi_in,
        }
    }
}
