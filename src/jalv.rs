use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

use crate::config::PluginConfig;
use crate::error::Error;

/// Handle to a running `jalv` child process.
///
/// Owns the process stdin (for live control changes) and a background thread
/// that drains stdout. Killing the process and joining the reader thread
/// happens automatically on [`Drop`].
pub struct JalvInstance {
    pub name: String,
    child: Child,
    stdin: std::process::ChildStdin,
    stdout_reader: Option<std::thread::JoinHandle<Vec<String>>>,
}

impl JalvInstance {
    /// Spawn a `jalv.gtk3` process for the given plugin configuration.
    ///
    /// The process is started with `--print-controls` so control port info is
    /// captured on stdout. A background thread drains stdout to prevent the
    /// child from blocking on a full pipe.
    pub fn spawn(plugin: &PluginConfig, buffer_size: Option<u32>) -> Result<Self, Error> {
        let mut cmd = Command::new("jalv.gtk3");

        cmd.env("GDK_BACKEND", "x11");
        cmd.arg("-n").arg(&plugin.name);
        cmd.arg("--print-controls");

        if !plugin.show_ui {
            cmd.arg("--generic-ui");
        }

        if let Some(bs) = buffer_size {
            cmd.arg("-b").arg(bs.to_string());
        }

        for (sym, val) in &plugin.controls {
            cmd.arg("-c").arg(format!("{sym}={val}"));
        }

        if let Some(ref state_dir) = plugin.state_dir {
            if !state_dir.exists() {
                return Err(Error::StateDirMissing(state_dir.clone()));
            }
            cmd.arg("-l").arg(state_dir);
        }

        cmd.arg(&plugin.uri);

        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn().map_err(|e| Error::Spawn {
            name: plugin.name.clone(),
            source: e,
        })?;

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let name_clone = plugin.name.clone();
        let stdout_reader = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            let mut lines = Vec::new();
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        log::trace!("[{name_clone}] {l}");
                        lines.push(l);
                    }
                    Err(_) => break,
                }
            }
            lines
        });

        Ok(Self {
            name: plugin.name.clone(),
            child,
            stdin,
            stdout_reader: Some(stdout_reader),
        })
    }

    /// Write a control value change to the plugin via jalv's stdin protocol
    /// (`"symbol = value\n"`).
    pub fn set_control(&mut self, symbol: &str, value: f32) -> Result<(), Error> {
        writeln!(self.stdin, "{symbol} = {value}").map_err(|e| Error::ControlWrite {
            name: self.name.clone(),
            source: e,
        })
    }

    /// Returns `true` if the child process has not exited yet.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    /// Send SIGKILL to the child and wait for it to exit.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// JACK client name registered by this jalv instance.
    pub fn jack_client_name(&self) -> &str {
        &self.name
    }
}

impl Drop for JalvInstance {
    fn drop(&mut self) {
        self.kill();
        if let Some(handle) = self.stdout_reader.take() {
            let _ = handle.join();
        }
    }
}
