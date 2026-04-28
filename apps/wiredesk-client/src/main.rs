mod app;
mod input;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use clap::Parser;
use eframe::egui;
use wiredesk_protocol::message::{Message, VERSION};
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

use app::{TransportEvent, WireDeskApp};

#[derive(Parser)]
#[command(name = "wiredesk-client", about = "WireDesk client for macOS")]
struct Args {
    /// Serial port (e.g., /dev/tty.usbserial-XXX)
    #[arg(short, long)]
    port: String,

    /// Baud rate
    #[arg(short, long, default_value = "921600")]
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

    // Open serial transport
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

    let transport: Arc<Mutex<Box<dyn Transport>>> = Arc::new(Mutex::new(Box::new(transport)));
    let (events_tx, events_rx) = mpsc::channel();

    // Transport thread: handles handshake, heartbeat, receiving
    let transport_clone = Arc::clone(&transport);
    let client_name = args.name.clone();
    thread::spawn(move || {
        transport_thread(transport_clone, events_tx, client_name);
    });

    // Build and run egui app
    let app = WireDeskApp::new(args.port.clone(), events_rx, transport);

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

fn transport_thread(
    transport: Arc<Mutex<Box<dyn Transport>>>,
    events_tx: mpsc::Sender<TransportEvent>,
    client_name: String,
) {
    // Send HELLO
    {
        let Ok(mut t) = transport.lock() else {
            log::error!("transport mutex poisoned");
            let _ = events_tx.send(TransportEvent::Disconnected("internal error".into()));
            return;
        };
        let hello = Packet::new(
            Message::Hello { version: VERSION, client_name },
            0,
        );
        if let Err(e) = t.send(&hello) {
            log::error!("failed to send HELLO: {e}");
            let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
            return;
        }
    }

    // Wait for HELLO_ACK and then handle incoming messages
    loop {
        let packet = {
            let Ok(mut t) = transport.lock() else {
                log::error!("transport mutex poisoned");
                let _ = events_tx.send(TransportEvent::Disconnected("internal error".into()));
                return;
            };
            t.recv()
        };

        match packet {
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
                    log::debug!("clipboard offer: {total_len} bytes");
                    // TODO: receive clipboard chunks
                }
                Message::ShellOutput { data } => {
                    let _ = events_tx.send(TransportEvent::ShellOutput(data));
                }
                Message::ShellExit { code } => {
                    let _ = events_tx.send(TransportEvent::ShellExit(code));
                }
                Message::Error { code, msg } => {
                    log::warn!("error from host: code={code} msg={msg}");
                    // Surface as shell error if it looks shell-related; otherwise log only.
                    if msg.contains("shell") {
                        let _ = events_tx.send(TransportEvent::ShellError(msg));
                    }
                }
                Message::Disconnect => {
                    log::info!("host disconnected");
                    let _ = events_tx.send(TransportEvent::Disconnected("host disconnected".into()));
                    break;
                }
                other => {
                    log::debug!("ignored message: {other:?}");
                }
            },
            Err(ref e) if e.to_string().contains("timeout") => {
                // Normal timeout, continue
                continue;
            }
            Err(e) => {
                log::error!("transport error: {e}");
                let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }
}
