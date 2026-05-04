use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;

#[cfg(target_os = "windows")]
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

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

enum ShellInput {
    Data(Vec<u8>),
    Close,
}

/// Backend variant for `ShellProcess`. Pipe-mode keeps the legacy
/// `Stdio::piped()` flow used by `wd --exec` and the GUI shell-panel
/// on every platform. PTY-mode is gated to Windows host because:
///   1. ConPTY is the actual production target (interactive `wd` →
///      Win11 host running PowerShell with PSReadLine).
///   2. portable-pty pulls Unix-side filedescriptor / signal-handler
///      machinery whose lifecycle conflicts with the parallel cargo
///      test runner on macOS dev (forkpty triggers SIGABRT in
///      surrounding test threads). Confining the dep to cfg(windows)
///      keeps the Mac dev loop clean.
enum Backend {
    Pipe {
        child: Child,
    },
    #[cfg(target_os = "windows")]
    Pty {
        child: Box<dyn portable_pty::Child + Send + Sync>,
        master: Box<dyn MasterPty + Send>,
    },
}

/// Live shell process with two background threads:
///   - reader: pumps stdout/stderr (or PTY master) into an mpsc::Receiver<ShellEvent>
///   - writer: pumps user input from mpsc::Sender<Vec<u8>> into the child's stdin
pub struct ShellProcess {
    backend: Backend,
    stdin_tx: mpsc::Sender<ShellInput>,
    pub events_rx: mpsc::Receiver<ShellEvent>,
}

impl ShellProcess {
    /// Spawn a shell. `pty=None` → legacy `Stdio::piped()` (used by
    /// `wd --exec` and the GUI shell-panel). `pty=Some((cols, rows))`
    /// → real PTY via `portable-pty` (interactive `wd`). On non-Windows
    /// hosts, pty-mode returns an error — this is by design (see Backend
    /// docs above).
    pub fn spawn(requested: &str, pty: Option<(u16, u16)>) -> Result<Self> {
        let argv = resolve_shell(requested);
        if argv.is_empty() {
            return Err(WireDeskError::Input("empty shell command".into()));
        }
        match pty {
            None => Self::spawn_pipe(&argv),
            #[cfg(target_os = "windows")]
            Some((cols, rows)) => Self::spawn_pty(&argv, cols, rows),
            #[cfg(not(target_os = "windows"))]
            Some(_) => Err(WireDeskError::Input(
                "PTY-mode shell is only supported on Windows host (the actual deployment \
                 target). Run wiredesk-term on macOS against the real Win11 host."
                    .into(),
            )),
        }
    }

    fn spawn_pipe(argv: &[String]) -> Result<Self> {
        let mut cmd = Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }

        // Suppress the new console window the OS would otherwise pop up
        // for any child of a windows_subsystem=windows process. ConPTY
        // (the `Pty` backend) does not need this — its child is anchored
        // to the pseudo-console and never gets its own visible window.
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
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

        let tx = events_tx.clone();
        thread::spawn(move || stream_to_channel(stdout, tx));
        let tx = events_tx;
        thread::spawn(move || stream_to_channel(stderr, tx));
        thread::spawn(move || writer_thread_pipe(stdin, stdin_rx));

        Ok(Self {
            backend: Backend::Pipe { child },
            stdin_tx,
            events_rx,
        })
    }

    #[cfg(target_os = "windows")]
    fn spawn_pty(argv: &[String], cols: u16, rows: u16) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| WireDeskError::Input(format!("openpty: {e}")))?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| WireDeskError::Input(format!("spawn pty {:?}: {e}", argv[0])))?;

        // Slave is not needed in the parent process after spawn — the
        // child inherits its FDs. Keeping it open here would prevent
        // EOF on the master after the child exits.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| WireDeskError::Input(format!("clone pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| WireDeskError::Input(format!("take pty writer: {e}")))?;

        let (events_tx, events_rx) = mpsc::channel();
        let (stdin_tx, stdin_rx) = mpsc::channel::<ShellInput>();

        thread::spawn(move || stream_to_channel(reader, events_tx));
        thread::spawn(move || writer_thread_pty(writer, stdin_rx));

        Ok(Self {
            backend: Backend::Pty {
                child,
                master: pair.master,
            },
            stdin_tx,
            events_rx,
        })
    }

    /// Send raw bytes to shell stdin. Returns false if writer thread is gone.
    pub fn write(&self, data: Vec<u8>) -> bool {
        self.stdin_tx.send(ShellInput::Data(data)).is_ok()
    }

    /// Request graceful close: writer thread breaks its loop and drops
    /// its handle so the shell sees EOF (or the PTY's writer side closes).
    pub fn close(&self) {
        let _ = self.stdin_tx.send(ShellInput::Close);
    }

    /// Resize the PTY. No-op when the backend is pipe-mode (or this build
    /// is not a Windows host — pipe-only).
    pub fn resize(&self, cols: u16, rows: u16) {
        #[cfg(target_os = "windows")]
        if let Backend::Pty { master, .. } = &self.backend {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
            return;
        }
        // Pipe-mode or non-Windows build — discard.
        let _ = (cols, rows);
    }

    /// Non-blocking check for child exit. Returns Some(code) if exited.
    /// portable-pty's `ExitStatus::exit_code` is `u32`; `std::process`'
    /// is `Option<i32>` — coalesce both into the same `i32` ABI used by
    /// `Message::ShellExit`.
    pub fn try_exit_code(&mut self) -> Option<i32> {
        match &mut self.backend {
            Backend::Pipe { child } => match child.try_wait() {
                Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
                _ => None,
            },
            #[cfg(target_os = "windows")]
            Backend::Pty { child, .. } => match child.try_wait() {
                Ok(Some(status)) => Some(i32::try_from(status.exit_code()).unwrap_or(-1)),
                _ => None,
            },
        }
    }

    /// Force kill — used on Drop or explicit shutdown.
    pub fn kill(&mut self) {
        match &mut self.backend {
            Backend::Pipe { child } => {
                let _ = child.kill();
            }
            #[cfg(target_os = "windows")]
            Backend::Pty { child, .. } => {
                let _ = child.kill();
            }
        }
    }
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        self.close();
        // Best-effort kill to avoid orphan processes.
        match &mut self.backend {
            Backend::Pipe { child } => {
                let _ = child.kill();
            }
            #[cfg(target_os = "windows")]
            Backend::Pty { child, .. } => {
                let _ = child.kill();
            }
        }
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

fn writer_thread_pipe(mut stdin: ChildStdin, rx: mpsc::Receiver<ShellInput>) {
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

#[cfg(target_os = "windows")]
fn writer_thread_pty(mut writer: Box<dyn Write + Send>, rx: mpsc::Receiver<ShellInput>) {
    while let Ok(input) = rx.recv() {
        match input {
            ShellInput::Data(data) => {
                if writer.write_all(&data).is_err() {
                    break;
                }
                let _ = writer.flush();
            }
            ShellInput::Close => break,
        }
    }
    // writer dropped here → master's input side closes, child sees EOF on TTY
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn echo_through_shell() {
        // Pipe-mode regression: existing behaviour preserved.
        let mut sh = ShellProcess::spawn("/bin/sh", None).unwrap();
        sh.write(b"echo wiredesk-shell-test\nexit\n".to_vec());

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

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn pty_mode_returns_error_on_non_windows() {
        // PTY backend is Windows-only by design — non-Windows builds must
        // refuse pty-spawn explicitly so callers get a clear message
        // instead of a silent fall-through to pipe-mode.
        let r = ShellProcess::spawn("/bin/sh", Some((24, 80)));
        assert!(r.is_err(), "pty-spawn must fail on non-Windows");
    }

    #[test]
    fn resize_no_op_on_pipe_mode() {
        // resize() on a pipe-mode shell must not panic. On non-Windows
        // builds this is the only resize-path that exists; on Windows
        // it exercises the pipe branch's early return.
        #[cfg(not(target_os = "windows"))]
        let sh = ShellProcess::spawn("/bin/sh", None).unwrap();
        #[cfg(target_os = "windows")]
        let sh = ShellProcess::spawn("cmd", None).unwrap();
        sh.resize(80, 24);
        sh.resize(0, 0);
        sh.resize(u16::MAX, u16::MAX);
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
