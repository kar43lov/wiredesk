use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;

use wiredesk_core::error::{Result, WireDeskError};

/// Picks the shell binary based on the requested name and platform.
fn resolve_shell(requested: &str) -> Vec<String> {
    let req = requested.trim().to_lowercase();
    #[cfg(target_os = "windows")]
    {
        match req.as_str() {
            "" | "powershell" | "pwsh" => vec!["powershell.exe".into(), "-NoLogo".into(), "-NoExit".into()],
            "cmd" => vec!["cmd.exe".into(), "/Q".into()],
            other => vec![other.into()],
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        match req.as_str() {
            "" | "sh" | "bash" => vec!["/bin/bash".into(), "-i".into()],
            "zsh" => vec!["/bin/zsh".into(), "-i".into()],
            other => vec![other.into()],
        }
    }
}

/// Outbound events from the shell — read by the session loop and forwarded over serial.
#[derive(Debug)]
#[allow(dead_code)]
pub enum ShellEvent {
    Output(Vec<u8>),
    /// Reserved: future extension when we want to signal exit through the
    /// channel rather than via try_exit_code() polling.
    Exit(i32),
}

/// Live shell process with two background threads:
///   - reader: pumps stdout/stderr into an mpsc::Receiver<ShellEvent>
///   - writer: pumps user input from mpsc::Sender<Vec<u8>> into the child's stdin
pub struct ShellProcess {
    child: Child,
    stdin_tx: mpsc::Sender<ShellInput>,
    pub events_rx: mpsc::Receiver<ShellEvent>,
}

enum ShellInput {
    Data(Vec<u8>),
    Close,
}

impl ShellProcess {
    pub fn spawn(requested: &str) -> Result<Self> {
        let argv = resolve_shell(requested);
        if argv.is_empty() {
            return Err(WireDeskError::Input("empty shell command".into()));
        }

        let mut cmd = Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| WireDeskError::Input(format!("spawn shell {:?}: {e}", argv[0])))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| WireDeskError::Input("no stdin handle".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| WireDeskError::Input("no stdout handle".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| WireDeskError::Input("no stderr handle".into()))?;

        let (events_tx, events_rx) = mpsc::channel();
        let (stdin_tx, stdin_rx) = mpsc::channel::<ShellInput>();

        // Reader thread for stdout
        let tx = events_tx.clone();
        thread::spawn(move || stream_to_channel(stdout, tx));

        // Reader thread for stderr (merged into same output channel)
        let tx = events_tx.clone();
        thread::spawn(move || stream_to_channel(stderr, tx));

        // Writer thread — pulls bytes from mpsc and writes to child stdin.
        // Closes stdin when ShellInput::Close is received or channel disconnects.
        thread::spawn(move || writer_thread(stdin, stdin_rx));

        Ok(Self {
            child,
            stdin_tx,
            events_rx,
        })
    }

    /// Send raw bytes to shell stdin. Returns false if writer thread is gone.
    pub fn write(&self, data: Vec<u8>) -> bool {
        self.stdin_tx.send(ShellInput::Data(data)).is_ok()
    }

    /// Request graceful close: flushes stdin and drops it (shell sees EOF).
    pub fn close(&self) {
        let _ = self.stdin_tx.send(ShellInput::Close);
    }

    /// Non-blocking check for child exit. Returns Some(code) if exited.
    pub fn try_exit_code(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        }
    }

    /// Force kill — used on Drop or explicit shutdown.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        self.close();
        // Best-effort kill to avoid orphan processes
        let _ = self.child.kill();
    }
}

fn stream_to_channel<R: Read>(mut reader: R, tx: mpsc::Sender<ShellEvent>) {
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                if tx.send(ShellEvent::Output(buf[..n].to_vec())).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn writer_thread(mut stdin: ChildStdin, rx: mpsc::Receiver<ShellInput>) {
    while let Ok(input) = rx.recv() {
        match input {
            ShellInput::Data(data) => {
                if stdin.write_all(&data).is_err() {
                    break;
                }
                let _ = stdin.flush();
            }
            ShellInput::Close => break,
        }
    }
    // stdin dropped here → child sees EOF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn echo_through_shell() {
        // Run a one-shot command via /bin/sh (works on macOS/Linux CI).
        let mut sh = ShellProcess::spawn("/bin/sh").unwrap();
        sh.write(b"echo wiredesk-shell-test\nexit\n".to_vec());

        // Drain output for up to ~2 sec total
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match sh.events_rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(ShellEvent::Output(d)) => got.extend_from_slice(&d),
                Ok(ShellEvent::Exit(_)) => break,
                Err(_) => {
                    if sh.try_exit_code().is_some() {
                        break;
                    }
                }
            }
        }
        let s = String::from_utf8_lossy(&got);
        assert!(s.contains("wiredesk-shell-test"), "output: {s:?}");
    }

    #[test]
    fn resolve_shell_defaults() {
        let r = resolve_shell("");
        assert!(!r.is_empty());
    }

    #[test]
    fn resolve_unknown_passes_through() {
        let r = resolve_shell("/usr/bin/env");
        assert_eq!(r[0], "/usr/bin/env");
    }
}
