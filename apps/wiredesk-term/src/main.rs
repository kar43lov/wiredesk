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

    let transport = SerialTransport::open(&args.port, args.baud)?;
    let transport: Arc<Mutex<Box<dyn Transport>>> = Arc::new(Mutex::new(Box::new(transport)));

    // Send Hello and wait for HelloAck. Limit how long we'll wait.
    handshake(&transport, &args.name)?;

    // Open shell on Host
    {
        let mut t = transport.lock().map_err(|_| WireDeskError::Transport("mutex poisoned".into()))?;
        t.send(&Packet::new(
            Message::ShellOpen { shell: args.shell.clone() },
            0,
        ))?;
    }

    // Switch local terminal to raw mode so we can forward keystrokes byte-by-byte.
    terminal::enable_raw_mode().map_err(|e| WireDeskError::Input(format!("raw mode: {e}")))?;

    let result = bridge_loop(transport.clone());

    // Restore terminal regardless of how the loop exited.
    let _ = terminal::disable_raw_mode();
    eprintln!("\nwiredesk-term: disconnected");

    // Best-effort: tell the host to close the shell.
    if let Ok(mut t) = transport.lock() {
        let _ = t.send(&Packet::new(Message::ShellClose, 0));
    }

    result
}

/// Send Hello, wait up to ~5 seconds for HelloAck. Drains other packets meanwhile.
fn handshake(transport: &Arc<Mutex<Box<dyn Transport>>>, client_name: &str) -> Result<()> {
    {
        let mut t = transport.lock().map_err(|_| WireDeskError::Transport("mutex poisoned".into()))?;
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
        let pkt = {
            let mut t = transport.lock().map_err(|_| WireDeskError::Transport("mutex poisoned".into()))?;
            t.recv()
        };
        match pkt {
            Ok(p) => match p.message {
                Message::HelloAck { host_name, .. } => {
                    eprintln!("wiredesk-term: connected to '{host_name}'");
                    return Ok(());
                }
                Message::Error { code, msg } => {
                    return Err(WireDeskError::Transport(format!(
                        "host returned error {code}: {msg}"
                    )));
                }
                // Other packets pre-handshake are ignored
                _ => continue,
            },
            Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Two threads — reader (serial → stdout) and main (stdin → serial).
fn bridge_loop(transport: Arc<Mutex<Box<dyn Transport>>>) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));

    // Reader thread: pull packets, write ShellOutput to stdout.
    let reader_stop = stop.clone();
    let reader_transport = transport.clone();
    let reader = thread::spawn(move || {
        reader_thread(reader_transport, reader_stop);
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
                if let Ok(mut t) = transport.lock() {
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

    // Signal reader to stop and join it.
    stop.store(true, Ordering::Relaxed);
    let _ = reader.join();
    Ok(())
}

fn reader_thread(transport: Arc<Mutex<Box<dyn Transport>>>, stop: Arc<AtomicBool>) {
    let stdout = io::stdout();
    while !stop.load(Ordering::Relaxed) {
        let pkt = {
            let Ok(mut t) = transport.lock() else { break };
            t.recv()
        };
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
