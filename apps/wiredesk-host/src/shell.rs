use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[cfg(target_os = "windows")]
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::packet::MAX_PAYLOAD;

/// Picks the shell binary based on the requested name and platform.
fn resolve_shell(requested: &str) -> Vec<String> {
    let req = requested.trim().to_lowercase();
    #[cfg(target_os = "windows")]
    {
        match req.as_str() {
            "" | "powershell" | "pwsh" => {
                vec!["powershell.exe".into(), "-NoLogo".into(), "-NoExit".into()]
            }
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

/// The argv a `ShellOpen` for `requested` would run. The session uses it to
/// tell whether a shell it warmed up earlier matches the one now asked for.
pub fn shell_argv(requested: &str) -> Vec<String> {
    resolve_shell(requested)
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
        let spill = PowerShellSpill::for_argv(argv);
        thread::spawn(move || writer_thread_pipe(stdin, stdin_rx, spill));

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
    /// Throw away whatever the shell has printed so far and report how
    /// many chunks went. Used when a pre-warmed shell is handed to a new
    /// `ShellOpen`: anything it printed while it was waiting belongs to
    /// nobody, and letting it through would put it in front of the next
    /// command's output.
    pub fn drain_pending(&self) -> usize {
        let mut n = 0;
        while self.events_rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }

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

/// Longest line handed to PowerShell's stdin verbatim. Past this the line
/// is spilled to a temp script and dot-sourced instead - see
/// [`PowerShellSpill`]. One wire packet is the natural cut: a line that
/// fits in a single `ShellInput` never changes behaviour.
const SPILL_THRESHOLD: usize = MAX_PAYLOAD;

/// How much of the original line's head and tail ride along in the
/// replacement line's trailing comment, each. `wd --exec` recognises the
/// echo of its own payload by the markers inside it, and in `--compress`
/// mode it needs *both* the `__WD_READY_` near the start and the
/// `__WD_DONE_` at the end to match. With only the tail it reads its own
/// echo as the finished sentinel and reports success before the command
/// has run. 192 bytes clears the longest of those prefixes.
const ECHO_FINGERPRINT: usize = 192;

/// Backstop for a burst whose total length is an exact multiple of
/// `MAX_PAYLOAD`: no short packet ever closes it, so without this the
/// writer would wait for a follow-up that never comes. Comfortably longer
/// than any inter-packet gap (26-40 ms on serial, ~100 ms on Bluetooth) so
/// it never splits a payload in normal use, and short enough that the one
/// payload in four thousand that needs it is not noticeably slower.
const BURST_GRACE: Duration = Duration::from_millis(400);

/// Spill long PowerShell command lines to a temp script.
///
/// PowerShell's console-host input path is superlinear in line length.
/// Measured live 2026-09-11 on the Win11 host: a 48 KB one-liner took
/// **14.8 s** to parse and run, while the identical text dot-sourced from
/// a file finished inside the `wd --exec` baseline (~0 s of extra time).
/// Delivery was never the problem - host-side instrumentation showed the
/// whole 48 KB arriving in 0.3 s with each `write_all` to the pipe costing
/// 14-320 microseconds. Long payloads are exactly what agents send (a
/// base64 blob, a here-doc script), so they hit the worst case every time,
/// on serial and Bluetooth alike.
///
/// Dot-sourcing runs the script in the *current* scope, so `$LASTEXITCODE`,
/// `$ErrorActionPreference`, variables and the `__WD_DONE_` sentinel behave
/// exactly as if the line had been typed. The script is written as UTF-8
/// with a BOM because Windows PowerShell 5.1 otherwise reads a plain file
/// as ANSI and mangles non-ASCII.
struct PowerShellSpill {
    path: std::path::PathBuf,
    /// Set once the dot-source line has been written to the shell. From
    /// that moment the file belongs to PowerShell, which deletes it as the
    /// second statement of that same line.
    handed_over: std::sync::atomic::AtomicBool,
}

/// Delete spill scripts left behind by a host that died between writing one
/// and feeding it to its shell.
///
/// Normally nothing accumulates: an unused script is removed by
/// [`PowerShellSpill::cleanup`] and a used one by the PowerShell line that
/// reads it. A crash or a kill skips both, and what stays behind is the
/// text of a command - possibly a password or a token. `age` guards against
/// touching a file another instance is using right now.
pub fn vacuum_stale_spills(age: Duration) {
    let dir = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.starts_with("wiredesk-exec-") && name.ends_with(".ps1")) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t.elapsed().map(|e| e >= age).unwrap_or(false))
            .unwrap_or(false);
        if old && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        log::info!("shell: removed {removed} stale spill script(s) from {dir:?}");
    }
}

/// Whether a spill file's own path can survive the trip through stdin.
///
/// The path is handed to PowerShell inside the replacement line, and that
/// line goes down the same pipe as everything else: we write UTF-8, and
/// Windows PowerShell decodes stdin in the console code page. An ASCII
/// path is identical under both, a Cyrillic `%TEMP%` is not - PowerShell
/// would then look for a mis-spelled file, `ReadAllText` would throw, and
/// the command would never reach its sentinel. The `is_ascii` gate on the
/// command line itself does not cover this: the command can be pure ASCII
/// while the temp directory is not.
fn spillable_path(path: &std::path::Path) -> bool {
    path.to_str().is_some_and(|s| s.is_ascii())
}

impl PowerShellSpill {
    /// `Some` only for a PowerShell child - `cmd.exe`, `bash` and friends
    /// keep the verbatim path.
    fn for_argv(argv: &[String]) -> Option<Self> {
        let exe = argv.first()?.to_ascii_lowercase();
        if !(exe.contains("powershell") || exe.contains("pwsh")) {
            return None;
        }
        // One file per shell instance: two pipe-mode shells at once would
        // otherwise overwrite each other's script between the write and
        // the dot-source.
        //
        // The name is random rather than derived from pid and a counter,
        // and that matters now that the host can run elevated. A guessable
        // path in the user's own `%TEMP%` lets any *unelevated* code
        // running as the same user pre-create it - as a hardlink to a file
        // it could not otherwise touch, so our elevated write clobbers it,
        // or as a file it keeps rewriting so PowerShell reads something we
        // never wrote. Both need the attacker to know the path first.
        let path = std::env::temp_dir().join(format!(
            "wiredesk-exec-{}-{}.ps1",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        if !spillable_path(&path) {
            log::warn!(
                "shell: long-line spill disabled, {path:?} is not pure ASCII \
                 (PowerShell would read the path back in the console code page)"
            );
            return None;
        }
        Some(Self {
            path,
            handed_over: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Write `line` (without its trailing newline) to the temp script and
    /// return the one-liner that dot-sources it and cleans up after.
    ///
    /// `None` leaves the line to travel the ordinary way, which is slow but
    /// always right. That happens when the file cannot be written, and for
    /// any line that is not pure ASCII. Non-ASCII is deliberate: today the
    /// bytes of a command survive the trip only because PowerShell decodes
    /// stdin and encodes stdout with the same console code page, so a
    /// mis-decoded string is re-encoded back to the original bytes. Reading
    /// the same text from a UTF-8 file breaks that symmetry - the string is
    /// then genuinely correct on the way in and gets mangled on the way out
    /// (live 2026-09-11: Cyrillic came back as garbage). Long commands are
    /// base64 blobs and scripts in practice, so the ASCII gate keeps the
    /// win where it matters and changes nothing elsewhere.
    fn dot_source_block(&self, block: &[u8]) -> Option<Vec<u8>> {
        if !block.is_ascii() {
            return None;
        }
        // PowerShell single-quoted literal: the only escape is a doubled
        // quote. Paths with one are absurd but cost nothing to handle.
        let quoted = self.path.to_string_lossy().replace('\'', "''");
        // A line break inside the path would end the quoted string and turn
        // the rest into its own statement. `TEMP` is the user's own to set,
        // so this is not a privilege boundary - but a shell line assembled
        // from data should never be able to grow a second statement.
        //
        // Checked *before* the file is written: a refusal afterwards would
        // leave the command's own text - passwords and tokens included -
        // sitting in `%TEMP%` with nothing on its way to delete it.
        if quoted.contains(|c: char| c.is_control()) {
            log::warn!(
                "shell: refusing to spill, {:?} holds a control character",
                self.path
            );
            return None;
        }
        let body = block.strip_suffix(b"\n").unwrap_or(block);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        let mut file = Vec::with_capacity(body.len() + 3);
        file.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
        file.extend_from_slice(body);
        // `create_new` rather than a plain write: it fails if anything is
        // already at the path, so a pre-created file or a link planted
        // there cannot be written *through*. Combined with the random name
        // that leaves no way to aim our elevated write at a chosen file -
        // and the failure is safe, because a refusal here just sends the
        // command down the ordinary, slower path.
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(&file));
        if let Err(e) = written {
            log::warn!(
                "shell: cannot spill {} bytes to {:?}: {e}",
                body.len(),
                self.path
            );
            return None;
        }
        // PowerShell mirrors every line it reads from a redirected stdin
        // back to stdout, and `wd --exec` recognises that echo by the
        // sentinel its own payload ends with. Our replacement line has no
        // sentinel in it, so the echo would surface as output (live
        // 2026-09-11: a stray `. 'C:\...ps1'` line in front of the result).
        // Carrying the tail of the original line along as a comment keeps
        // whatever fingerprint the caller put there, without this code
        // having to know what a sentinel is.
        // Same reasoning for the fingerprint: `#` comments run to the end
        // of the line, so a bare CR inside the tail would start a new one.
        // A trailing backtick would splice the following line instead.
        let strip = |b: &[u8]| -> String {
            String::from_utf8_lossy(b)
                .chars()
                .filter(|c| !c.is_control())
                .collect()
        };
        let head = strip(&body[..ECHO_FINGERPRINT.min(body.len())]);
        let tail = strip(&body[body.len().saturating_sub(ECHO_FINGERPRINT)..]);
        let tail = tail.trim_end_matches('`');
        log::debug!("shell: spilled {} bytes to {:?}", body.len(), self.path);
        // From here on the emitted line owns the file: it deletes it right
        // after reading it, and `cleanup` must not race that.
        self.handed_over
            .store(true, std::sync::atomic::Ordering::Release);
        // A script block built from a string is exempt from execution
        // policy, unlike dot-sourcing the path directly, which a host set
        // to `Restricted` or `AllSigned` would refuse - and that refusal
        // would look like a command that never produced its sentinel. It
        // costs nothing: measured live 2026-09-11, 0.40 s against 0.43 s
        // for the same 48 KB script dot-sourced from its path.
        //
        // Read, delete, *then* run. Deleting afterwards instead left the
        // file on disk for real: the client closes the shell the moment it
        // sees the sentinel, and the kill beat the trailing `Remove-Item`
        // often enough that scripts piled up in `%TEMP%` (live 2026-09-11).
        // The file holds the command's own text, so its lifetime should be
        // the read and nothing more.
        //
        // The two variables are dot-sourced into the shell's own scope, so
        // they carry names nobody would type.
        Some(
            format!(
                "$__wd_p = '{quoted}'; $__wd_c = [IO.File]::ReadAllText($__wd_p); \
                 Remove-Item -LiteralPath $__wd_p -Force -ErrorAction SilentlyContinue; \
                 . ([scriptblock]::Create($__wd_c)) \
                 # {head} {tail}\n"
            )
            .into_bytes(),
        )
    }

    /// Remove a file that was written but never handed to the shell - an
    /// error path, or a shell closed before its payload went out.
    ///
    /// A file that *was* handed over is left alone on purpose. The line
    /// that reads it deletes it, and deleting it from here would race that:
    /// a `ShellClose` following the payload closely enough would pull the
    /// script out from under a PowerShell that had not read it yet, turning
    /// a slow command into `ReadAllText: file not found`. Anything left
    /// behind by a crash is swept by [`vacuum_stale_spills`] at startup.
    fn cleanup(&self) {
        if self.handed_over.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Pump `ShellInput` into the child's stdin, one line at a time.
///
/// Input is reassembled before it is written because of
/// [`PowerShellSpill`]: the decision "is this line long enough to spill"
/// can only be made once the line is whole. A burst from
/// `shell_input_packets` is a run of `MAX_PAYLOAD`-sized packets closed by
/// a shorter one, so the short packet - a single keystroke included - is
/// what releases the buffer; [`BURST_GRACE`] only covers a burst whose
/// length is an exact multiple of `MAX_PAYLOAD`.
///
/// A line that had to be written in pieces anyway (grace expired mid-line)
/// is never spilled afterwards: half of it is already in the shell, and
/// dot-sourcing the rest would glue a `. 'file'` onto a half-typed
/// statement. Seen live 2026-09-11 when the release trigger was a plain
/// 50 ms timer: PowerShell echoed the mangled line back and `wd --exec`
/// timed out.
fn writer_thread_pipe<W: Write>(
    mut stdin: W,
    rx: mpsc::Receiver<ShellInput>,
    spill: Option<PowerShellSpill>,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut state = WriterState::new();
    loop {
        let input = if buf.is_empty() {
            match rx.recv() {
                Ok(v) => Some(v),
                Err(_) => break,
            }
        } else {
            match rx.recv_timeout(BURST_GRACE) {
                Ok(v) => Some(v),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        match input {
            Some(ShellInput::Data(data)) => {
                let burst_continues = data.len() >= MAX_PAYLOAD;
                buf.extend_from_slice(&data);
                // A full packet means more is coming, and that is the only
                // thing worth reading into it.
                //
                // This used to also release the buffer when a full packet
                // happened to end on a newline, on the theory that a burst
                // closing exactly on a `MAX_PAYLOAD` boundary would end
                // there. For a multi-line payload that is wrong almost
                // every time: with a line break every few bytes, most
                // packets end on one, so the very first packet released the
                // buffer and the rest of the command followed it raw. Live
                // 2026-09-11 a 39 KB, 3000-line script then never ran at
                // all - PowerShell sat on the fragments and the run died at
                // its timeout. The exact-multiple case is what
                // [`BURST_GRACE`] is for.
                if burst_continues {
                    continue;
                }
            }
            // Flush first: the shell is being closed, not abandoned, and a
            // buffered tail is still input the caller asked us to deliver.
            Some(ShellInput::Close) => {
                let _ = write_buffered(&mut stdin, &mut buf, spill.as_ref(), &mut state);
                break;
            }
            // Grace expired: hand over what we have rather than stall.
            None => {}
        }
        if write_buffered(&mut stdin, &mut buf, spill.as_ref(), &mut state).is_err() {
            break;
        }
    }
    if let Some(s) = spill.as_ref() {
        s.cleanup();
    }
    // stdin dropped here → child sees EOF
}

/// Write every complete line in `buf`, then whatever tail is left, and
/// clear it. `state` carries what has to survive between bursts: whether a
/// line is still half-written, and whether the shell is still on its very
/// first line (see [`WriterState::spill_allowed`]).
fn write_buffered<W: Write>(
    stdin: &mut W,
    buf: &mut Vec<u8>,
    spill: Option<&PowerShellSpill>,
    state: &mut WriterState,
) -> std::io::Result<()> {
    let split = buf
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |end| end + 1);

    // A payload is spilled *whole* or not at all, and "whole" means every
    // complete line the burst brought, not just its first one.
    //
    // `format_command` keeps the caller's own line breaks, so a multi-line
    // command - a here-string, an `if` block, a script pasted as-is - is
    // several lines of one statement. Moving only the first into a script
    // file would leave that file unparseable and send the rest of the block
    // to the shell without its opening. Moving all of them keeps the
    // statement intact, and multi-line is exactly the case that needs the
    // spill most: PowerShell parses a long line in super-linear time either
    // way (48 KB took 14.8 s live, 2026-09-11).
    //
    // An unterminated tail still blocks it: that tail is the head of a
    // statement whose rest has not arrived, and half a statement in a file
    // is the same broken thing one line at a time.
    let spillable =
        state.spill_allowed && !state.mid_line && split == buf.len() && split > SPILL_THRESHOLD;
    let rewritten = spill
        .filter(|_| spillable)
        .and_then(|s| s.dot_source_block(&buf[..split]));
    if let Some(line) = rewritten {
        write_flush(stdin, &line)?;
        state.spill_allowed = false;
        state.mid_line = false;
        buf.clear();
        return Ok(());
    }
    for line in buf[..split].split_inclusive(|&b| b == b'\n') {
        write_flush(stdin, line)?;
        state.spill_allowed = false;
    }
    if split > 0 {
        state.mid_line = false;
    }
    if split < buf.len() {
        write_flush(stdin, &buf[split..])?;
        state.mid_line = true;
        state.spill_allowed = false;
    }
    buf.clear();
    Ok(())
}

/// What the writer has to remember between bursts.
#[derive(Default)]
struct WriterState {
    /// The tail of a partly-written line is still outstanding, so the next
    /// piece completes a line that is already half inside the shell.
    mid_line: bool,
    /// Only the very first line a pipe shell ever receives may be spilled.
    ///
    /// That single line is the whole of a `wd --exec` run: the IPC handler
    /// opens a shell, sends one payload and closes it again. Everything
    /// after it belongs to some other conversation and must travel
    /// untouched, which rules out two failures at once. With `--ssh` the
    /// first line is `ssh -tt <host>` and the payload that follows is bash
    /// for the *remote* machine - rewriting that into `. ([scriptblock]…)`
    /// over a Windows path would ship nonsense down the tunnel. And two
    /// long lines in one shell would otherwise share one spill file, where
    /// the second write could land before PowerShell has read the first.
    spill_allowed: bool,
}

impl WriterState {
    fn new() -> Self {
        Self {
            mid_line: false,
            spill_allowed: true,
        }
    }
}

fn write_flush<W: Write>(stdin: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    stdin.write_all(bytes)?;
    stdin.flush()
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

    /// Drive `write_buffered` the way the writer thread does, one burst at
    /// a time, and report what reached the shell.
    fn feed(bursts: &[&[u8]], spill: Option<&PowerShellSpill>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = Vec::new();
        let mut state = WriterState::new();
        for b in bursts {
            buf.extend_from_slice(b);
            write_buffered(&mut out, &mut buf, spill, &mut state).unwrap();
        }
        out
    }

    #[test]
    fn short_lines_pass_through_untouched() {
        let spill = test_spill("short");
        let got = feed(&[b"echo one\n", b"echo two\n"], Some(&spill));
        assert_eq!(got, b"echo one\necho two\n");
        spill.cleanup();
    }

    #[test]
    fn a_long_line_is_replaced_by_a_dot_source() {
        let spill = test_spill("long");
        let long = format!("$x = '{}'\n", "a".repeat(SPILL_THRESHOLD * 2));
        let got = feed(&[long.as_bytes()], Some(&spill));
        let got = String::from_utf8(got).unwrap();
        assert!(
            got.starts_with("$__wd_p = '"),
            "got: {:?}",
            &got[..40.min(got.len())]
        );
        assert!(
            got.len() < 900,
            "the fed line must stay short: {}",
            got.len()
        );
        assert_eq!(
            std::fs::read(&spill.path).unwrap()[3..],
            long.as_bytes()[..long.len() - 1]
        );
        spill.cleanup();
    }

    #[test]
    fn a_line_split_across_bursts_is_still_spilled_once_whole() {
        // What the wire actually does: MAX_PAYLOAD-sized packets, then a
        // short one closing the burst. Only the closing packet releases it.
        let spill = test_spill("split");
        let long = format!("$x = '{}'\n", "a".repeat(MAX_PAYLOAD * 3));
        let bytes = long.as_bytes();
        let mut bursts: Vec<&[u8]> = bytes.chunks(MAX_PAYLOAD).collect();
        // Sanity: every chunk but the last is a full packet.
        assert!(bursts.len() > 3 && bursts.last().unwrap().len() < MAX_PAYLOAD);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        let mut state = WriterState::new();
        let last = bursts.pop().unwrap();
        for b in bursts {
            buf.extend_from_slice(b);
            // Full packet: the thread keeps buffering, nothing is written.
            assert!(b.len() >= MAX_PAYLOAD);
        }
        buf.extend_from_slice(last);
        write_buffered(&mut out, &mut buf, Some(&spill), &mut state).unwrap();
        let got = String::from_utf8(out).unwrap();
        assert!(
            got.starts_with("$__wd_p = '"),
            "got: {:?}",
            &got[..40.min(got.len())]
        );
        assert_eq!(
            std::fs::read(&spill.path).unwrap().len(),
            bytes.len() - 1 + 3
        );
        spill.cleanup();
    }

    #[test]
    fn only_the_first_line_of_a_shell_is_ever_spilled() {
        // `wd --exec --ssh` sends `ssh -tt host` first and the payload for
        // the *remote* bash second. Rewriting that second line into a
        // PowerShell script block over a Windows path would ship nonsense
        // down the tunnel, so nothing after the first line is touched.
        let spill = test_spill("first");
        let long = format!("{}\n", "b".repeat(SPILL_THRESHOLD * 2));
        let got = feed(&[b"ssh -tt prod\n", long.as_bytes()], Some(&spill));
        assert_eq!(got, format!("ssh -tt prod\n{long}").into_bytes());
        assert!(!spill.path.exists(), "nothing must be written");
    }

    #[test]
    fn a_second_long_line_never_reuses_the_spill_file() {
        // Two spills would share one path, and the second write could land
        // before PowerShell has read the first.
        let spill = test_spill("reuse");
        let first = format!("$a = '{}'\n", "a".repeat(SPILL_THRESHOLD * 2));
        let second = format!("$b = '{}'\n", "c".repeat(SPILL_THRESHOLD * 2));
        let got =
            String::from_utf8(feed(&[first.as_bytes(), second.as_bytes()], Some(&spill))).unwrap();
        let lines: Vec<&str> = got.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("$__wd_p = '"),
            "first: {:?}",
            &lines[0][..40]
        );
        assert_eq!(lines[1], second.trim_end(), "second line must be verbatim");
        spill.cleanup();
    }

    #[test]
    fn the_fingerprint_carries_both_compress_markers() {
        // In --compress mode the runner only recognises its own echo when
        // the line holds the READY marker *and* the DONE one; with just the
        // tail it reads the echo as a finished sentinel.
        let spill = test_spill("markers");
        let uuid = "1234abcd-0000-0000-0000-00000000ffff";
        let line = format!(
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8; Write-Output \"__WD_READY_{uuid}__\"; {} Write-Output \"__WD_DONE_{uuid}__0\"\n",
            "$x = 1; ".repeat(2000)
        );
        let fed = String::from_utf8(
            spill
                .dot_source_block(line.as_bytes())
                .expect("spill written"),
        )
        .unwrap();
        assert!(
            fed.contains(&format!("__WD_READY_{uuid}__")),
            "READY missing"
        );
        assert!(
            fed.contains(&format!("__WD_DONE_{uuid}__0")),
            "DONE missing"
        );
        spill.cleanup();
    }

    #[test]
    fn a_line_already_half_written_is_never_spilled() {
        // Regression: the first release trigger was a plain timer, so a
        // long line could be cut in half; spilling the tail glued a
        // `. 'file'` onto a half-typed statement and PowerShell choked.
        let spill = test_spill("half");
        let head = format!("$x = '{}", "a".repeat(SPILL_THRESHOLD * 2));
        let tail = format!("{}'\n", "b".repeat(SPILL_THRESHOLD * 2));
        let got = feed(&[head.as_bytes(), tail.as_bytes()], Some(&spill));
        assert_eq!(got, format!("{head}{tail}").into_bytes());
        spill.cleanup();
    }

    #[test]
    fn a_tail_without_a_newline_reaches_the_shell() {
        // Interactive-ish use: bytes with no terminator must not be held.
        let spill = test_spill("tail");
        let got = feed(&[b"partial"], Some(&spill));
        assert_eq!(got, b"partial");
        spill.cleanup();
    }

    fn test_spill(tag: &str) -> PowerShellSpill {
        PowerShellSpill {
            path: std::env::temp_dir().join(format!(
                "wiredesk-spill-{tag}-{}-{:?}.ps1",
                std::process::id(),
                std::thread::current().id()
            )),
            handed_over: std::sync::atomic::AtomicBool::new(false),
        }
    }

    #[test]
    fn spill_only_applies_to_powershell() {
        assert!(PowerShellSpill::for_argv(&["powershell.exe".into()]).is_some());
        assert!(PowerShellSpill::for_argv(&["pwsh".into()]).is_some());
        assert!(
            PowerShellSpill::for_argv(&["C:\\WINDOWS\\System32\\PowerShell.exe".into()]).is_some()
        );
        assert!(PowerShellSpill::for_argv(&["cmd.exe".into()]).is_none());
        assert!(PowerShellSpill::for_argv(&["/bin/bash".into()]).is_none());
        assert!(PowerShellSpill::for_argv(&[]).is_none());
    }

    #[test]
    fn spilled_line_dot_sources_a_bom_utf8_script() {
        let spill = test_spill("bom");
        let line = b"Get-Date; \"__WD_DONE_abc__$LASTEXITCODE\"\n";
        let fed = String::from_utf8(spill.dot_source_block(line).expect("spill written")).unwrap();

        let on_disk = std::fs::read(&spill.path).unwrap();
        assert_eq!(&on_disk[..3], &[0xEF, 0xBB, 0xBF], "UTF-8 BOM missing");
        assert_eq!(&on_disk[3..], &line[..line.len() - 1]);

        let quoted = spill.path.to_string_lossy().to_string();
        assert!(
            fed.contains(&format!("$__wd_p = '{quoted}'")),
            "fed: {fed:?}"
        );
        // The file is read and deleted before the command runs: the shell
        // is killed as soon as the sentinel appears, and a trailing delete
        // loses that race.
        let read_at = fed.find("ReadAllText").expect("fed: {fed:?}");
        let del_at = fed.find("Remove-Item").expect("fed: {fed:?}");
        let run_at = fed.find("scriptblock]::Create").expect("fed: {fed:?}");
        assert!(read_at < del_at && del_at < run_at, "fed: {fed:?}");
        assert!(fed.ends_with('\n'), "fed line must be submitted");
        // The echo of this line has to stay recognisable to the caller.
        assert!(
            fed.contains("__WD_DONE_abc__$LASTEXITCODE"),
            "fingerprint missing: {fed:?}"
        );

        // What reaches stdin stays short no matter how long the command is.
        // A second spill needs its own file: a shell only ever spills once,
        // and `create_new` refuses to write over an existing path.
        let spill2 = test_spill("bom2");
        let huge = format!("$x = {}\n", "a".repeat(64 * 1024));
        let fed_huge = spill2
            .dot_source_block(huge.as_bytes())
            .expect("spill written");
        assert!(
            fed_huge.len() < 2 * quoted.len() + 2 * ECHO_FINGERPRINT + 160,
            "fed {} bytes",
            fed_huge.len()
        );
        assert_eq!(
            std::fs::read(&spill2.path).unwrap().len(),
            huge.len() - 1 + 3
        );
        let _ = std::fs::remove_file(&spill2.path);

        // `cleanup` deliberately leaves a script that was handed over:
        // the line that reads it deletes it, and racing that would pull
        // the file out from under a PowerShell still parsing it.
        spill.cleanup();
        assert!(
            spill.path.exists(),
            "a handed-over script is PowerShell's to delete"
        );
        let _ = std::fs::remove_file(&spill.path);
    }

    #[test]
    fn the_vacuum_takes_old_spill_scripts_and_leaves_everything_else() {
        let dir = std::env::temp_dir();
        let tag = format!("{}-{:?}", std::process::id(), std::thread::current().id());
        // Shaped like ours and old enough: goes.
        let old = dir.join(format!("wiredesk-exec-{tag}-vacuum.ps1"));
        std::fs::write(&old, b"secret-looking command").unwrap();
        // Shaped like ours but fresh: another instance may be using it.
        let fresh = dir.join(format!("wiredesk-exec-{tag}-fresh.ps1"));
        std::fs::write(&fresh, b"in use").unwrap();
        // Not ours at all.
        let alien = dir.join(format!("something-else-{tag}.ps1"));
        std::fs::write(&alien, b"not ours").unwrap();

        // Age the first one past the threshold.
        std::thread::sleep(Duration::from_millis(60));
        vacuum_stale_spills(Duration::from_millis(50));

        assert!(!old.exists(), "an abandoned script must be removed");
        // `fresh` and `alien` were written at the same moment, so only the
        // name tells them apart from `old` - run the vacuum with a
        // threshold nothing can have reached to prove the name filter.
        vacuum_stale_spills(Duration::from_secs(3600));
        assert!(alien.exists(), "files that are not ours must be left alone");

        let _ = std::fs::remove_file(&fresh);
        let _ = std::fs::remove_file(&alien);
    }

    #[test]
    fn a_path_that_already_exists_is_never_written_through() {
        // The host can run elevated, so an unelevated process of the same
        // user must not be able to aim that write at a file of its
        // choosing by planting a link at the path first. Refusing is safe:
        // the command then travels the ordinary way.
        let spill = test_spill("planted");
        std::fs::write(&spill.path, b"planted by someone else").unwrap();
        let long = format!("$x = '{}'\n", "a".repeat(SPILL_THRESHOLD * 2));
        assert!(
            spill.dot_source_block(long.as_bytes()).is_none(),
            "an existing path must not be overwritten"
        );
        assert_eq!(
            std::fs::read(&spill.path).unwrap(),
            b"planted by someone else",
            "the planted content must be untouched"
        );
        let _ = std::fs::remove_file(&spill.path);
    }

    #[test]
    fn spill_paths_are_not_guessable() {
        // Two shells in one process must not collide, and neither should
        // be predictable from the process id alone.
        let a = PowerShellSpill::for_argv(&["powershell.exe".into()]).unwrap();
        let b = PowerShellSpill::for_argv(&["powershell.exe".into()]).unwrap();
        assert_ne!(a.path, b.path);
        let name = a.path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("wiredesk-exec-"), "{name}");
        assert!(name.ends_with(".ps1"), "{name}");
        // pid + '-' + 32 hex + ".ps1" - the random half is what makes it
        // unguessable, so it has to actually be there.
        assert!(
            name.trim_end_matches(".ps1").len() >= 32 + 2,
            "no random component: {name}"
        );
    }

    #[test]
    fn an_unused_spill_file_is_cleaned_up() {
        // The other half of the same rule: a script that never reached the
        // shell has nobody to delete it, so `cleanup` must.
        let spill = test_spill("unused");
        std::fs::write(&spill.path, b"leftover").unwrap();
        spill.cleanup();
        assert!(!spill.path.exists(), "an unused script must be removed");
    }

    #[test]
    fn a_control_character_never_reaches_the_fed_line() {
        // A bare CR in the middle of a command would end the trailing
        // comment and turn whatever follows into its own statement.
        let spill = test_spill("ctrl");
        let line = format!(
            "Get-Date; \r evil-here; \"__WD_DONE_z__$rc\"{}\n",
            "x".repeat(5000)
        );
        let fed = String::from_utf8(
            spill
                .dot_source_block(line.as_bytes())
                .expect("spill written"),
        )
        .unwrap();
        assert_eq!(fed.matches('\n').count(), 1, "one line only: {fed:?}");
        assert!(!fed[..fed.len() - 1].contains('\r'), "fed: {fed:?}");
        // The command text itself is untouched in the script.
        assert!(std::fs::read(&spill.path).unwrap().ends_with(b"x"));
        spill.cleanup();
    }

    /// Drive the real writer loop the way the wire does: `ShellInput`
    /// packets of `MAX_PAYLOAD` bytes with a short one closing the burst.
    fn feed_packets(payload: &[u8], spill: Option<PowerShellSpill>) -> Vec<u8> {
        let (tx, rx) = mpsc::channel();
        for chunk in payload.chunks(MAX_PAYLOAD) {
            tx.send(ShellInput::Data(chunk.to_vec())).unwrap();
        }
        drop(tx);
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        writer_thread_pipe(Sink(std::sync::Arc::clone(&out)), rx, spill);
        let v = out.lock().unwrap().clone();
        v
    }

    #[test]
    fn a_burst_of_full_packets_is_not_released_by_a_newline() {
        // A multi-line payload has a line break every few bytes, so most
        // of its `MAX_PAYLOAD` packets end on one. Treating that as the end
        // of the burst released the buffer after the very first packet and
        // sent the rest of the command raw behind it - live 2026-09-11 a
        // 39 KB script then never ran at all.
        let spill = test_spill("burst");
        let mut payload = String::from("$LASTEXITCODE=0; try { $v0 = 0\n");
        for i in 1..3000 {
            payload.push_str(&format!("$v{i} = {i}\n"));
        }
        payload
            .push_str("$v2999 } catch { $LASTEXITCODE=1 }; \"__WD_DONE_abc__$LASTEXITCODE\"\n\n");
        assert!(
            payload.len() > 8 * MAX_PAYLOAD,
            "payload must span many packets"
        );

        let got = String::from_utf8(feed_packets(payload.as_bytes(), Some(spill))).unwrap();
        assert_eq!(
            got.lines().count(),
            1,
            "the whole burst must reach the shell as one spilled line: {:?}",
            &got[..got.len().min(120)]
        );
        assert!(got.starts_with("$__wd_p = '"), "got: {:?}", &got[..40]);
    }

    #[test]
    fn a_multi_line_payload_is_spilled_whole() {
        // `format_command` keeps the caller's own line breaks, so a
        // multi-line command is several lines of one statement. Spilling
        // only the first would leave `try {` open in a file that no longer
        // holds its body; spilling all of them keeps the statement intact
        // and still spares PowerShell its super-linear line parser.
        let spill = test_spill("multiline");
        let head = format!(
            "$ErrorActionPreference='Stop'; try {{ '{}'",
            "x".repeat(SPILL_THRESHOLD * 2)
        );
        let payload = format!("{head}\nmore-of-the-block\n}} catch {{ $LASTEXITCODE=1 }}\n");
        let got = String::from_utf8(feed(&[payload.as_bytes()], Some(&spill))).unwrap();

        assert_eq!(
            got.lines().count(),
            1,
            "one line reaches the shell: {got:?}"
        );
        assert!(got.starts_with("$__wd_p = '"), "got: {:?}", &got[..40]);
        let on_disk = String::from_utf8(std::fs::read(&spill.path).unwrap()).unwrap();
        assert_eq!(
            on_disk.trim_start_matches('\u{feff}'),
            payload.trim_end_matches('\n'),
            "the whole block must be in the script, line breaks included"
        );
        let _ = std::fs::remove_file(&spill.path);
    }

    #[test]
    fn an_unterminated_tail_also_blocks_the_spill() {
        // Same reasoning one step earlier: a long first line followed by a
        // tail with no newline is the head of a statement that continues.
        let spill = test_spill("tail");
        let payload = format!("{}\nstill-going", "y".repeat(SPILL_THRESHOLD * 2));
        let got = feed(&[payload.as_bytes()], Some(&spill));
        assert_eq!(got, payload.clone().into_bytes());
        assert!(!spill.path.exists(), "nothing must be written");
    }

    #[test]
    fn a_non_ascii_temp_dir_disables_spilling() {
        // The replacement line carries the path through the same stdin pipe
        // the command travels: we write UTF-8, PowerShell decodes in the
        // console code page. Identical for ASCII, mangled otherwise - and
        // then `ReadAllText` throws on a file name that does not exist.
        assert!(spillable_path(std::path::Path::new(
            r"C:\Users\User\AppData\Local\Temp\wiredesk-exec-1-0.ps1"
        )));
        assert!(!spillable_path(std::path::Path::new(
            "C:\\Users\\\u{41f}\u{430}\u{432}\u{435}\u{43b}\\Temp\\wiredesk-exec-1-0.ps1"
        )));
    }

    #[test]
    fn a_non_ascii_line_is_left_alone() {
        // Reading the text back from a UTF-8 file would fix the decode and
        // break the encode, so non-ASCII keeps the verbatim path.
        let spill = test_spill("utf8");
        let line = "'\u{43f}\u{440}\u{438}\u{432}\u{435}\u{442}'\n".as_bytes();
        assert!(spill.dot_source_block(line).is_none());
        assert!(!spill.path.exists(), "nothing must be written");
    }

    #[test]
    fn spilled_line_drops_crlf_terminator() {
        let spill = PowerShellSpill {
            path: std::env::temp_dir().join(format!(
                "wiredesk-spill-crlf-{}-{:?}.ps1",
                std::process::id(),
                std::thread::current().id()
            )),
            handed_over: std::sync::atomic::AtomicBool::new(false),
        };
        spill
            .dot_source_block(b"Get-Date\r\n")
            .expect("spill written");
        let on_disk = std::fs::read(&spill.path).unwrap();
        assert_eq!(&on_disk[3..], b"Get-Date");
        spill.cleanup();
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn long_line_reaches_a_non_powershell_shell_verbatim() {
        // The writer buffers by line for everyone; a shell that is not
        // PowerShell must still receive the bytes unchanged, however long.
        let mut sh = ShellProcess::spawn("/bin/sh", None).unwrap();
        let payload = "x".repeat(SPILL_THRESHOLD * 3);
        sh.write(format!("echo {payload} | wc -c\nexit\n").into_bytes());

        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match sh
                .events_rx
                .recv_timeout(std::time::Duration::from_millis(200))
            {
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
        // echo adds one newline to the payload.
        assert!(
            s.contains(&format!("{}", SPILL_THRESHOLD * 3 + 1)),
            "output: {s:?}"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn echo_through_shell() {
        // Pipe-mode regression: existing behaviour preserved.
        let mut sh = ShellProcess::spawn("/bin/sh", None).unwrap();
        sh.write(b"echo wiredesk-shell-test\nexit\n".to_vec());

        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match sh
                .events_rx
                .recv_timeout(std::time::Duration::from_millis(200))
            {
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
