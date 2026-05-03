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

    // Best-effort: tell the host to close the shell.
    if let Ok(mut t) = writer.lock() {
        let _ = t.send(&Packet::new(Message::ShellClose, 0));
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

/// For each `0x7F` byte (macOS DEL = Backspace in raw mode) in `chunk`,
/// emit the standard "erase the previous character" sequence
/// `\x08 \x20 \x08` (cursor left, overwrite with space, cursor left
/// again). Everything else is dropped — interactive host shells echo
/// printable stdin back through stdout, so we don't want to mirror
/// them locally and produce doubled characters. Pure helper so the
/// behaviour is unit-tested without touching stdout.
fn local_echo_for_backspace(chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for &b in chunk {
        if b == 0x7F {
            out.extend_from_slice(b"\x08 \x08");
        }
    }
    out
}

/// Translate the byte stream we just read from stdin into what the host
/// shell expects on its stdin pipe. The only translation is `\r → \n`:
/// macOS raw mode delivers Enter as a bare CR (0x0D), but PowerShell /
/// cmd / bash on the host treat the line as terminated only when they
/// see an LF (0x0A). Without this fix you can type `dir<Enter>` and the
/// command just sits in the host's read buffer forever.
///
/// Bytes other than `\r` pass through unchanged — including 0x7F which
/// the host shell handles as its own backspace.
fn translate_input_for_host(chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(chunk.len());
    for &b in chunk {
        if b == b'\r' {
            out.push(b'\n');
        } else {
            out.push(b);
        }
    }
    out
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
    // Set stdin to non-blocking? On macOS we can rely on raw mode + a buffered read.
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
                // Selective local echo. Interactive shells echo
                // *printable* stdin bytes back through stdout, so we
                // don't double-print those. But they swallow control
                // bytes like 0x7F (macOS Backspace) silently — the
                // host shell erases its buffer correctly, the user
                // just sees nothing happen locally. Compute the
                // erase sequence for each Backspace and emit it now;
                // skip everything else.
                let echo = local_echo_for_backspace(chunk);
                if !echo.is_empty() {
                    let mut out = io::stdout().lock();
                    let _ = out.write_all(&echo);
                    let _ = out.flush();
                }
                let host_payload = translate_input_for_host(chunk);
                if let Ok(mut t) = writer.lock() {
                    if let Err(e) = t.send(&Packet::new(
                        Message::ShellInput { data: host_payload },
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
    fn local_echo_backspace_emits_erase_sequence() {
        let s = local_echo_for_backspace(b"\x7F");
        assert_eq!(s, b"\x08 \x08");
    }

    #[test]
    fn local_echo_backspace_ignores_printable() {
        // Printable bytes are echoed by the host shell itself; we must
        // not duplicate them locally.
        let s = local_echo_for_backspace(b"dir");
        assert!(s.is_empty(), "expected empty echo, got {s:?}");
    }

    #[test]
    fn local_echo_backspace_handles_multiple_in_chunk() {
        // Holding Backspace can deliver several DELs in one stdin read.
        let s = local_echo_for_backspace(b"a\x7F\x7Fb\x7F");
        assert_eq!(s, b"\x08 \x08\x08 \x08\x08 \x08");
    }

    #[test]
    fn translate_for_host_replaces_cr_with_lf() {
        // Raw-mode Enter is bare CR; host shells (PowerShell, cmd, bash)
        // need LF to terminate a line. Without this PowerShell
        // buffers `dir<Enter>` forever and never executes.
        let s = translate_input_for_host(b"dir\r");
        assert_eq!(s, b"dir\n");
    }

    #[test]
    fn translate_for_host_passes_through_non_cr_bytes() {
        // Backspace, printable ASCII, UTF-8 — all unchanged.
        let s = translate_input_for_host(b"a\x7Fb");
        assert_eq!(s, b"a\x7Fb");
        let s = translate_input_for_host("дир\r".as_bytes());
        let mut expected = "дир".as_bytes().to_vec();
        expected.push(b'\n');
        assert_eq!(s, expected);
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
