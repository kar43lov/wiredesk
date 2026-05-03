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

    /// Run COMMAND non-interactively on the host shell, print clean
    /// stdout, exit with the command's exit code. When set, all the
    /// interactive bridge machinery (raw mode, stdin pump,
    /// cooked-mode line discipline) is skipped.
    #[arg(long, value_name = "COMMAND")]
    exec: Option<String>,

    /// When --exec is set, first chain `ssh -tt ALIAS` on the host
    /// shell, wait for the remote prompt, and run COMMAND there.
    /// Strip remote MOTD / SSH banner from stdout so the agent sees
    /// only the command's output. Use OpenSSH ControlMaster in
    /// `~/.ssh/config` on the host for sub-second persistent SSH.
    #[arg(long, value_name = "ALIAS")]
    ssh: Option<String>,

    /// Seconds to wait for the sentinel before giving up and
    /// returning exit code 124 (the same convention as `timeout(1)`).
    #[arg(long, default_value = "30")]
    timeout: u64,
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
    if args.exec.is_none() {
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
    handshake(&writer, &mut reader, &args.name, args.exec.is_some())?;

    // Open shell on Host
    {
        let mut t = writer.lock().map_err(|_| WireDeskError::Transport("mutex poisoned".into()))?;
        t.send(&Packet::new(
            Message::ShellOpen { shell: args.shell.clone() },
            0,
        ))?;
    }

    // Branch: --exec runs the non-interactive sentinel-driven path
    // (no raw mode, no stdin, return command's exit code). Default
    // path is the interactive bridge.
    let result_code = if let Some(cmd) = &args.exec {
        run_oneshot(writer.clone(), reader, cmd, args.ssh.as_deref(), args.timeout)
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

/// Convert bare LFs (Unix-style line endings) into CRLF for a raw-mode
/// terminal. The local terminal is in raw mode (no line discipline),
/// so a bare `\n` only moves the cursor down — column stays where it
/// was. SSH'd Linux output, `cat foo.txt` on bash, anything Unix uses
/// bare `\n`, producing the staircase effect. We track `last_was_cr`
/// across chunks so a CRLF that happens to span a chunk boundary
/// isn't expanded to CRCRLF.
///
/// Returns the translated bytes; updates `last_was_cr` to the value
/// after processing the last byte.
fn translate_output_for_terminal(input: &[u8], last_was_cr: &mut bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for &b in input {
        if b == b'\n' && !*last_was_cr {
            out.extend_from_slice(b"\r\n");
        } else {
            out.push(b);
        }
        *last_was_cr = b == b'\r';
    }
    out
}

/// Pop the last whole UTF-8 char from `buf`. Returns `true` if anything
/// was popped, `false` if the buffer was already empty. Required because
/// our cooked-mode line buffer holds raw bytes — popping just one byte
/// would split a Russian "д" (0xD0 0xB4) and corrupt the line. We walk
/// backward over continuation bytes (0b10xxxxxx) until we hit the lead.
fn pop_utf8_char(buf: &mut Vec<u8>) -> bool {
    if buf.is_empty() {
        return false;
    }
    while let Some(&last) = buf.last() {
        buf.pop();
        if (last & 0xC0) != 0x80 {
            return true;
        }
    }
    true
}

/// Which host shell flavour we're targeting when formatting the
/// sentinel-bearing command. The host always runs PowerShell; when
/// `--ssh` chains us to a remote box we typically end up in bash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Bash variant used after `--ssh` lands a remote prompt.
enum ShellKind {
    PowerShell,
    Bash,
}

/// Build the `<command>; <emit-sentinel>` payload for `run_oneshot`.
///
/// PowerShell variant wraps the command in `try { … } catch …` so that
/// a *terminating* error (`Get-Item /nonexistent`, mistyped cmdlet)
/// still falls through to the sentinel-emit. Without that, a
/// terminating error skips the trailing statement and run_oneshot
/// hangs to `--timeout` (exit 124) instead of returning a clean
/// non-zero exit.
///
/// Bash continues past non-zero `exit` of any single command in a
/// `;`-list, so a plain `cmd; echo "<sentinel>"` is enough.
fn format_command(uuid: &uuid::Uuid, kind: ShellKind, cmd: &str) -> String {
    match kind {
        ShellKind::PowerShell => format!(
            "try {{ {cmd} }} catch {{ $LASTEXITCODE = 1 }}; \"__WD_DONE_{uuid}__$LASTEXITCODE\"\r"
        ),
        ShellKind::Bash => format!("{cmd}; echo \"__WD_DONE_{uuid}__$?\"\r"),
    }
}

/// State machine for `run_oneshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // names mirror state semantics intentionally
enum OneShotState {
    /// Waiting for the host PowerShell prompt to appear in the buffer.
    AwaitingHostPrompt,
    /// `--ssh` is set, host prompt already seen, sent `ssh -tt ALIAS`,
    /// now waiting for the remote shell prompt.
    AwaitingRemotePrompt,
    /// Target prompt seen, formatted command sent, looking for the
    /// expanded sentinel line.
    AwaitingSentinel,
}

/// Drive a single `--exec` run end to end. Owns the writer (for sending
/// `ssh`-hop and the formatted command), takes the reader by value
/// (synchronous polling — no separate reader thread, since we don't
/// also pump stdin like `bridge_loop` does). Heartbeat thread shares
/// the writer mutex so the host's idle timeout doesn't kick us off
/// during a slow command.
///
/// Returns the exit code that should propagate to `process::exit`:
/// - `Ok(N)` where N is the command's exit code (0–255 typical).
/// - `Ok(124)` on timeout (matches `timeout(1)` convention).
/// - `Err(...)` on a transport / handshake error (caller turns into exit 1).
fn run_oneshot(
    writer: Arc<Mutex<Box<dyn Transport>>>,
    mut reader: Box<dyn Transport>,
    cmd: &str,
    ssh: Option<&str>,
    timeout_secs: u64,
) -> Result<i32> {
    let stop = Arc::new(AtomicBool::new(false));
    let hb_stop = stop.clone();
    let hb_writer = writer.clone();
    let heartbeat = thread::spawn(move || heartbeat_thread(hb_writer, hb_stop));

    let uuid = uuid::Uuid::new_v4();
    let target_kind = if ssh.is_some() {
        ShellKind::Bash
    } else {
        ShellKind::PowerShell
    };
    let payload = format_command(&uuid, target_kind, cmd);

    let mut state = OneShotState::AwaitingHostPrompt;
    // `pending` is the line-walker scratch — only completed lines get
    // popped out of it. `full_log` accumulates EVERYTHING received so
    // `clean_stdout` at the end has the whole conversation to slice
    // (last prompt → drop, echo'ed sentinel-cmd → drop, expanded
    // sentinel → drop). Without `full_log` we'd lose the prompt/echo
    // delimiters once they're drained.
    let mut pending = String::new();
    let mut full_log = String::new();
    let started = std::time::Instant::now();
    let max_wait = Duration::from_secs(timeout_secs);

    let mut exit_code: Option<i32> = None;

    while started.elapsed() < max_wait {
        match reader.recv() {
            Ok(p) => {
                if let Message::ShellOutput { data } = p.message {
                    let text = String::from_utf8_lossy(&data);
                    pending.push_str(&text);
                    full_log.push_str(&text);
                }
                // Other message types (Heartbeat, etc.) are ignored here.
            }
            Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => {
                // No data this tick — re-check overall timeout below.
            }
            Err(e) => {
                stop.store(true, Ordering::Relaxed);
                let _ = heartbeat.join();
                return Err(e);
            }
        }

        // Walk completed lines (anything terminated by '\n'); leftover
        // partial line stays in `pending`.
        while let Some(nl_idx) = pending.find('\n') {
            let line: String = pending[..nl_idx].trim_end_matches('\r').to_string();
            pending.drain(..=nl_idx);

            match state {
                OneShotState::AwaitingHostPrompt => {
                    if is_powershell_prompt(&line) {
                        if let Some(alias) = ssh {
                            send_text(&writer, &format!("ssh -tt {alias}\r"))?;
                            state = OneShotState::AwaitingRemotePrompt;
                        } else {
                            send_text(&writer, &payload)?;
                            state = OneShotState::AwaitingSentinel;
                        }
                    }
                }
                OneShotState::AwaitingRemotePrompt => {
                    if is_remote_prompt(&line) {
                        send_text(&writer, &payload)?;
                        state = OneShotState::AwaitingSentinel;
                    }
                }
                OneShotState::AwaitingSentinel => {
                    if let Some(code) = parse_sentinel(&line, &uuid) {
                        exit_code = Some(code);
                        break;
                    }
                }
            }
        }

        // CRITICAL: prompts arrive WITHOUT a trailing newline — the
        // shell positions the cursor right after `> ` / `$ ` / `➜ `
        // and waits for input. The line-walker above only sees
        // `\n`-terminated lines, so it would never trigger on a
        // prompt by itself. Inspect the partial leftover in
        // `pending` and treat it as a prompt match if it looks
        // like one. On match we transition state and clear the
        // partial — its bytes have served their purpose.
        match state {
            OneShotState::AwaitingHostPrompt
                if is_powershell_prompt(pending.trim_end()) =>
            {
                if let Some(alias) = ssh {
                    send_text(&writer, &format!("ssh -tt {alias}\r"))?;
                    state = OneShotState::AwaitingRemotePrompt;
                } else {
                    send_text(&writer, &payload)?;
                    state = OneShotState::AwaitingSentinel;
                }
                pending.clear();
            }
            OneShotState::AwaitingRemotePrompt
                if is_remote_prompt(pending.trim_end()) =>
            {
                send_text(&writer, &payload)?;
                state = OneShotState::AwaitingSentinel;
                pending.clear();
            }
            _ => {}
        }

        if exit_code.is_some() {
            break;
        }
    }

    stop.store(true, Ordering::Relaxed);
    let _ = heartbeat.join();

    match exit_code {
        Some(code) => {
            let cleaned = clean_stdout(&full_log, &uuid);
            if !cleaned.is_empty() {
                use std::io::Write;
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let _ = out.write_all(cleaned.as_bytes());
                // Add trailing newline so caller shells see a clean line.
                let _ = out.write_all(b"\n");
            }
            Ok(code)
        }
        None => {
            eprintln!(
                "wiredesk-term: --exec timeout after {}s (no sentinel from host)",
                timeout_secs
            );
            Ok(124)
        }
    }
}

/// Send a text payload as a `ShellInput` packet through the shared
/// writer mutex. Centralises the lock/encode boilerplate so the
/// state-machine code in `run_oneshot` reads cleaner.
fn send_text(writer: &Arc<Mutex<Box<dyn Transport>>>, s: &str) -> Result<()> {
    let mut t = writer
        .lock()
        .map_err(|_| WireDeskError::Transport("mutex poisoned".into()))?;
    t.send(&Packet::new(
        Message::ShellInput { data: s.as_bytes().to_vec() },
        0,
    ))
}

/// Slice the accumulated output buffer down to *just* what `<cmd>`
/// produced. The wire-stream of one `run_oneshot` execution roughly
/// looks like:
///
/// ```text
/// [host MOTD / SSH banner / pre-prompt noise]
/// PS C:\…> | ➜ | user@host$         <- last prompt before our cmd
/// [echoed command with sentinel format string]   <- only in --ssh path
/// [actual stdout of <cmd>]
/// __WD_DONE_<uuid>__<exit_code>      <- expanded sentinel
/// ```
///
/// We:
///  1. Find the *last* prompt-line (host or remote) before the
///     sentinel; slice everything after it.
///  2. Find the sentinel line; slice everything before it.
///  3. If a line in between contains literal `__WD_DONE_<uuid>__$`
///     (i.e. the *unexpanded* sentinel — host echoing our stdin),
///     drop it. Asymmetry: PS host in pipe-mode without PSReadLine
///     does NOT echo stdin → no echoed line in PS-only path → this
///     step is a no-op there. With `ssh -tt` the remote shell DOES
///     echo, and the format string surfaces.
///  4. Trim trailing newlines for tidy stdout.
///
/// Pure helper, returns owned `String`.
fn clean_stdout(buf: &str, uuid: &uuid::Uuid) -> String {
    let lines: Vec<&str> = buf.split('\n').collect();

    // Find sentinel line index (first match).
    let sentinel_idx = lines
        .iter()
        .position(|l| parse_sentinel(l, uuid).is_some());
    let upper = sentinel_idx.unwrap_or(lines.len());

    // Find last prompt line strictly *before* the sentinel.
    let prompt_idx = lines[..upper]
        .iter()
        .rposition(|l| is_powershell_prompt(l) || is_remote_prompt(l));
    let lower = prompt_idx.map(|i| i + 1).unwrap_or(0);

    // Stdin-echo line literal — present in --ssh path because remote
    // shell echoes stdin. Filter it out.
    let echo_marker = format!("__WD_DONE_{uuid}__$");
    let kept: Vec<&str> = lines[lower..upper]
        .iter()
        .copied()
        .filter(|l| !l.contains(&echo_marker))
        .collect();

    let mut out = kept.join("\n");
    // Trim trailing newlines / whitespace lines for tidy display.
    while out.ends_with('\n') || out.ends_with('\r') {
        out.pop();
    }
    out
}

/// Parse a single line for our sentinel marker. Returns `Some(exit_code)`
/// only when the line is **exactly** `__WD_DONE_<our-uuid>__<digits>`,
/// modulo trailing whitespace. Crucially the digit-class match is what
/// disambiguates the *expanded* sentinel (e.g. `__WD_DONE_xxx__0`) from
/// the *stdin echo* of the format-string (e.g. `…__WD_DONE_xxx__$LASTEXITCODE`)
/// — the literal `$LASTEXITCODE` / `$?` won't parse as `i32`.
///
/// UUID is included in the match so a third party emitting a sentinel
/// with a *different* UUID can't fool us.
fn parse_sentinel(line: &str, uuid: &uuid::Uuid) -> Option<i32> {
    let prefix = format!("__WD_DONE_{uuid}__");
    let rest = line.trim().strip_prefix(&prefix)?;
    rest.parse::<i32>().ok()
}

/// `true` when `line` looks like a Windows PowerShell prompt:
/// `PS X:\…> ` or `PS C:\Users\User\path>`. Used by `run_oneshot` to
/// know when the host shell is ready to accept the formatted command.
/// Tolerates trailing whitespace / partial-buffer noise from the wire
/// by trimming first.
fn is_powershell_prompt(line: &str) -> bool {
    let s = line.trim_end();
    if !s.starts_with("PS ") {
        return false;
    }
    // Drive letter check: 4th char must be uppercase ASCII letter, 5th `:`.
    let bytes = s.as_bytes();
    if bytes.len() < 6 {
        return false;
    }
    if !bytes[3].is_ascii_uppercase() || bytes[4] != b':' {
        return false;
    }
    s.ends_with('>')
}

/// `true` when `line` looks like a remote shell prompt (the kind that
/// follows a successful `ssh -tt` hop). Recognises the common endings:
/// `$ ` (plain bash), `# ` (root bash), and Starship's `➜` glyph. Other
/// custom prompts can be added later if needed; the user can also
/// override via a future `--prompt-regex` flag if false-negatives bite.
fn is_remote_prompt(line: &str) -> bool {
    let s = line.trim_end();
    if s.is_empty() {
        return false;
    }
    s.ends_with('$') || s.ends_with('#') || s.ends_with('➜')
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
/// every keystroke on the reader's blocking recv timeout.
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

    // Main thread: read stdin and forward bytes as ShellInput.
    //
    // We run a small "cooked mode" line discipline locally — the host
    // shell (PowerShell pipe-mode without PSReadLine) only echoes the
    // *space* portion of an erase sequence, leaving a trailing
    // visible space and de-syncing the cursor. So instead of
    // forwarding bytes one-by-one we accumulate a line in `line_buf`,
    // local-echo printable bytes ourselves, and only flush the line
    // to host on Enter. Just before the flush we erase the local-
    // echoed characters with `\b \b` × N so the host's own echo paints
    // the line exactly once. Backspace pops a UTF-8 char from the
    // buffer (so Russian д = 2 bytes erases as one keystroke), Ctrl+C
    // / Ctrl+D bypass the buffer and hit the host immediately as
    // signals/EOF.
    let stdin = io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut buf = [0u8; 256];
    let mut line_buf: Vec<u8> = Vec::new();
    // Number of *display cells* currently on screen for line_buf —
    // not byte count. ASCII byte = 1 cell, UTF-8 multi-byte char
    // also typically = 1 cell. We track this so erase-on-flush emits
    // the right number of `\b \b` triples.
    let mut line_cells: usize = 0;

    while !stop.load(Ordering::Relaxed) {
        match stdin_lock.read(&mut buf) {
            Ok(0) => break, // EOF on stdin
            Ok(n) => {
                let chunk = &buf[..n];
                if chunk.contains(&ESCAPE_BYTE) {
                    stop.store(true, Ordering::Relaxed);
                    break;
                }

                // Per-byte processing. We handle Backspace and Enter
                // here without going to the host; printable bytes are
                // both echoed locally and held in line_buf.
                let mut send_now: Option<Vec<u8>> = None;
                let mut local_echo: Vec<u8> = Vec::new();
                let mut byte_idx = 0;
                while byte_idx < chunk.len() {
                    let b = chunk[byte_idx];
                    match b {
                        0x7F => {
                            // Backspace: pop one UTF-8 char from line_buf,
                            // erase locally if anything was popped.
                            if pop_utf8_char(&mut line_buf) {
                                line_cells = line_cells.saturating_sub(1);
                                local_echo.extend_from_slice(b"\x08 \x08");
                            }
                            byte_idx += 1;
                        }
                        b'\r' | b'\n' => {
                            // Enter: erase what we locally echoed so the
                            // host's own echo doesn't double the line,
                            // then flush the line + LF to host.
                            for _ in 0..line_cells {
                                local_echo.extend_from_slice(b"\x08 \x08");
                            }
                            let mut payload = std::mem::take(&mut line_buf);
                            payload.push(b'\n');
                            send_now = Some(payload);
                            line_cells = 0;
                            byte_idx += 1;
                            break;
                        }
                        0x03 | 0x04 => {
                            // Ctrl+C / Ctrl+D — forward immediately,
                            // bypass the line buffer. Buffer keeps its
                            // half-typed state; user can still finish
                            // the line if Ctrl+C was a no-op on host.
                            send_now = Some(vec![b]);
                            byte_idx += 1;
                            break;
                        }
                        _ => {
                            // Append to buffer, echo locally. We assume
                            // ASCII / UTF-8 input — multi-byte starts
                            // here, continuation bytes will fall through
                            // and be appended too on the next iteration.
                            // Cell count is bumped only on a *new char*
                            // (lead byte), not on continuation bytes.
                            let is_continuation = (b & 0xC0) == 0x80;
                            line_buf.push(b);
                            local_echo.push(b);
                            if !is_continuation {
                                line_cells += 1;
                            }
                            byte_idx += 1;
                        }
                    }
                }

                if !local_echo.is_empty() {
                    let mut out = io::stdout().lock();
                    let _ = out.write_all(&local_echo);
                    let _ = out.flush();
                }

                if let Some(payload) = send_now {
                    if let Ok(mut t) = writer.lock() {
                        if let Err(e) = t.send(&Packet::new(
                            Message::ShellInput { data: payload },
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

                // Anything past the byte that triggered send_now (e.g.
                // text typed after Ctrl+C in the same chunk) has not
                // been processed. Realistically stdin reads are aligned
                // to keystrokes, so this is not a hot edge case.
                let _ = byte_idx;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                break;
            }
        }
    }

    // Signal reader and heartbeat threads to stop, then join.
    stop.store(true, Ordering::Relaxed);
    let _ = reader_handle.join();
    let _ = heartbeat.join();
    Ok(())
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
    fn translate_output_bare_lf_becomes_crlf() {
        let mut last_cr = false;
        let s = translate_output_for_terminal(b"line1\nline2\n", &mut last_cr);
        assert_eq!(s, b"line1\r\nline2\r\n");
        assert!(!last_cr);
    }

    #[test]
    fn translate_output_existing_crlf_unchanged() {
        let mut last_cr = false;
        let s = translate_output_for_terminal(b"line1\r\nline2\r\n", &mut last_cr);
        assert_eq!(s, b"line1\r\nline2\r\n");
    }

    #[test]
    fn translate_output_crlf_across_chunk_boundary_no_double_cr() {
        // Chunk 1 ends with \r, chunk 2 starts with \n — must NOT
        // emit \r\r\n.
        let mut last_cr = false;
        let s1 = translate_output_for_terminal(b"line1\r", &mut last_cr);
        assert_eq!(s1, b"line1\r");
        assert!(last_cr);
        let s2 = translate_output_for_terminal(b"\nline2", &mut last_cr);
        assert_eq!(s2, b"\nline2");
        assert!(!last_cr);
    }

    #[test]
    fn translate_output_lone_cr_passes_through() {
        // Some progress-bar UIs use \r alone for "rewrite this line".
        let mut last_cr = false;
        let s = translate_output_for_terminal(b"50%\r100%", &mut last_cr);
        assert_eq!(s, b"50%\r100%");
    }

    #[test]
    fn translate_output_empty_input() {
        let mut last_cr = false;
        let s = translate_output_for_terminal(b"", &mut last_cr);
        assert!(s.is_empty());
        assert!(!last_cr);
    }

    #[test]
    fn pop_utf8_char_on_empty_buffer() {
        let mut buf: Vec<u8> = Vec::new();
        assert!(!pop_utf8_char(&mut buf));
        assert!(buf.is_empty());
    }

    #[test]
    fn pop_utf8_char_pops_single_ascii() {
        let mut buf = b"abc".to_vec();
        assert!(pop_utf8_char(&mut buf));
        assert_eq!(buf, b"ab");
    }

    #[test]
    fn pop_utf8_char_pops_two_byte_cyrillic() {
        // Russian "д" is 0xD0 0xB4 — 2 bytes, 1 char. Popping must
        // remove BOTH bytes, otherwise next typed char + leftover
        // continuation byte produces invalid UTF-8 in the line buffer.
        let mut buf = "abд".as_bytes().to_vec();
        assert_eq!(buf.len(), 4);
        assert!(pop_utf8_char(&mut buf));
        assert_eq!(buf, b"ab");
    }

    #[test]
    fn pop_utf8_char_pops_three_byte_emoji_lead() {
        // "€" is 0xE2 0x82 0xAC — 3 bytes. Pop removes all three.
        let mut buf = "a€".as_bytes().to_vec();
        assert_eq!(buf.len(), 4);
        assert!(pop_utf8_char(&mut buf));
        assert_eq!(buf, b"a");
    }

    #[test]
    fn pop_utf8_char_pops_to_empty() {
        let mut buf = "д".as_bytes().to_vec();
        assert_eq!(buf.len(), 2);
        assert!(pop_utf8_char(&mut buf));
        assert!(buf.is_empty());
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

    // is_powershell_prompt — recognise the host shell prompt so
    // run_oneshot knows when to send the formatted command.

    #[test]
    fn is_powershell_prompt_classic() {
        assert!(is_powershell_prompt("PS C:\\>"));
        assert!(is_powershell_prompt("PS C:\\Users\\User>"));
        assert!(is_powershell_prompt("PS C:\\Users\\User> "));
    }

    #[test]
    fn is_powershell_prompt_other_drives() {
        assert!(is_powershell_prompt("PS D:\\Projects\\foo>"));
        assert!(is_powershell_prompt("PS Z:\\>"));
    }

    #[test]
    fn is_powershell_prompt_rejects_non_prompt() {
        assert!(!is_powershell_prompt(""));
        assert!(!is_powershell_prompt("PS"));
        assert!(!is_powershell_prompt("PS >"));
        assert!(!is_powershell_prompt("bash$"));
        assert!(!is_powershell_prompt("> ls"));
        assert!(!is_powershell_prompt("PS c:\\>")); // lowercase drive — reject
    }

    // is_remote_prompt — recognise the bash/zsh/Starship prompt that
    // shows up after `ssh -tt prod-mup` succeeds.

    #[test]
    fn is_remote_prompt_bash_user() {
        assert!(is_remote_prompt("user@host:~$"));
        assert!(is_remote_prompt("user@host:~$ "));
    }

    #[test]
    fn is_remote_prompt_bash_root() {
        assert!(is_remote_prompt("root@host:/#"));
        assert!(is_remote_prompt("root@host:/# "));
    }

    #[test]
    fn is_remote_prompt_starship() {
        // Starship renders cwd on a separate info-line; the prompt
        // cursor line is just `➜ `. Real-world traces showed both
        // shapes coming over the wire — the bare arrow and the
        // whole prefix-then-arrow on a single line.
        assert!(is_remote_prompt("➜"));
        assert!(is_remote_prompt("➜ "));
        assert!(is_remote_prompt("karlovpg in 🌐 knd02 in ~ ➜ "));
    }

    // format_command — payload generation for both shell flavours.

    #[test]
    fn format_command_powershell_wraps_in_try_catch() {
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::PowerShell, "Get-ChildItem");
        assert!(
            s.contains("try { Get-ChildItem }"),
            "PS payload must wrap cmd in try/catch: {s}"
        );
        assert!(
            s.contains("catch { $LASTEXITCODE = 1 }"),
            "PS payload must set $LASTEXITCODE on terminating error: {s}"
        );
        assert!(
            s.contains("$LASTEXITCODE"),
            "PS sentinel must use $LASTEXITCODE: {s}"
        );
        assert!(s.ends_with('\r'), "payload must end with CR for host stdin: {s}");
    }

    #[test]
    fn format_command_bash_appends_sentinel() {
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::Bash, "docker ps");
        assert!(s.starts_with("docker ps; echo "), "bash payload prefix: {s}");
        assert!(
            s.contains("$?"),
            "bash sentinel must reference $?: {s}"
        );
        assert!(
            !s.contains("$LASTEXITCODE"),
            "bash payload must NOT use $LASTEXITCODE: {s}"
        );
        assert!(s.ends_with('\r'));
    }

    #[test]
    fn format_command_uuid_in_payload() {
        let uuid_a = uuid::Uuid::nil();
        let uuid_b = uuid::Uuid::from_u128(0x1234_5678_90ab_cdef_1234_5678_90ab_cdef);
        let a1 = format_command(&uuid_a, ShellKind::Bash, "ls");
        let a2 = format_command(&uuid_a, ShellKind::Bash, "ls");
        let b = format_command(&uuid_b, ShellKind::Bash, "ls");
        assert_eq!(a1, a2, "same UUID + same args should be deterministic");
        assert_ne!(a1, b, "different UUID → different payload");
        assert!(a1.contains(&uuid_a.to_string()));
        assert!(b.contains(&uuid_b.to_string()));
    }

    // parse_sentinel — expand-sentinel vs stdin-echo disambiguation.

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
            host.emit_chunk("PS C:\\Users\\User>\r\n");
            // Wait for the formatted command from the client.
            let cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("client should send formatted command");
            assert!(cmd.contains("Get-ChildItem"), "cmd payload: {cmd:?}");
            assert!(cmd.contains("__WD_DONE_"));
            assert!(cmd.contains("$LASTEXITCODE"));
            // Echo the command back (PS pipe-mode actually doesn't,
            // but echoing here is a no-op: clean_stdout will simply
            // not find the literal echo line and skip the echo strip).
            host.emit_chunk("file1\r\nfile2\r\n");
            // Pull the UUID from the cmd payload to build the expanded
            // sentinel.
            let uuid = extract_uuid_from_payload(&cmd);
            host.emit_chunk(&format!("__WD_DONE_{uuid}__0\r\n"));
        });

        let code = run_oneshot(writer, reader, "Get-ChildItem", None, 5)
            .expect("run_oneshot ok");
        assert_eq!(code, 0);
        host_thread.join().expect("host thread");
    }

    #[test]
    fn run_oneshot_happy_path_ssh() {
        let (writer, reader, host) = make_split_pair();
        let host_thread = thread::spawn(move || {
            host.emit_chunk("PS C:\\>\r\n");
            // Client should now send `ssh -tt prod\r`.
            let ssh_cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("client should send ssh -tt");
            assert!(ssh_cmd.starts_with("ssh -tt prod"), "got: {ssh_cmd:?}");
            // Emit MOTD + remote prompt.
            host.emit_chunk("Welcome to Ubuntu\r\nMOTD line\r\n➜ \r\n");
            // Client should send the formatted bash command.
            let cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("client should send formatted bash command");
            assert!(cmd.contains("docker ps"), "cmd payload: {cmd:?}");
            assert!(cmd.contains("$?"), "bash sentinel uses $?: {cmd:?}");
            // Mimic remote bash echoing the command line + emitting
            // output + expanded sentinel.
            let uuid = extract_uuid_from_payload(&cmd);
            host.emit_chunk(&format!(
                "docker ps; echo \"__WD_DONE_{uuid}__$?\"\r\nrow1\r\nrow2\r\n__WD_DONE_{uuid}__0\r\n"
            ));
        });

        let code = run_oneshot(writer, reader, "docker ps", Some("prod"), 5)
            .expect("run_oneshot ok");
        assert_eq!(code, 0);
        host_thread.join().expect("host thread");
    }

    #[test]
    fn run_oneshot_timeout_returns_124() {
        let (writer, reader, host) = make_split_pair();
        let host_thread = thread::spawn(move || {
            host.emit_chunk("PS C:\\>\r\n");
            // Consume the cmd but never emit a sentinel.
            let _cmd = host.recv_shell_input(Duration::from_secs(2));
            thread::sleep(Duration::from_millis(2_000));
        });

        let code = run_oneshot(writer, reader, "Start-Sleep 60", None, 1)
            .expect("run_oneshot ok");
        assert_eq!(code, 124, "expected timeout exit code");
        host_thread.join().expect("host thread");
    }

    #[test]
    fn run_oneshot_propagates_nonzero_exit() {
        let (writer, reader, host) = make_split_pair();
        let host_thread = thread::spawn(move || {
            host.emit_chunk("PS C:\\>\r\n");
            let cmd = host
                .recv_shell_input(Duration::from_secs(2))
                .expect("cmd");
            let uuid = extract_uuid_from_payload(&cmd);
            host.emit_chunk(&format!("__WD_DONE_{uuid}__7\r\n"));
        });

        let code = run_oneshot(writer, reader, "exit 7", None, 5).expect("ok");
        assert_eq!(code, 7);
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

    // clean_stdout — strip prompts, echoed sentinel, and the
    // expanded sentinel from the accumulated wire output.

    #[test]
    fn clean_stdout_ps_only_mode() {
        let uuid = uuid::Uuid::nil();
        let buf = format!(
            "Some pre-prompt noise\nPS C:\\Users\\User>\nactual line 1\nactual line 2\n__WD_DONE_{uuid}__0\n"
        );
        assert_eq!(clean_stdout(&buf, &uuid), "actual line 1\nactual line 2");
    }

    #[test]
    fn clean_stdout_ssh_mode_strips_echo() {
        let uuid = uuid::Uuid::nil();
        let buf = format!(
            "MOTD line\n➜ \ndocker ps; echo \"__WD_DONE_{uuid}__$?\"\nrow1\nrow2\n__WD_DONE_{uuid}__0\n"
        );
        let out = clean_stdout(&buf, &uuid);
        assert!(!out.contains("__WD_DONE"), "echoed sentinel must be stripped: {out:?}");
        assert!(!out.contains("docker ps;"), "echoed cmd line should be gone: {out:?}");
        assert_eq!(out, "row1\nrow2");
    }

    #[test]
    fn clean_stdout_no_prompt_returns_pre_sentinel() {
        // If no prompt was ever observed (edge case — host emitted only
        // the sentinel), we keep everything before the sentinel.
        let uuid = uuid::Uuid::nil();
        let buf = format!("output line\n__WD_DONE_{uuid}__0\n");
        assert_eq!(clean_stdout(&buf, &uuid), "output line");
    }

    #[test]
    fn clean_stdout_uuid_disambiguates() {
        // Two sentinels in buffer with different UUIDs; only ours
        // is treated as the cut-off.
        let ours = uuid::Uuid::nil();
        let theirs = uuid::Uuid::from_u128(1);
        let buf = format!(
            "PS C:\\>\nleftover from earlier\n__WD_DONE_{theirs}__0\nour output\n__WD_DONE_{ours}__0\n"
        );
        let out = clean_stdout(&buf, &ours);
        assert!(out.contains("our output"));
        // The `theirs` sentinel is not stripped — it's part of "before our cut",
        // so it shows up in the result. That's acceptable: we're scoped to
        // *our* sentinel; if a stray sentinel from another agent shows up
        // earlier in the buffer we don't claim authority over it.
        assert!(out.contains(&theirs.to_string()));
    }

    #[test]
    fn clean_stdout_no_sentinel_returns_post_prompt() {
        // Defensive: helper called before sentinel arrived (caller
        // would normally only invoke after a match, but still).
        let uuid = uuid::Uuid::nil();
        let buf = "PS C:\\>\nstuff\n";
        assert_eq!(clean_stdout(buf, &uuid), "stuff");
    }

    #[test]
    fn parse_sentinel_matches_zero() {
        let uuid = uuid::Uuid::nil();
        let s = format!("__WD_DONE_{uuid}__0");
        assert_eq!(parse_sentinel(&s, &uuid), Some(0));
    }

    #[test]
    fn parse_sentinel_matches_nonzero() {
        let uuid = uuid::Uuid::nil();
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__7"), &uuid), Some(7));
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__124"), &uuid), Some(124));
        // Tolerate trailing whitespace / CR.
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__9\r"), &uuid), Some(9));
    }

    #[test]
    fn parse_sentinel_rejects_stdin_echo() {
        // Host PS echoing the format-string back: literal $LASTEXITCODE.
        let uuid = uuid::Uuid::nil();
        assert_eq!(
            parse_sentinel(&format!("__WD_DONE_{uuid}__$LASTEXITCODE"), &uuid),
            None
        );
        // Bash echo: literal $?
        assert_eq!(
            parse_sentinel(&format!("__WD_DONE_{uuid}__$?"), &uuid),
            None
        );
    }

    #[test]
    fn parse_sentinel_rejects_other_uuid() {
        let ours = uuid::Uuid::nil();
        let theirs = uuid::Uuid::from_u128(1);
        let line = format!("__WD_DONE_{theirs}__0");
        assert_eq!(parse_sentinel(&line, &ours), None);
    }

    #[test]
    fn parse_sentinel_rejects_garbage() {
        let uuid = uuid::Uuid::nil();
        assert_eq!(parse_sentinel("", &uuid), None);
        assert_eq!(parse_sentinel("hello world", &uuid), None);
        assert_eq!(parse_sentinel("__WD_DONE__0", &uuid), None);
        // Wrong tail format.
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__"), &uuid), None);
        // Non-numeric tail.
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__abc"), &uuid), None);
    }

    #[test]
    fn is_remote_prompt_rejects_non_prompt() {
        assert!(!is_remote_prompt(""));
        assert!(!is_remote_prompt("Welcome to Ubuntu 20.04.6 LTS"));
        assert!(!is_remote_prompt("karlovpg in 🌐 knd02 in ~"));
        assert!(!is_remote_prompt("PS C:\\>"));
    }
}

fn reader_thread(mut transport: Box<dyn Transport>, stop: Arc<AtomicBool>) {
    let stdout = io::stdout();
    // Cross-chunk state for LF→CRLF translation. The local terminal
    // is in raw mode, so a bare \n only moves the cursor down — it
    // doesn't return to column 0. Linux / SSH output uses bare \n,
    // which produced staircase output ("Welcome..." sliding right
    // line by line). We insert \r before any \n that wasn't already
    // preceded by \r, even across chunk boundaries.
    let mut last_was_cr = false;
    while !stop.load(Ordering::Relaxed) {
        let pkt = transport.recv();
        match pkt {
            Ok(p) => match p.message {
                Message::ShellOutput { data } => {
                    let translated = translate_output_for_terminal(&data, &mut last_was_cr);
                    let mut out = stdout.lock();
                    let _ = out.write_all(&translated);
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
