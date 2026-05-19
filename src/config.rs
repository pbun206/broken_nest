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
    pub generic_ui: bool,
    /// Override MIDI input detection. When `Some(true)`, the chain always
    /// creates a MIDI route to this plugin. When `Some(false)`, never.
    /// When `None`, auto-detects via JACK port types.
    pub midi_in: Option<bool>,
    pub dual_mono: bool,
    pub stereo_mix: Option<Vec<[f32; 2]>>,
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
    generic_ui: bool,
    midi_in: Option<bool>,
    dual_mono: bool,
    stereo_mix: Option<Vec<[f32; 2]>>,
}

impl PluginBuilder {
    pub fn new(uri: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            uri: uri.into(),
            name: name.into(),
            generic_ui: false,
            midi_in: None,
            dual_mono: false,
            stereo_mix: None,
        }
    }

    pub fn generic_ui(mut self) -> Self {
        self.generic_ui = true;
        self
    }

    pub fn midi_in(mut self, enabled: bool) -> Self {
        self.midi_in = Some(enabled);
        self
    }

    pub fn dual_mono(mut self) -> Self {
        self.dual_mono = true;
        self
    }

    pub fn stereo_mix(mut self, mix: Vec<[f32; 2]>) -> Self {
        self.stereo_mix = Some(mix);
        self
    }

    pub fn build(self) -> PluginConfig {
        PluginConfig {
            uri: self.uri,
            name: self.name,
            generic_ui: self.generic_ui,
            midi_in: self.midi_in,
            dual_mono: self.dual_mono,
            stereo_mix: self.stereo_mix,
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
        assert!(plugin.midi_in.is_none());
    }

    #[test]
    fn plugin_builder_all_options() {
        let plugin = PluginBuilder::new("urn:synth", "synth1")
            .midi_in(true)
            .build();

        assert_eq!(plugin.midi_in, Some(true));
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
