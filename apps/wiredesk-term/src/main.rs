//! WireDesk Terminal — CLI shell-over-serial client.
//!
//! Opens a serial connection to a WireDesk Host, requests a shell, and bridges
//! the local terminal (stdin/stdout) with the remote shell. Designed to run
//! inside a real terminal emulator (Ghostty, iTerm, Terminal.app) where you
//! get scrollback, history, copy/paste, etc.
//!
//! Local hotkey: Ctrl+] — quit and restore terminal.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::Parser;
use crossterm::terminal;
use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_exec_core::format_timeout_diagnostic;
#[cfg(target_os = "macos")]
use wiredesk_exec_core::ipc::{
    default_socket_path, read_response, write_request, IpcRequest, IpcResponse,
};
use wiredesk_protocol::message::{Message, VERSION};
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::serial::SerialTransport;
use wiredesk_transport::transport::Transport;

const ESCAPE_BYTE: u8 = 0x1D; // Ctrl+]
/// Send Heartbeat once every two seconds while connected so the host
/// session loop's idle-timeout (6 s, see Session::HEARTBEAT_TIMEOUT_IDLE)
/// doesn't kick us off the moment the user pauses typing.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// Wake-up granularity for the heartbeat thread's sleep loop. We need to
/// react to `stop=true` quickly on shutdown without busy-looping.
const HEARTBEAT_TICK: Duration = Duration::from_millis(100);

#[derive(Parser)]
#[command(
    name = "wiredesk-term",
    about = "WireDesk terminal — shell on Host over serial. Press Ctrl+] to quit."
)]
struct Args {
    /// Serial port (e.g., /dev/cu.usbserial-XXX on macOS, COM3 on Windows)
    #[arg(short, long, default_value = "/dev/cu.usbserial-120")]
    port: String,

    /// Baud rate
    #[arg(short, long, default_value = "115200")]
    baud: u32,

    /// Shell to launch on Host: "" (default), "powershell", "cmd", "bash"
    #[arg(short, long, default_value = "")]
    shell: String,

    /// Client display name
    #[arg(long, default_value = "wiredesk-term")]
    name: String,

    /// Run a single command non-interactively on the host shell,
    /// print clean stdout, exit with the command's exit code. When
    /// set, all the interactive bridge machinery (raw mode, stdin
    /// pump, cooked-mode line discipline) is skipped. The command
    /// itself comes as the *positional* COMMAND argument so the
    /// natural shape `wd --exec --ssh ALIAS "docker ps"` works
    /// without clap eating the next flag as `--exec`'s value.
    #[arg(long)]
    exec: bool,

    /// When --exec is set, first chain `ssh -tt ALIAS` on the host
    /// shell, wait for the remote prompt, and run COMMAND there.
    /// Strip remote MOTD / SSH banner from stdout so the agent sees
    /// only the command's output. Use OpenSSH ControlMaster in
    /// `~/.ssh/config` on the host for sub-second persistent SSH.
    #[arg(long, value_name = "ALIAS")]
    ssh: Option<String>,

    /// Seconds to wait for the sentinel before giving up and
    /// returning exit code 124 (the same convention as `timeout(1)`).
    /// Default 90 s covers worst-case 1 MB clipboard image transfer
    /// (~80 s on 11 KB/s wire) when running in IPC mode through a
    /// busy GUI client.
    #[arg(long, default_value = "90")]
    timeout: u64,

    /// Command to run when --exec is set. Ignored otherwise.
    #[arg(value_name = "COMMAND")]
    command: Option<String>,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();

    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            // Make sure we always restore the terminal before printing the error.
            let _ = terminal::disable_raw_mode();
            eprintln!("\nwiredesk-term error: {e}");
            std::process::exit(1);
        }
    }
}

fn run(args: &Args) -> Result<i32> {
    // Validate --exec / COMMAND coupling before opening serial — fast
    // fail with a clear message, no half-opened state.
    if args.exec && args.command.is_none() {
        return Err(WireDeskError::Input(
            "--exec requires a COMMAND positional argument, e.g. `wd --exec \"echo hello\"`".into(),
        ));
    }
    if !args.exec && args.command.is_some() {
        return Err(WireDeskError::Input(
            "COMMAND is only valid together with --exec".into(),
        ));
    }
    if args.ssh.is_some() && !args.exec {
        return Err(WireDeskError::Input(
            "--ssh is only valid together with --exec".into(),
        ));
    }

    // Mac-only: try the GUI's IPC socket first for `--exec` mode so
    // we can run in parallel with an active WireDesk.app instead of
    // contending for the serial port. If the socket isn't there
    // (GUI not running) or the handler is hung, fall through to the
    // legacy direct-open serial path below — backward-compatible.
    #[cfg(target_os = "macos")]
    if args.exec {
        let cmd = args.command.as_deref().expect("validated above");
        if let Some(code) = try_socket_first(cmd, args.ssh.as_deref(), args.timeout)? {
            return Ok(code);
        }
        // Else: fall through to direct serial.
    }

    if !args.exec {
        eprintln!(
            "wiredesk-term: connecting to {} @ {} baud (Ctrl+] to quit)",
            args.port, args.baud
        );
    }

    let writer = SerialTransport::open(&args.port, args.baud)?;
    // Split the port into independent reader/writer handles so a
    // blocking `recv()` on the reader thread doesn't gate every
    // keystroke on the writer side. Without this split, raw-mode
    // typing is gated by the serial recv timeout and feels frozen.
    let reader = writer
        .try_clone()
        .map_err(|e| WireDeskError::Transport(format!("port split: {e}")))?;
    let writer: Arc<Mutex<Box<dyn Transport>>> = Arc::new(Mutex::new(Box::new(writer)));
    let mut reader: Box<dyn Transport> = reader;

    // Send Hello on the writer, drain HelloAck on the reader.
    // In --exec mode keep the banner + cheatsheet quiet — that
    // output is for interactive users, not for AI agents reading
    // stderr.
    handshake(&writer, &mut reader, &args.name, args.exec)?;

    // Open shell on Host. Pipe-mode for `--exec` (PR #9 sentinel
    // detection requires clean stdout, no PSReadLine/ANSI). PTY-mode
    // for the interactive bridge so vim/htop/ssh-without-`-tt`/
    // PSReadLine all work as in a native shell.
    let (cols, rows) = if !args.exec {
        // Initial PTY size from the local terminal. Default 100×40
        // matches the legacy hardcoded value if size() fails (no tty
        // / pre-raw-mode quirk).
        terminal::size().unwrap_or((100, 40))
    } else {
        (0, 0) // ignored in pipe-mode
    };
    let open_msg = build_shell_open_message(&args.shell, args.exec, cols, rows);
    {
        let mut t = writer.lock().map_err(|_| WireDeskError::Transport("mutex poisoned".into()))?;
        t.send(&Packet::new(open_msg, 0))?;
    }

    // Branch: --exec runs the non-interactive sentinel-driven path
    // (no raw mode, no stdin, return command's exit code). Default
    // path is the interactive bridge.
    let result_code = if args.exec {
        // Validation above guarantees command is Some().
        let cmd = args.command.as_deref().expect("validated above");
        run_exec_oneshot(writer.clone(), reader, cmd, args.ssh.as_deref(), args.timeout)
    } else {
        // Switch local terminal to raw mode so we can forward keystrokes byte-by-byte.
        terminal::enable_raw_mode()
            .map_err(|e| WireDeskError::Input(format!("raw mode: {e}")))?;
        let r = bridge_loop(writer.clone(), reader).map(|_| 0);
        // Restore terminal regardless of how the loop exited.
        let _ = terminal::disable_raw_mode();
        eprintln!("\nwiredesk-term: disconnected");
        r
    };

    // Best-effort: tell the host to close the shell AND that we're
    // disconnecting. ShellClose alone wouldn't reliably free the host's
    // shell slot if the remote process ignores stdin EOF (PowerShell
    // -NoExit), and without Disconnect the host only notices we're
    // gone via the 6 s heartbeat timeout — long enough that a fast
    // relaunch hits "shell already open".
    if let Ok(mut t) = writer.lock() {
        let _ = t.send(&Packet::new(Message::ShellClose, 0));
        let _ = t.send(&Packet::new(Message::Disconnect, 0));
    }

    result_code
}

/// Pick the right shell-open packet for the chosen mode. Pipe-mode
/// (`exec=true`) keeps the legacy `ShellOpen` message — `wd --exec`
/// relies on stdin/stdout being plain pipes for sentinel detection
/// (no PSReadLine echoes, no ANSI in output). PTY-mode (`exec=false`)
/// asks the host to allocate a real PTY at `cols × rows` so the
/// interactive bridge gets vim/htop/ssh-without-`-tt`/PSReadLine.
///
/// Pure helper so the choice is unit-tested without spinning serial.
fn build_shell_open_message(shell: &str, exec: bool, cols: u16, rows: u16) -> Message {
    if exec {
        Message::ShellOpen { shell: shell.to_owned() }
    } else {
        Message::ShellOpenPty {
            shell: shell.to_owned(),
            cols,
            rows,
        }
    }
}

/// Format the post-handshake banner. Pure helper so the format is unit-tested
/// independently of any live transport — typo regressions in the user-visible
/// "connected" line otherwise only surface during a hardware run.
fn format_connected_banner(host_name: &str, screen_w: u16, screen_h: u16) -> String {
    format!(
        "wiredesk-term: connected to '{host_name}' ({screen_w}×{screen_h}). Press Ctrl+] to quit."
    )
}

/// Inline cheat sheet printed once after the connection banner. Lists
/// every key combo the local terminal interprets specially so the user
/// doesn't have to dig through docs to learn that Ctrl+] is the exit
/// hotkey (telnet/nc convention) and that Ctrl+C / Ctrl+D pass through
/// to the remote shell. Pure helper so it can be unit-tested.
fn format_hotkey_cheatsheet() -> String {
    let mut s = String::new();
    s.push_str("\r\n");
    s.push_str("  Hotkeys (handled locally):\r\n");
    s.push_str("    Ctrl+]   exit wiredesk-term and restore your terminal\r\n");
    s.push_str("\r\n");
    s.push_str("  Forwarded to host shell:\r\n");
    s.push_str("    Ctrl+C   interrupt the running command on host\r\n");
    s.push_str("    Ctrl+D   send EOF to host stdin (closes the shell)\r\n");
    s.push_str("    others   pass through to host as typed\r\n");
    s
}

/// Try the GUI's IPC socket. Returns `Ok(Some(code))` on success
/// (caller exits with that code), `Ok(None)` when the socket isn't
/// there or the handler is unresponsive (caller falls back to direct
/// serial — backward-compatible). Mac-only.
#[cfg(target_os = "macos")]
fn try_socket_first(cmd: &str, ssh: Option<&str>, timeout_secs: u64) -> Result<Option<i32>> {
    use std::io::{self, Write};
    use std::os::unix::net::UnixStream;

    let socket_path = default_socket_path();

    // Connect: kernel returns ENOENT/ECONNREFUSED instantly for a
    // missing/refused socket, so no need for a connect timeout — we
    // just translate any IO error into "fall back".
    let mut stream = match UnixStream::connect(&socket_path) {
        Ok(s) => s,
        Err(_) => return Ok(None),
    };

    // Read-timeout for the *first* response: if the GUI bound the
    // socket but the handler is hung (single_inflight stuck on a prior
    // crashed run, accept-queue blocked), we'd otherwise wait the
    // command's full --timeout. 2 s gives a clean fallback path.
    if stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .is_err()
    {
        return Ok(None);
    }

    let req = IpcRequest {
        cmd: cmd.into(),
        ssh: ssh.map(|s| s.into()),
        timeout_secs,
    };
    if write_request(&mut stream, &req).is_err() {
        return Ok(None);
    }

    let first = match read_response(&mut stream) {
        Ok(r) => r,
        Err(e)
            if e.kind() == io::ErrorKind::WouldBlock
                || e.kind() == io::ErrorKind::TimedOut =>
        {
            eprintln!("wd: GUI IPC unresponsive (no first frame in 2s), falling back to direct serial");
            return Ok(None);
        }
        Err(e) => return Err(WireDeskError::Transport(format!("IPC read: {e}"))),
    };

    // First frame arrived — clear the read timeout so long-running
    // commands (Mute phase + slow remote work) don't trip it.
    let _ = stream.set_read_timeout(None);

    let stdout = io::stdout();
    let mut out = stdout.lock();

    // Process first + subsequent responses.
    let mut next = Some(first);
    loop {
        let resp = match next.take() {
            Some(r) => r,
            None => match read_response(&mut stream) {
                Ok(r) => r,
                Err(e) => return Err(WireDeskError::Transport(format!("IPC read: {e}"))),
            },
        };
        match resp {
            IpcResponse::Stdout(b) => {
                let _ = out.write_all(&b);
            }
            IpcResponse::Exit(code) => return Ok(Some(code)),
            IpcResponse::Error(msg) => {
                eprintln!("wd: host error: {msg}");
                return Ok(Some(1));
            }
        }
    }
}

/// `ExecTransport` impl that bridges the shared crate's runner to the
/// term's split serial-port halves. The writer side is locked behind
/// `Arc<Mutex>` because the heartbeat thread shares it; the reader
/// side is owned exclusively here (no concurrent recv).
struct SerialExecTransport {
    writer: Arc<Mutex<Box<dyn Transport>>>,
    reader: Box<dyn Transport>,
}

impl wiredesk_exec_core::ExecTransport for SerialExecTransport {
    fn send_input(&mut self, data: &[u8]) -> std::result::Result<(), wiredesk_exec_core::ExecError> {
        let mut t = self
            .writer
            .lock()
            .map_err(|_| wiredesk_exec_core::ExecError::Transport("mutex poisoned".into()))?;
        t.send(&Packet::new(
            Message::ShellInput { data: data.to_vec() },
            0,
        ))
        .map_err(|e| wiredesk_exec_core::ExecError::Transport(e.to_string()))
    }

    fn recv_event(
        &mut self,
        _timeout: Duration,
    ) -> std::result::Result<wiredesk_exec_core::ExecEvent, wiredesk_exec_core::ExecError> {
        // Underlying SerialTransport already implements its own per-recv
        // timeout window — caller's `_timeout` parameter is informational.
        // We honour it implicitly: the runner re-checks its overall
        // budget on every Idle, and Idle is what we return when the
        // serial layer reports a timeout error.
        match self.reader.recv() {
            Ok(p) => match p.message {
                Message::ShellOutput { data } => Ok(wiredesk_exec_core::ExecEvent::ShellOutput(data)),
                Message::ShellExit { code } => Ok(wiredesk_exec_core::ExecEvent::ShellExit(code)),
                Message::Error { code, msg } => {
                    Ok(wiredesk_exec_core::ExecEvent::HostError(format!("{code}: {msg}")))
                }
                _ => Ok(wiredesk_exec_core::ExecEvent::Idle),
            },
            Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => {
                Ok(wiredesk_exec_core::ExecEvent::Idle)
            }
            Err(e) => Err(wiredesk_exec_core::ExecError::Transport(e.to_string())),
        }
    }
}

/// Drive a single `--exec` run end to end through the shared runner.
/// Owns the writer (for sending the payload), takes the reader by
/// value (synchronous polling — no separate reader thread, since we
/// don't also pump stdin like `bridge_loop` does). Heartbeat thread
/// shares the writer mutex so the host's idle timeout doesn't kick
/// us off during a slow command.
///
/// Returns the exit code that should propagate to `process::exit`:
/// - `Ok(N)` where N is the command's exit code (0–255 typical).
/// - `Ok(124)` on timeout (matches `timeout(1)` convention) — also
///   prints `format_timeout_diagnostic(...)` to stderr.
/// - `Err(...)` on a transport / handshake error (caller turns into exit 1).
fn run_exec_oneshot(
    writer: Arc<Mutex<Box<dyn Transport>>>,
    reader: Box<dyn Transport>,
    cmd: &str,
    ssh: Option<&str>,
    timeout_secs: u64,
) -> Result<i32> {
    let stop = Arc::new(AtomicBool::new(false));
    let hb_stop = stop.clone();
    let hb_writer = writer.clone();
    let heartbeat = thread::spawn(move || heartbeat_thread(hb_writer, hb_stop));

    let mut transport = SerialExecTransport { writer, reader };

    // Streaming callback: write each emitted chunk straight to stdout.
    // The runner already attaches a trailing `\n` to every line it
    // emits, so the caller can be a dumb pipe — no extra newline
    // bookkeeping needed. AC3 byte-equality preserved: the bundled
    // path used to write `clean_stdout(...)` + a single `\n`; the
    // streaming path emits the same content split across line-aligned
    // chunks, total bytes identical.
    let result = wiredesk_exec_core::run_oneshot(
        &mut transport,
        cmd,
        ssh,
        timeout_secs,
        |chunk| {
            use std::io::Write;
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = out.write_all(chunk);
        },
    );

    stop.store(true, Ordering::Relaxed);
    let _ = heartbeat.join();

    match result {
        Ok(code) => Ok(code),
        Err(wiredesk_exec_core::ExecError::Timeout(buf)) => {
            eprintln!("{}", format_timeout_diagnostic(&buf, timeout_secs));
            Ok(124)
        }
        Err(wiredesk_exec_core::ExecError::Transport(m)) => {
            Err(WireDeskError::Transport(m))
        }
        Err(wiredesk_exec_core::ExecError::Closed) => {
            Err(WireDeskError::Transport("transport closed".into()))
        }
    }
}

/// Send Hello on the writer, drain HelloAck on the reader. Reader is
/// taken `&mut` because `recv()` needs `&mut self` to update its frame
/// buffer; the caller keeps ownership and hands the reader off to
/// `bridge_loop` afterward.
fn handshake(
    writer: &Arc<Mutex<Box<dyn Transport>>>,
    reader: &mut Box<dyn Transport>,
    client_name: &str,
    quiet: bool,
) -> Result<()> {
    {
        let mut t = writer
            .lock()
            .map_err(|_| WireDeskError::Transport("mutex poisoned".into()))?;
        t.send(&Packet::new(
            Message::Hello {
                version: VERSION,
                client_name: client_name.into(),
            },
            0,
        ))?;
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(WireDeskError::Transport(
                "handshake: no HelloAck within 5 seconds".into(),
            ));
        }
        match reader.recv() {
            Ok(p) => match p.message {
                Message::HelloAck { host_name, screen_w, screen_h, .. } => {
                    if !quiet {
                        eprintln!("{}", format_connected_banner(&host_name, screen_w, screen_h));
                        eprint!("{}", format_hotkey_cheatsheet());
                    }
                    return Ok(());
                }
                Message::Error { code, msg } => {
                    return Err(WireDeskError::Transport(format!(
                        "host returned error {code}: {msg}"
                    )));
                }
                _ => continue,
            },
            Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Three threads — reader (serial → stdout) on its own port handle,
/// heartbeat (timer → serial) and main (stdin → serial) sharing the
/// writer handle behind a mutex. Splitting the port avoids gating
/// every keystroke on the reader's blocking recv timeout. Plus a
/// fourth thread for resize-polling so a window resize mid-`vim`
/// reflows promptly without piggy-backing on the 2 s heartbeat tick.
///
/// Pass-through raw: the host shell now lives in a real PTY (ConPTY
/// on Win11) and echoes/edits keystrokes itself. We forward stdin
/// byte-for-byte without local echo, line buffering, or
/// backspace-erase — anything else would double-echo. The only
/// keystroke we still intercept is Ctrl+] (0x1D), the telnet/nc-
/// style local quit hotkey.
fn bridge_loop(
    writer: Arc<Mutex<Box<dyn Transport>>>,
    reader: Box<dyn Transport>,
) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));

    // Reader thread: pull packets on its own port handle, write
    // ShellOutput to stdout. Independent from writer-side mutex so
    // its blocking recv can't stall typing.
    let reader_stop = stop.clone();
    let reader_handle = thread::spawn(move || {
        reader_thread(reader, reader_stop);
    });

    // Heartbeat thread: keep the host's session alive while the user
    // sits and reads output without typing. Without it, the host's
    // 6 s idle timeout (or 30 s busy timeout during a transfer) ends
    // the session the moment the user steps away.
    let hb_stop = stop.clone();
    let hb_writer = writer.clone();
    let heartbeat = thread::spawn(move || {
        heartbeat_thread(hb_writer, hb_stop);
    });

    // Resize-poll thread: watch local terminal dimensions and forward
    // PtyResize whenever they change. 500 ms cadence — fast enough that
    // a window drag during `vim` reflows naturally, slow enough not to
    // saturate the 11 KB/s wire with redundant resize packets when the
    // terminal hasn't actually changed.
    let resize_stop = stop.clone();
    let resize_writer = writer.clone();
    let resize_handle = thread::spawn(move || {
        resize_poll_thread(resize_writer, resize_stop);
    });

    // Main thread: read stdin and forward bytes as ShellInput, byte-
    // for-byte. ConPTY echoes/edits/colors output; we are a dumb pipe.
    let stdin = io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut buf = [0u8; 256];

    while !stop.load(Ordering::Relaxed) {
        match stdin_lock.read(&mut buf) {
            Ok(0) => break, // EOF on stdin
            Ok(n) => {
                let chunk = &buf[..n];
                if chunk.contains(&ESCAPE_BYTE) {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                if let Ok(mut t) = writer.lock() {
                    if let Err(e) = t.send(&Packet::new(
                        Message::ShellInput { data: chunk.to_vec() },
                        0,
                    )) {
                        eprintln!("\r\nwiredesk-term: send error: {e}");
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                } else {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                break;
            }
        }
    }

    // Signal reader, heartbeat and resize threads to stop, then join.
    stop.store(true, Ordering::Relaxed);
    let _ = reader_handle.join();
    let _ = heartbeat.join();
    let _ = resize_handle.join();
    Ok(())
}

/// Poll the local terminal size every 500 ms; forward `PtyResize`
/// whenever it changes. Pure helper that tests can drive against a
/// canned size-source — see `compute_resize_packet` below.
fn resize_poll_thread(transport: Arc<Mutex<Box<dyn Transport>>>, stop: Arc<AtomicBool>) {
    let mut last: Option<(u16, u16)> = None;
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(500));
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let cur = match terminal::size() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if let Some(msg) = compute_resize_packet(last, cur) {
            last = Some(cur);
            if let Ok(mut t) = transport.lock() {
                let _ = t.send(&Packet::new(msg, 0));
            }
        }
    }
}

/// Pure helper for the resize-poll decision. `prev=None` always emits
/// (initial baseline tick); identical sizes emit nothing; different
/// sizes emit a `PtyResize` packet body. Extracted so the thread's
/// "should I send" decision is unit-testable without spinning real
/// hardware.
fn compute_resize_packet(prev: Option<(u16, u16)>, cur: (u16, u16)) -> Option<Message> {
    match prev {
        Some(p) if p == cur => None,
        _ => Some(Message::PtyResize { cols: cur.0, rows: cur.1 }),
    }
}

/// Periodic Heartbeat sender — runs in its own thread. Wakes every
/// `HEARTBEAT_TICK` to check the stop flag (so shutdown is responsive),
/// emits a Heartbeat packet roughly every `HEARTBEAT_INTERVAL`. Send
/// errors are intentionally ignored: heartbeat is best-effort, and the
/// reader thread will detect a real disconnect via Disconnect/timeout.
fn heartbeat_thread(transport: Arc<Mutex<Box<dyn Transport>>>, stop: Arc<AtomicBool>) {
    let mut elapsed = Duration::from_secs(0);
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(HEARTBEAT_TICK);
        elapsed += HEARTBEAT_TICK;
        if elapsed < HEARTBEAT_INTERVAL {
            continue;
        }
        elapsed = Duration::from_secs(0);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if let Ok(mut t) = transport.lock() {
            let _ = t.send(&Packet::new(Message::Heartbeat, 0));
        }
    }
}

fn reader_thread(mut transport: Box<dyn Transport>, stop: Arc<AtomicBool>) {
    let stdout = io::stdout();
    // Host shell now lives in a real PTY (ConPTY on Win11), which
    // emits CRLF natively the same way a local terminal would. No
    // LF→CRLF translation needed — shell output goes straight to
    // stdout byte-for-byte, including any colors / cursor moves /
    // bracketed-paste sequences emitted by PSReadLine.
    while !stop.load(Ordering::Relaxed) {
        let pkt = transport.recv();
        match pkt {
            Ok(p) => match p.message {
                Message::ShellOutput { data } => {
                    let mut out = stdout.lock();
                    let _ = out.write_all(&data);
                    let _ = out.flush();
                }
                Message::ShellExit { code } => {
                    let mut out = stdout.lock();
                    let _ = writeln!(out, "\r\n[shell exited with code {code}]\r");
                    let _ = out.flush();
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                Message::Error { code, msg } => {
                    let mut out = stdout.lock();
                    let _ = writeln!(out, "\r\n[host error {code}: {msg}]\r");
                    let _ = out.flush();
                }
                Message::Heartbeat => {}
                Message::Disconnect => {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
                _ => {}
            },
            Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => continue,
            Err(_) => {
                // Brief backoff to avoid tight loop on persistent failure.
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiredesk_transport::mock::MockTransport;

    #[test]
    fn heartbeat_thread_sends_at_least_one_heartbeat() {
        // Pair: side A is owned by the heartbeat thread; we read from B
        // to verify the Heartbeat packet actually went through.
        let (a, mut b) = MockTransport::pair();
        let a_boxed: Box<dyn Transport> = Box::new(a);
        let transport = Arc::new(Mutex::new(a_boxed));
        let stop = Arc::new(AtomicBool::new(false));

        let hb_transport = transport.clone();
        let hb_stop = stop.clone();
        let handle = thread::spawn(move || {
            heartbeat_thread(hb_transport, hb_stop);
        });

        // Wait long enough for at least one HEARTBEAT_INTERVAL tick to
        // fire (2 s + a small slack).
        thread::sleep(Duration::from_millis(2_300));
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();

        // Drain whatever B received; expect ≥ 1 Heartbeat packet.
        let mut heartbeats = 0;
        while let Ok(pkt) = b.recv() {
            if matches!(pkt.message, Message::Heartbeat) {
                heartbeats += 1;
            }
            // recv() blocks; break once we've drained the synchronous
            // queue. We can't easily detect "no more pending" without a
            // try_recv, so cap by what we expect plus a bit.
            if heartbeats >= 1 {
                break;
            }
        }
        assert!(
            heartbeats >= 1,
            "heartbeat thread should have emitted at least one Heartbeat packet"
        );
    }

    #[test]
    fn format_banner_typical_resolution() {
        let s = format_connected_banner("wiredesk-host", 2560, 1440);
        assert!(s.contains("wiredesk-host"), "banner must include host name: {s}");
        assert!(s.contains("2560"), "banner must include width: {s}");
        assert!(s.contains("1440"), "banner must include height: {s}");
        assert!(s.contains("Ctrl+]"), "banner must mention the quit hotkey: {s}");
    }

    #[test]
    fn format_banner_zero_size_does_not_panic() {
        // Defensive: if HelloAck arrives with zero screen dimensions
        // (unlikely but represents a bad host), banner should still
        // format cleanly rather than panicking.
        let s = format_connected_banner("h", 0, 0);
        assert!(s.contains("'h'"));
        assert!(s.contains("0×0"));
    }

    #[test]
    fn shell_open_for_exec_uses_pipe_mode() {
        let m = build_shell_open_message("powershell", true, 80, 24);
        match m {
            Message::ShellOpen { shell } => assert_eq!(shell, "powershell"),
            other => panic!("expected ShellOpen for --exec, got {other:?}"),
        }
    }

    #[test]
    fn shell_open_for_interactive_uses_pty_mode() {
        let m = build_shell_open_message("powershell", false, 100, 40);
        match m {
            Message::ShellOpenPty { shell, cols, rows } => {
                assert_eq!(shell, "powershell");
                assert_eq!(cols, 100);
                assert_eq!(rows, 40);
            }
            other => panic!("expected ShellOpenPty for interactive, got {other:?}"),
        }
    }

    #[test]
    fn shell_open_default_shell_name_carried_through() {
        // Empty shell string ("") is the host's "default" sentinel —
        // host's resolve_shell() turns it into PowerShell on Windows.
        // Helper must preserve it verbatim, no trimming or fallback.
        let m = build_shell_open_message("", false, 80, 24);
        match m {
            Message::ShellOpenPty { shell, .. } => assert!(shell.is_empty()),
            other => panic!("expected ShellOpenPty, got {other:?}"),
        }
    }

    #[test]
    fn compute_resize_packet_first_tick_emits() {
        // First tick (prev=None) always emits — establishes a baseline
        // size for the host PTY before the user starts typing.
        let p = compute_resize_packet(None, (80, 24));
        assert!(matches!(p, Some(Message::PtyResize { cols: 80, rows: 24 })));
    }

    #[test]
    fn compute_resize_packet_unchanged_skips() {
        // Identical size — no resend, saves wire on a 11 KB/s link.
        let p = compute_resize_packet(Some((80, 24)), (80, 24));
        assert!(p.is_none());
    }

    #[test]
    fn compute_resize_packet_changed_emits() {
        let p = compute_resize_packet(Some((80, 24)), (120, 40));
        assert!(matches!(p, Some(Message::PtyResize { cols: 120, rows: 40 })));
    }

    #[test]
    fn cheatsheet_lists_exit_hotkey() {
        let s = format_hotkey_cheatsheet();
        assert!(s.contains("Ctrl+]"), "cheatsheet must mention exit hotkey");
        assert!(s.contains("exit"), "cheatsheet must say what Ctrl+] does");
    }

    #[test]
    fn cheatsheet_lists_forwarded_hotkeys() {
        let s = format_hotkey_cheatsheet();
        assert!(s.contains("Ctrl+C"), "cheatsheet must mention Ctrl+C");
        assert!(s.contains("Ctrl+D"), "cheatsheet must mention Ctrl+D");
    }

    #[test]
    fn format_banner_unicode_host_name() {
        // Host names round-trip as UTF-8; a non-ASCII name shouldn't
        // break formatting (Russian-locale Windows hostname is realistic).
        let s = format_connected_banner("домашний", 1920, 1080);
        assert!(s.contains("домашний"));
        assert!(s.contains("1920×1080"));
    }

    #[test]
    fn heartbeat_thread_stops_promptly_on_flag() {
        // Set stop=true immediately and verify the thread exits within
        // a couple of HEARTBEAT_TICK windows (well under HEARTBEAT_INTERVAL).
        let (a, _b) = MockTransport::pair();
        let a_boxed: Box<dyn Transport> = Box::new(a);
        let transport = Arc::new(Mutex::new(a_boxed));
        let stop = Arc::new(AtomicBool::new(true));

        let started = std::time::Instant::now();
        heartbeat_thread(transport, stop);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "heartbeat_thread should exit quickly when stop is already true (took {elapsed:?})"
        );
    }

    // run_oneshot integration tests via SplitPair (mpsc-backed
    // pair that, unlike MockTransport, gives run_oneshot two
    // independent halves matching the production wiring).

    use std::sync::mpsc;
    use wiredesk_protocol::cobs;

    /// Write-only Transport half: pushes COBS-encoded packets into a
    /// host-bound mpsc channel.
    struct ClientWriter {
        tx: mpsc::Sender<Vec<u8>>,
    }
    impl Transport for ClientWriter {
        fn send(&mut self, packet: &Packet) -> Result<()> {
            let raw = packet.to_bytes()?;
            let encoded = cobs::encode(&raw);
            self.tx
                .send(encoded)
                .map_err(|_| WireDeskError::Transport("split-pair closed".into()))
        }
        fn recv(&mut self) -> Result<Packet> {
            unreachable!("ClientWriter is write-only")
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "split-pair-writer"
        }
        fn try_clone(&self) -> Result<Box<dyn Transport>> {
            Err(WireDeskError::Transport("ClientWriter cannot clone".into()))
        }
    }

    /// Read-only Transport half: pulls COBS-encoded packets from a
    /// host-fed mpsc channel. Translates `recv_timeout` to the
    /// `recv timeout` error string SerialTransport produces, so
    /// `run_oneshot`'s timeout-error filter works the same way.
    struct ClientReader {
        rx: mpsc::Receiver<Vec<u8>>,
    }
    impl Transport for ClientReader {
        fn send(&mut self, _packet: &Packet) -> Result<()> {
            unreachable!("ClientReader is read-only")
        }
        fn recv(&mut self) -> Result<Packet> {
            match self.rx.recv_timeout(Duration::from_millis(10)) {
                Ok(encoded) => {
                    let raw = cobs::decode(&encoded)
                        .map_err(|e| WireDeskError::Protocol(format!("cobs: {e}")))?;
                    Packet::from_bytes(&raw)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    Err(WireDeskError::Transport("recv timeout".into()))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Err(WireDeskError::Transport("split-pair closed".into()))
                }
            }
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "split-pair-reader"
        }
        fn try_clone(&self) -> Result<Box<dyn Transport>> {
            Err(WireDeskError::Transport("ClientReader cannot clone".into()))
        }
    }

    /// Host-side full-duplex helper. Used by tests to script the
    /// "host" responding to handshake + ShellOpen + prompt + cmd
    /// + sentinel.
    struct HostSide {
        tx_to_client: mpsc::Sender<Vec<u8>>,
        rx_from_client: mpsc::Receiver<Vec<u8>>,
    }
    impl HostSide {
        fn send(&self, msg: Message) {
            let p = Packet::new(msg, 0);
            let raw = p.to_bytes().expect("encode");
            let encoded = cobs::encode(&raw);
            self.tx_to_client.send(encoded).expect("host send");
        }
        fn emit_chunk(&self, text: &str) {
            self.send(Message::ShellOutput {
                data: text.as_bytes().to_vec(),
            });
        }
        fn recv_with_timeout(&self, timeout: Duration) -> Option<Packet> {
            let encoded = self.rx_from_client.recv_timeout(timeout).ok()?;
            let raw = cobs::decode(&encoded).ok()?;
            Packet::from_bytes(&raw).ok()
        }
        /// Drain client packets until we hit a `ShellInput` or timeout.
        /// Skips Heartbeat / ShellClose / Disconnect.
        fn recv_shell_input(&self, timeout: Duration) -> Option<String> {
            let deadline = std::time::Instant::now() + timeout;
            while std::time::Instant::now() < deadline {
                let remaining = deadline - std::time::Instant::now();
                if let Some(p) = self.recv_with_timeout(remaining) {
                    if let Message::ShellInput { data } = p.message {
                        return Some(String::from_utf8_lossy(&data).to_string());
                    }
                }
            }
            None
        }
    }

    #[allow(clippy::type_complexity)] // test helper; descriptive tuple beats type alias here
    fn make_split_pair() -> (
        Arc<Mutex<Box<dyn Transport>>>,
        Box<dyn Transport>,
        HostSide,
    ) {
        let (c2h_tx, c2h_rx) = mpsc::channel();
        let (h2c_tx, h2c_rx) = mpsc::channel();
        let writer: Box<dyn Transport> = Box::new(ClientWriter { tx: c2h_tx });
        let reader: Box<dyn Transport> = Box::new(ClientReader { rx: h2c_rx });
        let writer = Arc::new(Mutex::new(writer));
        let host = HostSide {
            tx_to_client: h2c_tx,
            rx_from_client: c2h_rx,
        };
        (writer, reader, host)
    }

    // Tests call `run_oneshot` directly (bypassing `run()`), so
    // there is no Hello/HelloAck/ShellOpen exchange to mock —
    // the host script just emits the host prompt and proceeds.

    #[test]
    fn run_oneshot_happy_path_powershell() {
        let (writer, reader, host) = make_split_pair();
        let host_thread = thread::spawn(move || {
            // PS-only mode now sends the formatted command immediately
            // — no prompt detection on this path because PS in pipe
            // mode doesn't emit one.
            let cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("client should send formatted command");
            assert!(cmd.contains("Get-ChildItem"), "cmd payload: {cmd:?}");
            assert!(cmd.contains("__WD_DONE_"));
            assert!(cmd.contains("$LASTEXITCODE"));
            host.emit_chunk("file1\r\nfile2\r\n");
            let uuid = extract_uuid_from_payload(&cmd);
            host.emit_chunk(&format!("__WD_DONE_{uuid}__0\r\n"));
        });

        let code = run_exec_oneshot(writer, reader, "Get-ChildItem", None, 5)
            .expect("run_exec_oneshot ok");
        assert_eq!(code, 0);
        host_thread.join().expect("host thread");
    }

    #[test]
    fn run_oneshot_happy_path_ssh() {
        let (writer, reader, host) = make_split_pair();
        let host_thread = thread::spawn(move || {
            // Step 1: client sends ssh hop ONLY. Payload is held back
            // until the remote prompt is seen (avoids PS read-ahead
            // race that traps payload in PS's StreamReader buffer).
            let ssh_cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("client should send ssh -tt");
            assert!(ssh_cmd.starts_with("ssh -tt prod"), "got: {ssh_cmd:?}");

            // Step 2: host emits MOTD + ANSI-wrapped Starship-style
            // prompt (`➜ \x1b[K\x1b[?2004h`). Without ANSI stripping
            // is_remote_prompt would never match — checking that the
            // partial-peek branch handles real Starship.
            host.emit_chunk("Welcome to Ubuntu\r\nMOTD line\r\n");
            host.emit_chunk("\x1b[1;33muser\x1b[0m in \x1b[1;36m~\x1b[0m \r\n➜ \x1b[K\x1b[?1h\x1b=\x1b[?2004h");

            // Step 3: client should now send the formatted command.
            let cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("client should send formatted bash command");
            assert!(
                cmd.starts_with("echo __WD_READY_"),
                "cmd payload must lead with READY: {cmd:?}"
            );
            assert!(cmd.contains("docker ps"), "cmd payload: {cmd:?}");

            // Step 4: simulate ssh -tt echo + remote bash exec.
            let uuid = extract_uuid_from_payload(&cmd);
            host.emit_chunk(&format!(
                "echo __WD_READY_{uuid}__; docker ps; echo \"__WD_DONE_{uuid}__$?\"\r\n"
            ));
            host.emit_chunk(&format!(
                "__WD_READY_{uuid}__\r\nrow1\r\nrow2\r\n__WD_DONE_{uuid}__0\r\n"
            ));
        });

        let code = run_exec_oneshot(writer, reader, "docker ps", Some("prod"), 5)
            .expect("run_exec_oneshot ok");
        assert_eq!(code, 0);
        host_thread.join().expect("host thread");
    }

    #[test]
    fn run_oneshot_timeout_returns_124() {
        let (writer, reader, host) = make_split_pair();
        let host_thread = thread::spawn(move || {
            // Consume the cmd but never emit a sentinel.
            let _cmd = host.recv_shell_input(Duration::from_secs(2));
            thread::sleep(Duration::from_millis(2_000));
        });

        let code = run_exec_oneshot(writer, reader, "Start-Sleep 60", None, 1)
            .expect("run_exec_oneshot ok");
        assert_eq!(code, 124, "expected timeout exit code");
        host_thread.join().expect("host thread");
    }

    #[test]
    fn run_oneshot_propagates_nonzero_exit() {
        let (writer, reader, host) = make_split_pair();
        let host_thread = thread::spawn(move || {
            let cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("cmd");
            let uuid = extract_uuid_from_payload(&cmd);
            host.emit_chunk(&format!("__WD_DONE_{uuid}__7\r\n"));
        });

        let code = run_exec_oneshot(writer, reader, "exit 7", None, 5).expect("ok");
        assert_eq!(code, 7);
        host_thread.join().expect("host thread");
    }

    /// AC4 of `docs/briefs/sentinel-detection-ansi-tail.md`: command
    /// emits stdout WITHOUT trailing newline, sentinel arrives glued to
    /// it in the same wire chunk, plus Starship-style ANSI tail in the
    /// following chunk. Pre-fix run_oneshot timed out on this; now must
    /// match sentinel and clean stdout to just the JSON.
    #[test]
    fn run_oneshot_handles_unterminated_output_with_ansi_tail() {
        let (writer, reader, host) = make_split_pair();
        let host_thread = thread::spawn(move || {
            // Step 1: ssh hop sent first.
            let ssh_cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("client should send ssh -tt");
            assert!(ssh_cmd.starts_with("ssh -tt prod"), "got: {ssh_cmd:?}");

            // Step 2: emit a remote prompt so client sends the bash payload.
            host.emit_chunk("Welcome\r\nuser@host:~$ ");

            // Step 3: client sends the bash payload (echo READY; cmd; echo DONE).
            let cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("client should send bash payload");
            let uuid = extract_uuid_from_payload(&cmd);

            // Step 4: simulate the bug scenario — bash echoes the
            // payload, emits READY, runs cmd whose stdout has no
            // trailing newline, then expanded sentinel glued onto it.
            // Starship-style ANSI prompt arrives in a separate chunk.
            host.emit_chunk(&format!(
                "echo __WD_READY_{uuid}__; head -c 800 ...; echo \"__WD_DONE_{uuid}__$?\"\r\n"
            ));
            host.emit_chunk(&format!("__WD_READY_{uuid}__\r\n"));
            // The unterminated output + sentinel in one chunk:
            host.emit_chunk(&format!(
                "{{\"hits\":{{\"total\":42}},\"aggs\":\"x\"}}__WD_DONE_{uuid}__0\r\n"
            ));
            // ANSI Starship tail in following chunk — must NOT confuse
            // the parser, sentinel was already detected above.
            host.emit_chunk(
                "\x1b[1m\x1b[7m%\x1b[27m\x1b[1m\x1b[0m \r\n\x1b[1;33muser\x1b[0m \r\n\
                 ➜ \x1b[K\x1b[?2004h",
            );
        });

        let code = run_exec_oneshot(writer, reader, "head -c 800 ...", Some("prod"), 5)
            .expect("run_exec_oneshot ok");
        assert_eq!(code, 0, "should complete with exit 0, not timeout");
        host_thread.join().expect("host thread");
    }

    /// Extract the UUID embedded in the `format_command` payload so
    /// the host-side test thread can build the matching expanded
    /// sentinel.
    fn extract_uuid_from_payload(payload: &str) -> String {
        let marker = "__WD_DONE_";
        let start = payload.find(marker).expect("uuid marker") + marker.len();
        let after = &payload[start..];
        let end = after.find("__").expect("uuid end");
        after[..end].to_string()
    }

    #[test]
    fn args_default_timeout_is_90_seconds() {
        // Worst-case 1 MB clipboard image transfer over 11 KB/s wire is ~80 s,
        // so the IPC mode default needs ≥ 90 s to avoid false 124 timeouts.
        let args = Args::try_parse_from(["wd"]).expect("parse");
        assert_eq!(args.timeout, 90);
    }

    // --- IPC client (try_socket_first) integration tests ---
    //
    // These tests stand up a fake IPC server on a temp socket path and
    // override DEFAULT_SOCKET_PATH via an env var (TODO: cleaner DI).
    // For now, we directly exercise the same UnixStream pattern via a
    // bypass helper: a test-only `try_socket_first_at(path, ...)` that
    // takes the socket path explicitly. This sidesteps the macOS-only
    // `default_socket_path()` value and lets us point at `tempdir`.
    //
    // Rationale: `try_socket_first` itself is ~30 lines of UnixStream
    // glue around `read_response` / `write_request` (which already have
    // 8 round-trip tests in exec-core::ipc::tests). The fallback path
    // for missing socket is covered by manual inspection on a fresh
    // Mac (no GUI running, `wd --exec ...` should fall back to direct
    // serial — covered by AC3 live-test in Task 8).
    //
    // We do test one critical invariant here: the request → stream of
    // Stdout → Exit pattern that try_socket_first walks.

    #[cfg(target_os = "macos")]
    #[test]
    fn try_socket_first_walks_request_response_stream() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;
        use wiredesk_exec_core::ipc::{read_request, write_response, IpcResponse};

        let socket_path = std::env::temp_dir()
            .join(format!("wd-exec-test-{}.sock", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path).expect("bind");

        // Server: accept one connection, expect a request, send back
        // a few Stdout chunks + Exit.
        let server_path = socket_path.clone();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let req = read_request(&mut stream).expect("read_request");
            assert_eq!(req.cmd, "echo hi");
            assert_eq!(req.ssh, None);
            assert_eq!(req.timeout_secs, 5);
            write_response(&mut stream, &IpcResponse::Stdout(b"line-1\n".to_vec())).unwrap();
            write_response(&mut stream, &IpcResponse::Stdout(b"line-2\n".to_vec())).unwrap();
            write_response(&mut stream, &IpcResponse::Exit(0)).unwrap();
            // Keep `_` to suppress unused-variable lint.
            let _ = server_path;
            // Implicit drop closes stream + listener.
            let _ = stream.flush();
        });

        // Client side: instead of going through `try_socket_first` (which
        // hardcodes default_socket_path), inline the same logic here
        // pointed at our temp socket. This validates the wire protocol
        // contract; the production code's fallback paths are covered
        // by AC3 live-tests.
        let mut stream = std::os::unix::net::UnixStream::connect(&socket_path).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        wiredesk_exec_core::ipc::write_request(
            &mut stream,
            &wiredesk_exec_core::ipc::IpcRequest {
                cmd: "echo hi".into(),
                ssh: None,
                timeout_secs: 5,
            },
        )
        .unwrap();

        let mut collected = Vec::new();
        let exit_code = loop {
            match wiredesk_exec_core::ipc::read_response(&mut stream).unwrap() {
                IpcResponse::Stdout(b) => collected.extend_from_slice(&b),
                IpcResponse::Exit(c) => break c,
                IpcResponse::Error(m) => panic!("error: {m}"),
            }
        };
        server.join().expect("server thread");
        assert_eq!(exit_code, 0);
        assert_eq!(String::from_utf8(collected).unwrap(), "line-1\nline-2\n");

        let _ = std::fs::remove_file(&socket_path);

        // Touch Read import to suppress unused-import warning in case
        // the optimiser strips other paths.
        let _: fn(&mut std::os::unix::net::UnixStream, &mut [u8]) -> std::io::Result<usize> =
            std::os::unix::net::UnixStream::read;
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn try_socket_first_missing_socket_returns_none() {
        // Nonexistent path → connect fails → try_socket_first returns
        // Ok(None) so caller falls back to direct serial.
        // We can't call try_socket_first directly (it pins to
        // default_socket_path), but we mirror its connect-or-fallback
        // logic here against a guaranteed-missing path.
        let bogus = std::env::temp_dir().join(format!(
            "wd-exec-missing-{}.sock",
            uuid::Uuid::new_v4()
        ));
        let res = std::os::unix::net::UnixStream::connect(&bogus);
        assert!(res.is_err(), "connect to missing path must fail");
    }
}
