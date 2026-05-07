//! **broken_nest** — LV2 plugin chain host built on top of
//! [jalv](https://gitlab.com/drobilla/jalv).
//!
//! Spawns one `jalv` process per plugin, wires their audio ports together via
//! `pw-link`, and optionally routes MIDI to plugins that expose MIDI inputs.
//!
//! # Quick start
//!
//! ```no_run
//! use broken_nest::{Chain, ChainBuilder, PluginBuilder};
//!
//! let config = ChainBuilder::new("my-rig")
//!     .plugin(
//!         PluginBuilder::new(
//!             "http://calf.sourceforge.net/plugins/Compressor",
//!             "comp",
//!         ).build()
//!     )
//!     .plugin(
//!         PluginBuilder::new(
//!             "http://calf.sourceforge.net/plugins/Equalizer5Band",
//!             "eq",
//!         ).build()
//!     )
//!     .auto_connect_output()
//!     .build();
//!
//! let mut chain = Chain::start(&config).unwrap();
//!
//! // Send MIDI to a plugin
//! if let Ok(sender) = chain.midi_sender("synth") {
//!     sender.send(broken_nest::MidiEvent::NoteOn {
//!         channel: 0, note: 60, velocity: 100,
//!     }).ok();
//! }
//!
//! chain.stop();
//! ```

pub mod chain;
pub mod config;
pub mod error;
pub mod midi;
mod jalv;

pub use chain::Chain;
pub use config::{ChainBuilder, ChainConfig, PluginBuilder, PluginConfig};
pub use error::Error;
pub use midi::{MidiEvent, MidiSender};
