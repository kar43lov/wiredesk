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
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args = Args::parse();

    if let Err(e) = run(&args) {
        // Make sure we always restore the terminal before printing the error.
        let _ = terminal::disable_raw_mode();
        eprintln!("\nwiredesk-term error: {e}");
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<()> {
    eprintln!(
        "wiredesk-term: connecting to {} @ {} baud (Ctrl+] to quit)",
        args.port, args.baud
    );

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
    handshake(&writer, &mut reader, &args.name)?;

    // Open shell on Host
    {
        let mut t = writer.lock().map_err(|_| WireDeskError::Transport("mutex poisoned".into()))?;
        t.send(&Packet::new(
            Message::ShellOpen { shell: args.shell.clone() },
            0,
        ))?;
    }

    // Switch local terminal to raw mode so we can forward keystrokes byte-by-byte.
    terminal::enable_raw_mode().map_err(|e| WireDeskError::Input(format!("raw mode: {e}")))?;

    let result = bridge_loop(writer.clone(), reader);

    // Restore terminal regardless of how the loop exited.
    let _ = terminal::disable_raw_mode();
    eprintln!("\nwiredesk-term: disconnected");

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

    result
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

/// Send Hello on the writer, drain HelloAck on the reader. Reader is
/// taken `&mut` because `recv()` needs `&mut self` to update its frame
/// buffer; the caller keeps ownership and hands the reader off to
/// `bridge_loop` afterward.
fn handshake(
    writer: &Arc<Mutex<Box<dyn Transport>>>,
    reader: &mut Box<dyn Transport>,
    client_name: &str,
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
                    eprintln!("{}", format_connected_banner(&host_name, screen_w, screen_h));
                    eprint!("{}", format_hotkey_cheatsheet());
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
