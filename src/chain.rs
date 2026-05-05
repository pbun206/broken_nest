use std::process::Command;
use std::thread;
use std::time::Duration;

use crate::config::ChainConfig;
use crate::error::Error;
use crate::jalv::JalvInstance;

pub struct Chain {
    instances: Vec<JalvInstance>,
}

impl Chain {
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

        // Wire adjacent plugins
        for i in 0..instances.len() - 1 {
            let from = instances[i].jack_client_name().to_string();
            let to = instances[i + 1].jack_client_name().to_string();
            wire_plugins(&from, &to)?;
        }

        Ok(Self { instances })
    }

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

    pub fn stop(&mut self) {
        for instance in &mut self.instances {
            instance.kill();
        }
        self.instances.clear();
    }

    pub fn is_running(&mut self) -> bool {
        self.instances.iter_mut().all(|i| i.is_running())
    }

    pub fn chain_input_ports(&self) -> Option<(String, String)> {
        self.instances.first().map(|i| {
            let name = i.jack_client_name();
            (format!("{name}:in_l"), format!("{name}:in_r"))
        })
    }

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
    // Filter out MIDI/control ports — audio ports typically contain
    // "audio", "out_l", "out_r", "in_l", "in_r", "output", "input"
    // MIDI ports contain "midi", "event"
    let lower = port.to_lowercase();
    !lower.contains("midi") && !lower.contains("event") && !lower.contains("control")
}
