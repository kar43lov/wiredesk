mod app;
mod input;

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
    #[arg(short, long)]
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

    let transport: Box<dyn Transport> = Box::new(transport);
    let (events_tx, events_rx) = mpsc::channel();
    let (outgoing_tx, outgoing_rx) = mpsc::channel();

    let client_name = args.name.clone();
    thread::spawn(move || {
        transport_thread(transport, outgoing_rx, events_tx, client_name);
    });

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

/// Single-threaded owner of the serial transport. Drains the outgoing channel,
/// emits periodic heartbeats, and forwards received packets to the UI as events.
fn transport_thread(
    mut transport: Box<dyn Transport>,
    outgoing_rx: mpsc::Receiver<Packet>,
    events_tx: mpsc::Sender<TransportEvent>,
    client_name: String,
) {
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

    // Send HELLO
    if let Err(e) = transport.send(&Packet::new(
        Message::Hello { version: VERSION, client_name },
        0,
    )) {
        log::error!("failed to send HELLO: {e}");
        let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
        return;
    }

    let mut last_heartbeat_sent = Instant::now();

    loop {
        // 1. Drain pending outgoing packets first — this is what keeps input latency low.
        loop {
            match outgoing_rx.try_recv() {
                Ok(packet) => {
                    if let Err(e) = transport.send(&packet) {
                        log::error!("send error: {e}");
                        let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                        return;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // 2. Periodic heartbeat
        if last_heartbeat_sent.elapsed() >= HEARTBEAT_INTERVAL {
            let _ = transport.send(&Packet::new(Message::Heartbeat, 0));
            last_heartbeat_sent = Instant::now();
        }

        // 3. Try to receive one packet (short timeout — see SerialTransport::open).
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
                    log::debug!("clipboard offer: {total_len} bytes (not yet implemented)");
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
            Err(ref e) if e.to_string().contains("timeout") => {
                // Normal idle timeout — loop back and drain outgoing.
                continue;
            }
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
