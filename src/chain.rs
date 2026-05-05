use std::collections::HashMap;
use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::config::ChainConfig;
use crate::error::Error;
use crate::jalv::JalvInstance;
use crate::midi::{self, MidiRouter, MidiSender};

/// Per-plugin ring buffer capacity for queued MIDI events.
const MIDI_RING_CAPACITY: usize = 1024;

/// A running chain of LV2 plugins wired in series.
///
/// Each plugin runs as a separate `jalv` process. Audio ports are connected
/// left-to-right via `pw-link`. If any plugin exposes MIDI input, a dedicated
/// JACK client is created to route [`MidiEvent`](crate::MidiEvent)s from
/// user code into the corresponding plugin.
///
/// Drop or call [`Chain::stop`] to tear down all child processes and JACK
/// connections.
pub struct Chain {
    instances: Vec<JalvInstance>,
    midi_router: Option<MidiRouter>,
    midi_senders: HashMap<String, MidiSender>,
}

impl Chain {
    /// Spawn all plugins, wire audio ports in series, and set up MIDI routing.
    ///
    /// Blocks briefly (~700 ms) while jalv processes register their JACK ports.
    /// Returns an error if any plugin fails to spawn or wiring fails.
    pub fn start(config: &ChainConfig) -> Result<Self, Error> {
        if config.plugins.is_empty() {
            return Err(Error::Config("chain has no plugins".into()));
        }

        let mut instances = Vec::with_capacity(config.plugins.len());

        for plugin in &config.plugins {
            let instance = JalvInstance::spawn(plugin, config.buffer_size)?;
            instances.push(instance);
        }

        // Give jalv time to register JACK ports
        thread::sleep(Duration::from_millis(500));

        // Wire adjacent plugins (audio)
        for i in 0..instances.len() - 1 {
            let from = instances[i].jack_client_name().to_string();
            let to = instances[i + 1].jack_client_name().to_string();
            wire_plugins(&from, &to)?;
        }

        // Auto-connect to system physical ports
        if config.auto_connect_input || config.auto_connect_output {
            auto_connect(&instances, config)?;
        }

        // Create MIDI router — one output port per plugin that has MIDI input
        let mut midi_senders = HashMap::new();
        let mut midi_ports = Vec::new();

        for (instance, plugin_cfg) in instances.iter().zip(&config.plugins) {
            let name = instance.jack_client_name();
            let wants_midi = plugin_cfg
                .midi_in
                .unwrap_or_else(|| has_midi_input(name));
            if wants_midi {
                let (sender, port) = midi::create_midi_channel(name, MIDI_RING_CAPACITY);
                midi_senders.insert(name.to_string(), sender);
                midi_ports.push(port);
            }
        }

        let midi_router = if !midi_ports.is_empty() {
            let router_name = format!("{}-midi", config.jack_client_prefix);
            let router = MidiRouter::new(&router_name, midi_ports)?;

            // Wire MIDI router outputs to plugin MIDI inputs
            thread::sleep(Duration::from_millis(200));
            for instance in &instances {
                let name = instance.jack_client_name();
                if midi_senders.contains_key(name) {
                    wire_midi(&format!("{}-midi", config.jack_client_prefix), name)?;
                }
            }

            Some(router)
        } else {
            None
        };

        Ok(Self {
            instances,
            midi_router,
            midi_senders,
        })
    }

    /// Set a control port value on a running plugin.
    ///
    /// `plugin_name` must match [`PluginConfig::name`](crate::PluginConfig::name).
    /// `port` is the LV2 port symbol (e.g. `"threshold"`).
    pub fn set_control(
        &mut self,
        plugin_name: &str,
        port: &str,
        value: f32,
    ) -> Result<(), Error> {
        let instance = self
            .instances
            .iter_mut()
            .find(|i| i.jack_client_name() == plugin_name)
            .ok_or_else(|| Error::PluginNotFound {
                name: plugin_name.into(),
            })?;
        instance.set_control(port, value)
    }

    /// Get a [`MidiSender`] for pushing MIDI events to the named plugin.
    ///
    /// Returns [`Error::PluginNotFound`] if the plugin has no MIDI input or
    /// doesn't exist.
    pub fn midi_sender(&mut self, plugin_name: &str) -> Result<&mut MidiSender, Error> {
        self.midi_senders
            .get_mut(plugin_name)
            .ok_or_else(|| Error::PluginNotFound {
                name: plugin_name.into(),
            })
    }

    /// Get the [`MidiSender`] when exactly one plugin in the chain accepts MIDI.
    ///
    /// Returns [`Error::AmbiguousMidi`] if zero or more than one plugin has
    /// MIDI input.
    pub fn sole_midi_sender(&mut self) -> Result<&mut MidiSender, Error> {
        if self.midi_senders.len() != 1 {
            return Err(Error::AmbiguousMidi {
                count: self.midi_senders.len(),
            });
        }
        Ok(self.midi_senders.values_mut().next().unwrap())
    }

    /// Tear down the chain: drop MIDI resources, kill all jalv processes.
    pub fn stop(&mut self) {
        self.midi_senders.clear();
        drop(self.midi_router.take());
        for instance in &mut self.instances {
            instance.kill();
        }
        self.instances.clear();
    }

    /// Returns `true` if every plugin in the chain is still alive.
    pub fn is_running(&mut self) -> bool {
        self.instances.iter_mut().all(|i| i.is_running())
    }

    /// JACK port names for the first plugin's stereo audio input (`in_l`, `in_r`).
    pub fn chain_input_ports(&self) -> Option<(String, String)> {
        self.instances.first().map(|i| {
            let name = i.jack_client_name();
            (format!("{name}:in_l"), format!("{name}:in_r"))
        })
    }

    /// JACK port names for the last plugin's stereo audio output (`out_l`, `out_r`).
    pub fn chain_output_ports(&self) -> Option<(String, String)> {
        self.instances.last().map(|i| {
            let name = i.jack_client_name();
            (format!("{name}:out_l"), format!("{name}:out_r"))
        })
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        self.stop();
    }
}

const AUTO_CONNECT_RETRIES: u32 = 5;
const AUTO_CONNECT_RETRY_MS: u64 = 300;

/// Connect chain endpoints to physical system ports (capture/playback).
/// Uses JACK API to find physical audio ports by flag, not name.
/// Re-discovers ports on each retry to handle BT devices whose names change.
fn auto_connect(instances: &[JalvInstance], config: &ChainConfig) -> Result<(), Error> {
    if config.auto_connect_input {
        if let Some(first) = instances.first() {
            auto_connect_with_retry(first.jack_client_name(), PortDirection::Input)?;
        }
    }

    if config.auto_connect_output {
        if let Some(last) = instances.last() {
            auto_connect_with_retry(last.jack_client_name(), PortDirection::Output)?;
        }
    }

    Ok(())
}

fn auto_connect_with_retry(chain_client: &str, direction: PortDirection) -> Result<(), Error> {
    let mut last_err = None;

    for attempt in 0..AUTO_CONNECT_RETRIES {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(AUTO_CONNECT_RETRY_MS));
        }

        let (jc, _) = jack::Client::new("bn-autoconnect", jack::ClientOptions::NO_START_SERVER)
            .map_err(|e| Error::Wiring(format!("JACK client for auto-connect failed: {e}")))?;

        match direction {
            PortDirection::Input => {
                let capture_ports = jc.ports(
                    None,
                    Some("32 bit float mono audio"),
                    jack::PortFlags::IS_OUTPUT | jack::PortFlags::IS_PHYSICAL,
                );
                let chain_ins = jc.ports(
                    Some(&format!("^{chain_client}:")),
                    Some("32 bit float mono audio"),
                    jack::PortFlags::IS_INPUT,
                );
                match try_link_pairs(&capture_ports, &chain_ins) {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = Some(e),
                }
            }
            PortDirection::Output => {
                let playback_ports = jc.ports(
                    None,
                    Some("32 bit float mono audio"),
                    jack::PortFlags::IS_INPUT | jack::PortFlags::IS_PHYSICAL,
                );
                let chain_outs = jc.ports(
                    Some(&format!("^{chain_client}:")),
                    Some("32 bit float mono audio"),
                    jack::PortFlags::IS_OUTPUT,
                );
                match try_link_pairs(&chain_outs, &playback_ports) {
                    Ok(()) => return Ok(()),
                    Err(e) => last_err = Some(e),
                }
            }
        }

        log::warn!(
            "auto-connect attempt {}/{AUTO_CONNECT_RETRIES} failed for {chain_client}, retrying",
            attempt + 1,
        );
    }

    Err(last_err.unwrap())
}

fn try_link_pairs(sources: &[String], destinations: &[String]) -> Result<(), Error> {
    for (src, dst) in sources.iter().zip(destinations.iter()) {
        pw_link(src, dst)?;
    }
    Ok(())
}

/// Wire audio output ports of `from` to audio input ports of `to` via `pw-link`.
fn wire_plugins(from: &str, to: &str) -> Result<(), Error> {
    let out_ports = get_ports(from, PortDirection::Output)?;
    let in_ports = get_ports(to, PortDirection::Input)?;

    let audio_outs: Vec<_> = out_ports
        .iter()
        .filter(|p| is_audio_port(p))
        .collect();
    let audio_ins: Vec<_> = in_ports
        .iter()
        .filter(|p| is_audio_port(p))
        .collect();

    // Wire matching channels: out[0]→in[0], out[1]→in[1], etc.
    for (out, inp) in audio_outs.iter().zip(audio_ins.iter()) {
        pw_link(out, inp)?;
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum PortDirection {
    Output,
    Input,
}

/// List JACK ports for `client` using `pw-link`. Retries up to 10 times
/// (200 ms apart) while the client is still registering ports.
fn get_ports(client: &str, direction: PortDirection) -> Result<Vec<String>, Error> {
    let flag = match direction {
        PortDirection::Output => "-o",
        PortDirection::Input => "-i",
    };

    let mut attempts = 0;
    loop {
        let output = Command::new("pw-link")
            .arg(flag)
            .output()
            .map_err(|e| Error::PwLink(format!("failed to run pw-link: {e}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let ports: Vec<String> = stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| l.starts_with(client))
            .collect();

        if !ports.is_empty() || attempts >= 10 {
            return Ok(ports);
        }

        attempts += 1;
        thread::sleep(Duration::from_millis(200));
    }
}

/// Create a single PipeWire link between two JACK ports.
fn pw_link(from: &str, to: &str) -> Result<(), Error> {
    let output = Command::new("pw-link")
        .arg(from)
        .arg(to)
        .output()
        .map_err(|e| Error::PwLink(format!("failed to run pw-link: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::PwLink(format!(
            "pw-link {from} → {to} failed: {stderr}"
        )));
    }

    log::debug!("linked: {from} → {to}");
    Ok(())
}

fn is_audio_port(port: &str) -> bool {
    let lower = port.to_lowercase();
    !lower.contains("midi") && !lower.contains("event") && !lower.contains("control")
}

fn is_midi_port(port: &str) -> bool {
    let lower = port.to_lowercase();
    lower.contains("midi") || lower.contains("event")
}

/// Check if `client` has any MIDI input ports via JACK API (queries port type,
/// not name — works for plugins like DrumGizmo that name their MIDI port
/// `control`).
fn has_midi_input(client: &str) -> bool {
    let probe = jack::Client::new("bn-probe", jack::ClientOptions::NO_START_SERVER);
    let (jc, _) = match probe {
        Ok(c) => c,
        Err(_) => {
            log::warn!("JACK probe failed, falling back to port-name heuristic for {client}");
            return has_midi_input_heuristic(client);
        }
    };
    let prefix = format!("{client}:");
    let midi_ins = jc.ports(
        None,
        Some("8 bit raw midi"),
        jack::PortFlags::IS_INPUT,
    );
    midi_ins.iter().any(|p| p.starts_with(&prefix))
}

fn has_midi_input_heuristic(client: &str) -> bool {
    get_ports(client, PortDirection::Input)
        .map(|ports| ports.iter().any(|p| is_midi_port(p)))
        .unwrap_or(false)
}

/// Wire the MIDI router's output port for `target_client` to the target's
/// first MIDI input port. Uses JACK API to identify MIDI ports by type rather
/// than name.
fn wire_midi(router_client: &str, target_client: &str) -> Result<(), Error> {
    let (jc, _) = jack::Client::new("bn-wire", jack::ClientOptions::NO_START_SERVER)
        .map_err(|e| Error::Wiring(format!("JACK probe for MIDI wiring failed: {e}")))?;

    let router_prefix = format!("{router_client}:");
    let target_prefix = format!("{target_client}:");

    let all_midi_outs = jc.ports(None, Some("8 bit raw midi"), jack::PortFlags::IS_OUTPUT);
    let all_midi_ins = jc.ports(None, Some("8 bit raw midi"), jack::PortFlags::IS_INPUT);

    let router_port = all_midi_outs
        .iter()
        .find(|p| p.starts_with(&router_prefix) && p.contains(target_client));
    let target_port = all_midi_ins
        .iter()
        .find(|p| p.starts_with(&target_prefix));

    if let (Some(out), Some(inp)) = (router_port, target_port) {
        pw_link(out, inp)?;
    }

    Ok(())
}
