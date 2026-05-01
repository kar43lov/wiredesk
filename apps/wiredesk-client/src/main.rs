mod app;
mod clipboard;
mod input;
mod keyboard_tap;

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use eframe::egui;
use wiredesk_protocol::message::{Message, VERSION};
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

use app::{TransportEvent, WireDeskApp};

#[derive(Parser)]
#[command(name = "wiredesk-client", about = "WireDesk client for macOS")]
struct Args {
    /// Serial port (e.g., /dev/cu.usbserial-XXX)
    #[arg(short, long, default_value = "/dev/cu.usbserial-120")]
    port: String,

    /// Baud rate
    #[arg(short, long, default_value = "115200")]
    baud: u32,

    /// Client display name
    #[arg(long, default_value = "wiredesk-client")]
    name: String,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    log::info!("WireDesk Client");
    log::info!("serial: {} @ {} baud", args.port, args.baud);

    let transport = match wiredesk_transport::serial::SerialTransport::open(&args.port, args.baud) {
        Ok(t) => t,
        Err(e) => {
            log::error!("failed to open serial port: {e}");
            eprintln!("Error: {e}");
            eprintln!("Available ports:");
            if let Ok(ports) = serialport::available_ports() {
                for p in ports {
                    eprintln!("  {}", p.port_name);
                }
            }
            std::process::exit(1);
        }
    };

    let writer_transport: Box<dyn Transport> = Box::new(transport);
    let reader_transport = match writer_transport.try_clone() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Error: cannot clone serial port for reader thread: {e}");
            std::process::exit(1);
        }
    };

    let (events_tx, events_rx) = mpsc::channel();
    let (outgoing_tx, outgoing_rx) = mpsc::channel();

    // Shared clipboard state — used by both poll thread (which detects local
    // changes) and reader thread (which writes incoming text). Hash-based
    // dedup avoids the bounce-back loop.
    let clipboard_state = clipboard::ClipboardState::new();

    // Writer thread — owns one half of the port. Drains outgoing channel,
    // sends heartbeats, sends Hello on startup. UI never blocks because this
    // thread has zero shared locks with the egui thread.
    let writer_events_tx = events_tx.clone();
    let client_name = args.name.clone();
    thread::spawn(move || {
        writer_thread(writer_transport, outgoing_rx, writer_events_tx, client_name);
    });

    // Reader thread — owns the other half. Just receives and dispatches.
    let reader_clipboard = clipboard_state.clone();
    thread::spawn(move || {
        reader_thread(reader_transport, events_tx, reader_clipboard);
    });

    // Clipboard poll thread — pushes Mac clipboard changes to host.
    clipboard::spawn_poll_thread(clipboard_state, outgoing_tx.clone());

    let app = WireDeskApp::new(args.port.clone(), events_rx, outgoing_tx);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 600.0])
            .with_title("WireDesk"),
        ..Default::default()
    };

    if let Err(e) = eframe::run_native("WireDesk", options, Box::new(|_cc| Ok(Box::new(app)))) {
        log::error!("eframe error: {e}");
    }
}

/// Sole writer to the serial port. Any UI-driven packet hits the wire within
/// one channel hop (~µs) — no waiting on a recv timeout.
fn writer_thread(
    mut transport: Box<dyn Transport>,
    outgoing_rx: mpsc::Receiver<Packet>,
    events_tx: mpsc::Sender<TransportEvent>,
    client_name: String,
) {
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

    if let Err(e) = transport.send(&Packet::new(
        Message::Hello { version: VERSION, client_name },
        0,
    )) {
        log::error!("failed to send HELLO: {e}");
        let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
        return;
    }

    let mut last_heartbeat = Instant::now();

    loop {
        let timeout = HEARTBEAT_INTERVAL
            .saturating_sub(last_heartbeat.elapsed())
            .max(Duration::from_millis(1));

        match outgoing_rx.recv_timeout(timeout) {
            Ok(packet) => {
                if let Err(e) = transport.send(&packet) {
                    log::error!("send error: {e}");
                    let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            let _ = transport.send(&Packet::new(Message::Heartbeat, 0));
            last_heartbeat = Instant::now();
        }
    }
}

/// Sole reader of the serial port. Translates incoming packets to UI events.
fn reader_thread(
    mut transport: Box<dyn Transport>,
    events_tx: mpsc::Sender<TransportEvent>,
    clipboard_state: clipboard::ClipboardState,
) {
    let mut incoming_clip = clipboard::IncomingClipboard::new(clipboard_state);
    loop {
        match transport.recv() {
            Ok(p) => match p.message {
                Message::HelloAck { host_name, screen_w, screen_h, .. } => {
                    log::info!("connected to '{host_name}' ({screen_w}x{screen_h})");
                    let _ = events_tx.send(TransportEvent::Connected {
                        host_name,
                        screen_w,
                        screen_h,
                    });
                }
                Message::Heartbeat => {
                    let _ = events_tx.send(TransportEvent::Heartbeat);
                }
                Message::ClipOffer { total_len, .. } => {
                    incoming_clip.on_offer(total_len);
                }
                Message::ClipChunk { index, data } => {
                    incoming_clip.on_chunk(index, data);
                }
                Message::ShellOutput { data } => {
                    let _ = events_tx.send(TransportEvent::ShellOutput(data));
                }
                Message::ShellExit { code } => {
                    let _ = events_tx.send(TransportEvent::ShellExit(code));
                }
                Message::Error { code, msg } => {
                    log::warn!("error from host: code={code} msg={msg}");
                    if msg.contains("shell") {
                        let _ = events_tx.send(TransportEvent::ShellError(msg));
                    }
                }
                Message::Disconnect => {
                    log::info!("host disconnected");
                    let _ = events_tx.send(TransportEvent::Disconnected("host disconnected".into()));
                    return;
                }
                other => {
                    log::debug!("ignored message: {other:?}");
                }
            },
            Err(ref e) if e.to_string().contains("timeout") => continue,
            Err(wiredesk_core::error::WireDeskError::Protocol(ref msg)) => {
                log::warn!("dropping bad frame: {msg}");
                continue;
            }
            Err(e) => {
                log::error!("transport error: {e}");
                let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                return;
            }
        }
    }
}
