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
    #[arg(long, default_value = "30")]
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
/// PowerShell variant:
///   - `$LASTEXITCODE = 0` — pre-init the variable. Cmdlets (like
///     `echo`/`Write-Output`) do NOT set `$LASTEXITCODE`, only
///     external commands do. Without pre-init `$LASTEXITCODE` may be
///     `$null` and the interpolated sentinel becomes
///     `__WD_DONE_<uuid>__` (no integer tail), which `parse_sentinel`
///     correctly rejects → run_oneshot hangs to `--timeout`. This was
///     the root cause of the very first sentinel-never-arrives bug.
///   - `try { <cmd> } catch { $LASTEXITCODE = 1 }` — catches *terminating*
///     errors (`Get-Item /nonexistent`, mistyped cmdlet) so the
///     sentinel still emits. Without try/catch, a terminating error
///     skips the trailing statement.
///   - The trailing string is just emitted to the success stream;
///     PS prints it on its own line via implicit Write-Output.
///
/// Bash variant uses `$?` — bash always sets it after every command,
/// terminating or not. Bash also continues past a non-zero exit in a
/// `;`-list, so a plain `cmd; echo "<sentinel>"` is enough.
///
/// Line terminator is bare `\n` — PowerShell stdin in pipe mode does
/// NOT treat a lone `\r` as end-of-line and parks the line in its
/// read buffer waiting for `\n`. The interactive bridge sends `\n`
/// for the same reason (see `bridge_loop`'s line-flush).
fn format_command(uuid: &uuid::Uuid, kind: ShellKind, cmd: &str) -> String {
    match kind {
        // `$ErrorActionPreference='Stop'` flips PS *non-terminating*
        // errors into terminating ones for the duration of this line.
        // Without it, `Get-Item /nonexistent` writes to the error
        // stream, returns control, and the catch block never fires —
        // `$LASTEXITCODE` stays 0 → `--exec` returns 0 for an
        // obviously-failed command (the original AC2a regression).
        // Setting it inline keeps the scope local: assignments inside
        // an expression-statement only apply until the statement
        // separator. `$ErrorActionPreference` resets back to whatever
        // PS had before once the line ends.
        ShellKind::PowerShell => format!(
            "$LASTEXITCODE=0; $ErrorActionPreference='Stop'; try {{ {cmd} }} catch {{ $LASTEXITCODE=1 }}; \"__WD_DONE_{uuid}__$LASTEXITCODE\"\n"
        ),
        // Bash sandwich: READY marker BEFORE the command and DONE
        // sentinel AFTER. READY is the lower-bound that lets
        // clean_stdout slice off MOTD / SSH banner / prompt fragments
        // — without it, `ssh -tt prod-mup` (which falls back to
        // *non-interactive* remote shell when PS's stdin pipe blocks
        // PTY allocation, so no prompt ever arrives) would dump the
        // whole motd into the user's stdout.
        ShellKind::Bash => format!(
            "echo __WD_READY_{uuid}__; {cmd}; echo \"__WD_DONE_{uuid}__$?\"\n"
        ),
    }
}

/// State machine for `run_oneshot`. PS-only mode skips straight to
/// AwaitingSentinel (the formatted command is sent immediately —
/// PS pipe-mode reads stdin line-by-line, no need to sync). SSH
/// mode goes AwaitingRemotePrompt → AwaitingSentinel: we MUST wait
/// for the remote shell to emit its prompt before pushing the payload,
/// otherwise PS's .NET StreamReader read-ahead swallows whatever line
/// we sent after `ssh -tt ALIAS\n` (PS has consumed line 1 + buffered
/// line 2 BEFORE spawning ssh; line 2 is stuck in PS-memory, never
/// reaches the ssh subprocess).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum OneShotState {
    AwaitingRemotePrompt,
    AwaitingSentinel,
}

/// Strip ANSI/VT100 escape sequences from a string so prompt-detection
/// can match a real Starship/oh-my-zsh prompt that arrives wrapped in
/// color and terminal-mode escapes. Real-world `ssh -tt` Starship
/// trace ends a prompt line with `➜ \x1b[K\x1b[?1h\x1b=\x1b[?2004h` —
/// `is_remote_prompt` against the raw string fails (last char is `h`,
/// not `➜`/`$`/`#`).
///
/// Handles two common shapes:
///   - CSI: `ESC [ ... letter` (color, cursor, mode-set)
///   - OSC: `ESC ] ... BEL or ESC \\` (titles)
///   - simple two-char: `ESC =`, `ESC >`, `ESC c`, etc.
///
/// Pure helper, returns owned `String`.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some(&'[') => {
                // CSI: ESC [ <params> <letter>. Skip until terminator.
                chars.next();
                for nc in chars.by_ref() {
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(&']') => {
                // OSC: ESC ] <text> BEL  (or ESC ] <text> ESC \).
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc == '\x07' {
                        break;
                    }
                    if nc == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                // Simple two-char escape: ESC <one char>. Drop both.
                chars.next();
            }
        }
    }
    out
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
    log::debug!("[exec] uuid={uuid} kind={target_kind:?} payload={payload:?}");

    // PS-only path: PowerShell with piped stdout reads stdin
    // line-by-line and executes. Send the formatted command
    // immediately — no prompt-detection needed (PS doesn't reliably
    // emit a prompt in pipe-mode anyway).
    //
    // SSH path: we MUST wait for the *remote* shell prompt before
    // sending the payload. Reason: PS's .NET StreamReader does
    // read-ahead. If we push `ssh -tt ALIAS\n` + payload back-to-back
    // (one batch into PS's stdin pipe), PS slurps both lines into its
    // internal buffer, executes line 1 (spawns ssh), but line 2 stays
    // trapped in PS-memory and never reaches the ssh subprocess
    // (confirmed in trace logs — Starship prompt arrived on the wire,
    // payload silently went nowhere). Waiting for the remote prompt
    // before sending payload guarantees ssh is the active reader on
    // PS's stdin pipe by the time we push bytes.
    let mut state = if let Some(alias) = ssh {
        let ssh_cmd = format!("ssh -tt {alias}\n");
        log::debug!("[exec] ssh hop: {ssh_cmd:?}");
        send_text(&writer, &ssh_cmd)?;
        OneShotState::AwaitingRemotePrompt
    } else {
        log::debug!("[exec] sending payload");
        send_text(&writer, &payload)?;
        OneShotState::AwaitingSentinel
    };

    // `pending` is the line-walker scratch — only completed lines get
    // popped out of it. `full_log` accumulates EVERYTHING received so
    // `clean_stdout` at the end has the whole conversation to slice.
    let mut pending = String::new();
    let mut full_log = String::new();
    let started = std::time::Instant::now();
    let max_wait = Duration::from_secs(timeout_secs);

    let mut exit_code: Option<i32> = None;

    while started.elapsed() < max_wait {
        match reader.recv() {
            Ok(p) => match p.message {
                Message::ShellOutput { data } => {
                    let text = String::from_utf8_lossy(&data);
                    log::debug!(
                        "[exec] recv ShellOutput {} bytes: {text:?}",
                        data.len()
                    );
                    pending.push_str(&text);
                    full_log.push_str(&text);
                }
                Message::ShellExit { code } => {
                    log::debug!("[exec] recv ShellExit code={code} — host shell died");
                    exit_code = Some(code);
                    break;
                }
                Message::Error { code, msg } => {
                    log::debug!("[exec] recv host Error code={code} msg={msg:?}");
                }
                other => {
                    log::trace!("[exec] recv (ignored) {other:?}");
                }
            },
            Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => {
                // No data this tick — re-check overall timeout below.
            }
            Err(e) => {
                log::debug!("[exec] recv error: {e}");
                stop.store(true, Ordering::Relaxed);
                let _ = heartbeat.join();
                return Err(e);
            }
        }

        // Walk completed lines.
        while let Some(nl_idx) = pending.find('\n') {
            let line: String = pending[..nl_idx].trim_end_matches('\r').to_string();
            pending.drain(..=nl_idx);
            log::trace!("[exec] line state={state:?}: {line:?}");

            match state {
                OneShotState::AwaitingRemotePrompt => {
                    // Strip ANSI before prompt detection — Starship et al
                    // wrap prompts in color/cursor escapes plus a
                    // trailing `\x1b[K` (clear-to-EOL).
                    let stripped = strip_ansi(&line);
                    if is_remote_prompt(stripped.trim_end()) {
                        log::debug!("[exec] remote prompt matched (line), sending payload");
                        send_text(&writer, &payload)?;
                        state = OneShotState::AwaitingSentinel;
                    }
                }
                OneShotState::AwaitingSentinel => {
                    if let Some(code) = parse_sentinel(&line, &uuid) {
                        log::debug!("[exec] sentinel matched, exit code = {code}");
                        exit_code = Some(code);
                        break;
                    }
                }
            }
        }

        // Remote prompts arrive WITHOUT a trailing newline — bash/zsh
        // park the cursor right after `$ ` / `# ` / `➜ `. Peek the
        // partial leftover after stripping ANSI escapes.
        if state == OneShotState::AwaitingRemotePrompt {
            let stripped = strip_ansi(&pending);
            if is_remote_prompt(stripped.trim_end()) {
                log::debug!("[exec] remote prompt matched (partial), sending payload");
                send_text(&writer, &payload)?;
                state = OneShotState::AwaitingSentinel;
                pending.clear();
            }
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
            eprintln!("{}", format_timeout_diagnostic(&full_log, timeout_secs));
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

/// `true` when `line` is the literal expanded READY marker we emit at
/// the start of the Bash payload (just before the user's command), so
/// `clean_stdout` can slice off MOTD / SSH banner / `ssh -tt` warning
/// from the actual command output. The remote shell echoes our stdin
/// in `ssh -tt` mode, so the *unexpanded* literal `echo __WD_READY_<uuid>__`
/// also surfaces — `parse_ready` only matches the *expanded* form
/// (no `echo ` prefix).
fn parse_ready(line: &str, uuid: &uuid::Uuid) -> bool {
    line.trim() == format!("__WD_READY_{uuid}__")
}

/// Format a diagnostic message for `--exec` timeout. Includes the
/// last 256 bytes of the wire log so the user can see where things
/// stalled (mid-MOTD vs after READY-marker vs mid-command output).
///
/// Slicing on byte boundaries can land in the middle of a multi-byte
/// UTF-8 char; `String::from_utf8_lossy` handles that safely with a
/// `?` replacement. Debug-format on the tail escapes ANSI/CRLF so a
/// piped stderr stays parseable for downstream tooling.
fn format_timeout_diagnostic(buf: &str, timeout_secs: u64) -> String {
    let bytes = buf.as_bytes();
    let start = bytes.len().saturating_sub(256);
    let tail = String::from_utf8_lossy(&bytes[start..]);
    format!(
        "wiredesk-term: --exec timeout after {timeout_secs}s (no sentinel from host)\nlast bytes received: {tail:?}"
    )
}

/// Slice the accumulated output buffer down to *just* what `<cmd>`
/// produced. The wire-stream of one `run_oneshot` execution roughly
/// looks like:
///
/// ```text
/// [host MOTD / SSH banner / pre-prompt noise]
/// __WD_READY_<uuid>__              <- only in --ssh (Bash) path
/// [echoed command with sentinel format string]   <- only in --ssh path
/// [actual stdout of <cmd>]
/// __WD_DONE_<uuid>__<exit_code>    <- expanded sentinel
/// ```
///
/// Lower bound:
///  1. If READY marker is present (Bash payload, --ssh path) — slice
///     everything after it. This is the robust path.
///  2. Else, fall back to the last prompt line (PS-only path; no
///     prompt is fine since PS doesn't echo stdin and the only
///     pre-cmd noise is the prompt itself which doesn't have a `\n`,
///     so slice from beginning).
///
/// Upper bound: the sentinel line. Sentinel itself is dropped.
///
/// Within the slice, lines containing `__WD_DONE_<uuid>__$` (the
/// *unexpanded* sentinel — remote bash echoing our stdin under
/// `ssh -tt`) and `__WD_READY_<uuid>__` (the unexpanded READY echo,
/// same reason) are filtered out.
///
/// Pure helper, returns owned `String`.
fn clean_stdout(buf: &str, uuid: &uuid::Uuid) -> String {
    let lines: Vec<&str> = buf.split('\n').collect();

    // Find sentinel line index (first match).
    let sentinel_idx = lines
        .iter()
        .position(|l| parse_sentinel(l, uuid).is_some());
    let upper = sentinel_idx.unwrap_or(lines.len());

    // Lower bound: prefer READY marker, fall back to last prompt.
    let ready_idx = lines[..upper]
        .iter()
        .position(|l| parse_ready(l, uuid));
    let lower = if let Some(idx) = ready_idx {
        idx + 1
    } else {
        let prompt_idx = lines[..upper]
            .iter()
            .rposition(|l| is_powershell_prompt(l) || is_remote_prompt(l));
        prompt_idx.map(|i| i + 1).unwrap_or(0)
    };

    // Stdin-echo line literals — present in --ssh path because remote
    // shell echoes stdin. Filter both DONE and READY echoes.
    let done_echo = format!("__WD_DONE_{uuid}__$");
    let ready_echo = format!("__WD_READY_{uuid}__");
    let kept: Vec<&str> = lines[lower..upper]
        .iter()
        .copied()
        .filter(|l| {
            // Drop the unexpanded echoed sentinel formatter.
            if l.contains(&done_echo) {
                return false;
            }
            // Drop the unexpanded echoed READY emitter (the line
            // contains literal `echo __WD_READY_<uuid>__` — the
            // *expanded* one already served as our lower bound and
            // is not in this slice).
            if l.contains("echo ") && l.contains(&ready_echo) {
                return false;
            }
            true
        })
        .collect();

    let mut out = kept.join("\n");
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
            s.starts_with("$LASTEXITCODE=0;"),
            "PS payload must pre-init $LASTEXITCODE so cmdlet success → 0: {s}"
        );
        assert!(
            s.contains("try { Get-ChildItem }"),
            "PS payload must wrap cmd in try/catch: {s}"
        );
        assert!(
            s.contains("catch { $LASTEXITCODE=1 }"),
            "PS payload must set $LASTEXITCODE on terminating error: {s}"
        );
        assert!(
            s.contains("$LASTEXITCODE"),
            "PS sentinel must use $LASTEXITCODE: {s}"
        );
        assert!(s.ends_with('\n'), "payload must end with LF for host stdin: {s}");
    }

    #[test]
    fn format_command_powershell_cmdlet_yields_zero_exit() {
        // Regression: pre-init `$LASTEXITCODE=0` is what makes
        // sentinel parsing work for cmdlets (echo, Get-ChildItem, …)
        // — without it, PS would interpolate `$null` and the wire
        // line becomes `__WD_DONE_<uuid>__` (no integer tail), which
        // parse_sentinel rejects → run_oneshot hangs to --timeout.
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::PowerShell, "echo hello");
        // Simulate what PS would emit on success: $LASTEXITCODE
        // expands to 0, so the wire line is …__0.
        let simulated_wire_line = format!("__WD_DONE_{uuid}__0");
        assert_eq!(parse_sentinel(&simulated_wire_line, &uuid), Some(0));
        // The payload itself contains the literal `$LASTEXITCODE`,
        // not its expansion (we send it verbatim, PS expands).
        assert!(s.contains("__WD_DONE_") && s.contains("$LASTEXITCODE"));
    }

    #[test]
    fn format_command_bash_appends_sentinel() {
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::Bash, "docker ps");
        assert!(
            s.starts_with("echo __WD_READY_"),
            "bash payload must start with READY emitter: {s}"
        );
        assert!(s.contains("docker ps;"), "bash payload must contain cmd: {s}");
        assert!(
            s.contains("$?"),
            "bash sentinel must reference $?: {s}"
        );
        assert!(
            !s.contains("$LASTEXITCODE"),
            "bash payload must NOT use $LASTEXITCODE: {s}"
        );
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn format_command_bash_includes_ready_marker() {
        // Regression: READY marker before the cmd is what makes
        // clean_stdout slice MOTD / `ssh -tt` PTY warning / banner
        // off the output. Without it, --ssh stdout includes ~30 lines
        // of Ubuntu welcome text per call.
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::Bash, "ls");
        let ready_marker = format!("__WD_READY_{uuid}__");
        assert!(s.contains(&ready_marker), "missing READY marker: {s}");
        // READY must precede the user command.
        let ready_pos = s.find(&ready_marker).unwrap();
        let cmd_pos = s.find("ls;").unwrap();
        assert!(ready_pos < cmd_pos, "READY must come before cmd: {s}");
    }

    #[test]
    fn format_timeout_diagnostic_truncates_and_handles_utf8() {
        // Long ASCII buffer → only last 256 bytes appear in output.
        let long = "X".repeat(1024);
        let out = format_timeout_diagnostic(&long, 30);
        assert!(out.contains("--exec timeout after 30s"));
        // First 256 bytes must NOT appear (we truncated to the tail).
        // Quick check: count Xs in the formatted tail — should be 256
        // exactly (Debug-format wraps in quotes, no escaping for ASCII).
        let x_count = out.matches('X').count();
        assert_eq!(x_count, 256, "expected last 256 X's, got {x_count}");

        // Empty buffer → no panic, output still includes the timeout
        // sentence and an empty tail (`""` after Debug-format).
        let out = format_timeout_diagnostic("", 5);
        assert!(out.contains("--exec timeout after 5s"));
        assert!(out.contains("last bytes received: \"\""));

        // Buffer ending mid-cyrillic multi-byte char → no panic.
        // Cyrillic "к" is 2 bytes (0xD0 0xBA). Build a 257-byte buf
        // where the last 2 bytes are a complete "к" so when we slice
        // `[len-256..]` the start lands inside the previous "к" → lossy
        // decode replaces with `?`. Test only verifies no-panic and
        // that the output is well-formed UTF-8.
        let mut buf = String::from("a");
        for _ in 0..128 {
            buf.push('к'); // 256 bytes of cyrillic
        }
        // buf is now 1 + 256 = 257 bytes; slice [1..257] starts inside
        // first "к" (at the 2nd byte 0xBA). lossy decoder must handle.
        let out = format_timeout_diagnostic(&buf, 1);
        assert!(out.contains("--exec timeout after 1s"));
        assert!(out.is_ascii() || out.chars().all(|c| !c.is_control() || c == '\n'));
    }

    #[test]
    fn strip_ansi_csi_color_codes() {
        assert_eq!(
            strip_ansi("\x1b[1;33muser\x1b[0m in \x1b[1;36m~\x1b[0m"),
            "user in ~"
        );
    }

    #[test]
    fn strip_ansi_keeps_unicode_arrow() {
        // Real Starship trailing prompt — `➜ \x1b[K` plus terminal
        // mode escapes. After strip, only `➜ ` should remain.
        assert_eq!(
            strip_ansi("➜ \x1b[K\x1b[?1h\x1b=\x1b[?2004h"),
            "➜ "
        );
    }

    #[test]
    fn strip_ansi_leaves_plain_text_unchanged() {
        assert_eq!(strip_ansi("just text"), "just text");
        assert_eq!(strip_ansi(""), "");
        assert_eq!(strip_ansi("PS C:\\>"), "PS C:\\>");
    }

    #[test]
    fn strip_ansi_starship_full_prompt_line_matches_remote_prompt() {
        // The real wire format from a live `ssh -tt prod` session.
        // After strip we expect a string that ends with `➜ ` so
        // is_remote_prompt returns true.
        let raw = "\r\u{1b}[0m\u{1b}[27m\u{1b}[24m\u{1b}[J\u{1b}[1;33muser\u{1b}[0m in \u{1b}[1;2;32m🌐 cgu-knd-firecards-1\u{1b}[0m in \u{1b}[1;36m~\u{1b}[0m \r\n➜ \u{1b}[K\u{1b}[?1h\u{1b}=\u{1b}[?2004h";
        let stripped = strip_ansi(raw);
        // Last non-newline content should end with `➜ ` (or `➜`
        // after trim).
        assert!(
            is_remote_prompt(stripped.trim_end()),
            "stripped Starship prompt should match is_remote_prompt: {stripped:?}"
        );
    }

    #[test]
    fn parse_ready_matches_expanded_only() {
        let uuid = uuid::Uuid::nil();
        // Expanded form (what shell prints when it executes echo).
        assert!(parse_ready(&format!("__WD_READY_{uuid}__"), &uuid));
        assert!(parse_ready(&format!("  __WD_READY_{uuid}__  "), &uuid));
        // Stdin echo from `ssh -tt` (literal `echo …`) — must NOT match.
        assert!(!parse_ready(
            &format!("echo __WD_READY_{uuid}__"),
            &uuid
        ));
        // Wrong UUID.
        let other = uuid::Uuid::from_u128(1);
        assert!(!parse_ready(&format!("__WD_READY_{other}__"), &uuid));
        // Empty / garbage.
        assert!(!parse_ready("", &uuid));
        assert!(!parse_ready("hello", &uuid));
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

        let code = run_oneshot(writer, reader, "Get-ChildItem", None, 5)
            .expect("run_oneshot ok");
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

        let code = run_oneshot(writer, reader, "docker ps", Some("prod"), 5)
            .expect("run_oneshot ok");
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

        let code = run_oneshot(writer, reader, "Start-Sleep 60", None, 1)
            .expect("run_oneshot ok");
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
    fn clean_stdout_ssh_mode_strips_motd_and_echo() {
        // Realistic --ssh wire stream: MOTD flood, `ssh -tt`-echoed
        // payload (single line containing READY-emitter + cmd + DONE-
        // formatter), expanded READY (lower bound), command output,
        // expanded DONE sentinel (upper bound).
        let uuid = uuid::Uuid::nil();
        let buf = format!(
            "Welcome to Ubuntu\nMOTD line 1\nMOTD line 2\n\
             echo __WD_READY_{uuid}__; docker ps; echo \"__WD_DONE_{uuid}__$?\"\n\
             __WD_READY_{uuid}__\n\
             row1\nrow2\n\
             __WD_DONE_{uuid}__0\n"
        );
        let out = clean_stdout(&buf, &uuid);
        assert!(!out.contains("Welcome"), "MOTD must be stripped: {out:?}");
        assert!(!out.contains("__WD_READY"), "READY echo must be stripped: {out:?}");
        assert!(!out.contains("__WD_DONE"), "echoed/expanded sentinel must be stripped: {out:?}");
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
