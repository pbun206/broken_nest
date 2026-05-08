use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::config::PluginConfig;
use crate::error::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiMode {
    Headless,
    Gtk,
}

/// Handle to a running `jalv` child process.
///
/// Owns the process stdin (for live control changes) and a background thread
/// that drains stdout while parsing control port values. Killing the process
/// and joining the reader thread happens automatically on [`Drop`].
pub struct JalvInstance {
    pub name: String,
    child: Child,
    stdin: std::process::ChildStdin,
    stdout_reader: Option<std::thread::JoinHandle<()>>,
    controls: Arc<Mutex<HashMap<String, f32>>>,
    ui_mode: UiMode,
}

impl JalvInstance {
    /// Spawn a jalv process for the given plugin configuration.
    ///
    /// `mode` selects between headless (offscreen GDK) and GUI (X11 window).
    /// `initial_controls` are extra `-c` flags beyond what's in `plugin.controls`
    /// (used to restore saved state).
    pub fn spawn(
        plugin: &PluginConfig,
        buffer_size: Option<u32>,
        mode: UiMode,
        initial_controls: &HashMap<String, f32>,
    ) -> Result<Self, Error> {
        let mut cmd = Command::new("jalv.gtk3");

        match mode {
            UiMode::Headless => { cmd.env("GDK_BACKEND", "offscreen"); }
            UiMode::Gtk => { cmd.env("GDK_BACKEND", "x11"); }
        };
        cmd.arg("-n").arg(&plugin.name);
        cmd.arg("--print-controls");

        if let Some(bs) = buffer_size {
            cmd.arg("-b").arg(bs.to_string());
        }

        // Saved state first, then config controls override
        for (sym, val) in initial_controls {
            cmd.arg(format!("--control={sym}={val}"));
        }
        for (sym, val) in &plugin.controls {
            cmd.arg(format!("--control={sym}={val}"));
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

        let controls: Arc<Mutex<HashMap<String, f32>>> = Arc::new(Mutex::new(HashMap::new()));
        let controls_clone = Arc::clone(&controls);
        let name_clone = plugin.name.clone();

        let stdout_reader = std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if let Some((sym, val)) = parse_control_line(&l) {
                            if let Ok(mut map) = controls_clone.lock() {
                                map.insert(sym, val);
                            }
                        }
                        log::trace!("[{name_clone}] {l}");
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            name: plugin.name.clone(),
            child,
            stdin,
            stdout_reader: Some(stdout_reader),
            controls,
            ui_mode: mode,
        })
    }

    /// Write a control value change to the plugin via jalv's stdin protocol
    /// (`"symbol = value\n"`).
    pub fn set_control(&mut self, symbol: &str, value: f32) -> Result<(), Error> {
        if let Ok(mut map) = self.controls.lock() {
            map.insert(symbol.to_string(), value);
        }
        writeln!(self.stdin, "{symbol} = {value}").map_err(|e| Error::ControlWrite {
            name: self.name.clone(),
            source: e,
        })
    }

    /// Snapshot of all known control port values.
    pub fn current_controls(&self) -> HashMap<String, f32> {
        self.controls.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// Returns `true` if the child process has not exited yet.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().ok().flatten().is_none()
    }

    pub fn ui_mode(&self) -> UiMode {
        self.ui_mode
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

/// Parse a jalv `--print-controls` line like `"  gain = 0.5"` or `"gain = 0.5"`.
fn parse_control_line(line: &str) -> Option<(String, f32)> {
    let trimmed = line.trim();
    let (sym, rest) = trimmed.split_once('=')?;
    let sym = sym.trim().to_string();
    let val: f32 = rest.trim().parse().ok()?;
    if sym.is_empty() {
        return None;
    }
    Some((sym, val))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple() {
        let (sym, val) = parse_control_line("gain = 0.5").unwrap();
        assert_eq!(sym, "gain");
        assert!((val - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_leading_whitespace() {
        let (sym, val) = parse_control_line("  gain = 1.0").unwrap();
        assert_eq!(sym, "gain");
        assert!((val - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_negative_value() {
        let (sym, val) = parse_control_line("offset = -3.14").unwrap();
        assert_eq!(sym, "offset");
        assert!((val - (-3.14)).abs() < 0.001);
    }

    #[test]
    fn parse_integer_value() {
        let (sym, val) = parse_control_line("rate = 44100").unwrap();
        assert_eq!(sym, "rate");
        assert!((val - 44100.0).abs() < f32::EPSILON);
    }

    #[test]
    fn parse_no_equals_returns_none() {
        assert!(parse_control_line("no equals here").is_none());
    }

    #[test]
    fn parse_empty_symbol_returns_none() {
        assert!(parse_control_line(" = 0.5").is_none());
    }

    #[test]
    fn parse_non_numeric_value_returns_none() {
        assert!(parse_control_line("gain = abc").is_none());
    }

    #[test]
    fn parse_empty_line_returns_none() {
        assert!(parse_control_line("").is_none());
    }

    #[test]
    fn parse_extra_spaces() {
        let (sym, val) = parse_control_line("  volume   =   0.75  ").unwrap();
        assert_eq!(sym, "volume");
        assert!((val - 0.75).abs() < f32::EPSILON);
    }
}
