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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_builder_defaults() {
        let config = ChainBuilder::new("test").build();
        assert_eq!(config.name, "test");
        assert_eq!(config.jack_client_prefix, "bn");
        assert_eq!(config.buffer_size, None);
        assert!(!config.auto_connect_input);
        assert!(!config.auto_connect_output);
        assert!(config.plugins.is_empty());
    }

    #[test]
    fn chain_builder_all_options() {
        let plugin = PluginBuilder::new("urn:test", "p1").build();
        let config = ChainBuilder::new("rig")
            .jack_client_prefix("myprefix")
            .buffer_size(256)
            .auto_connect_input()
            .auto_connect_output()
            .plugin(plugin)
            .build();

        assert_eq!(config.jack_client_prefix, "myprefix");
        assert_eq!(config.buffer_size, Some(256));
        assert!(config.auto_connect_input);
        assert!(config.auto_connect_output);
        assert_eq!(config.plugins.len(), 1);
    }

    #[test]
    fn plugin_builder_defaults() {
        let plugin = PluginBuilder::new("urn:test", "comp").build();
        assert_eq!(plugin.uri, "urn:test");
        assert_eq!(plugin.name, "comp");
        assert!(plugin.show_ui);
        assert!(plugin.midi_in.is_none());
        assert!(plugin.controls.is_empty());
    }

    #[test]
    fn plugin_builder_all_options() {
        let plugin = PluginBuilder::new("urn:synth", "synth1")
            .hide_ui()
            .midi_in(true)
            .control("gain", 0.75)
            .control("freq", 440.0)
            .build();

        assert!(!plugin.show_ui);
        assert_eq!(plugin.midi_in, Some(true));
        assert_eq!(plugin.controls.len(), 2);
        assert!((plugin.controls["gain"] - 0.75).abs() < f32::EPSILON);
        assert!((plugin.controls["freq"] - 440.0).abs() < f32::EPSILON);
    }

    #[test]
    fn plugin_builder_control_override() {
        let plugin = PluginBuilder::new("urn:test", "p")
            .control("gain", 0.5)
            .control("gain", 0.9)
            .build();
        assert!((plugin.controls["gain"] - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn chain_builder_multiple_plugins() {
        let config = ChainBuilder::new("chain")
            .plugin(PluginBuilder::new("urn:a", "a").build())
            .plugin(PluginBuilder::new("urn:b", "b").build())
            .plugin(PluginBuilder::new("urn:c", "c").build())
            .build();
        assert_eq!(config.plugins.len(), 3);
        assert_eq!(config.plugins[0].name, "a");
        assert_eq!(config.plugins[1].name, "b");
        assert_eq!(config.plugins[2].name, "c");
    }
}
