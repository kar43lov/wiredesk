mod app;
mod clipboard;
mod config;
mod input;
mod keyboard_tap;
mod monitor;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use clap::{CommandFactory, Parser};
use eframe::egui;
use wiredesk_protocol::message::{Message, VERSION};
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

use app::{TransportEvent, WireDeskApp};
use config::ClientConfig;

#[derive(Parser)]
#[command(name = "wiredesk-client", about = "WireDesk client for macOS")]
pub struct Args {
    /// Serial port (e.g., /dev/cu.usbserial-XXX). Overrides config.toml.
    #[arg(short, long, default_value = "/dev/cu.usbserial-120")]
    port: String,

    /// Baud rate. Overrides config.toml.
    #[arg(short, long, default_value = "115200")]
    baud: u32,

    /// Client display name. Overrides config.toml.
    #[arg(long, default_value = "wiredesk-client")]
    name: String,
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Resolve config: defaults → config.toml → CLI args (override).
    let toml_cfg = ClientConfig::load();
    let matches = Args::command().get_matches();
    let cfg = config::merge_args(&matches, toml_cfg);

    log::info!("WireDesk Client");
    log::info!("config: {}", ClientConfig::config_path().display());
    log::info!("serial: {} @ {} baud", cfg.port, cfg.baud);

    let transport = match wiredesk_transport::serial::SerialTransport::open(&cfg.port, cfg.baud) {
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
    let (tap_events_tx, tap_events_rx) = mpsc::channel();

    // Shared clipboard state — used by both poll thread (which detects local
    // changes) and reader thread (which writes incoming text). Hash-based
    // dedup avoids the bounce-back loop.
    let clipboard_state = clipboard::ClipboardState::new();

    // Writer thread — owns one half of the port. Drains outgoing channel,
    // sends heartbeats, sends Hello on startup. UI never blocks because this
    // thread has zero shared locks with the egui thread.
    let writer_events_tx = events_tx.clone();
    let client_name = cfg.client_name.clone();
    thread::spawn(move || {
        writer_thread(writer_transport, outgoing_rx, writer_events_tx, client_name);
    });

    // Clipboard progress counters — read by the UI status-line (wired up in
    // Task 7a). Created here so the same Arc is shared by the poll thread,
    // the reader thread, and the egui app.
    let outgoing_progress = Arc::new(AtomicU64::new(0));
    let outgoing_total = Arc::new(AtomicU64::new(0));
    let incoming_progress = Arc::new(AtomicU64::new(0));
    let incoming_total = Arc::new(AtomicU64::new(0));

    // Reader thread — owns the other half. Just receives and dispatches.
    let reader_clipboard = clipboard_state.clone();
    let reader_incoming_progress = incoming_progress.clone();
    let reader_incoming_total = incoming_total.clone();
    thread::spawn(move || {
        reader_thread(
            reader_transport,
            events_tx,
            reader_clipboard,
            reader_incoming_progress,
            reader_incoming_total,
        );
    });

    // Clipboard poll thread — pushes Mac clipboard changes to host.
    clipboard::spawn_poll_thread(
        clipboard_state,
        outgoing_tx.clone(),
        outgoing_progress.clone(),
        outgoing_total.clone(),
    );

    // Keyboard tap (macOS only — no-op elsewhere). Initially disabled;
    // enable() is called when the user enters capture-mode.
    let tap_handle = keyboard_tap::start(outgoing_tx.clone(), tap_events_tx);

    let app = WireDeskApp::new(
        cfg,
        events_rx,
        outgoing_tx,
        tap_events_rx,
        tap_handle,
        outgoing_progress,
        outgoing_total,
        incoming_progress,
        incoming_total,
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 600.0])
            .with_title("WireDesk"),
        ..Default::default()
    };

    if let Err(e) = eframe::run_native(
        "WireDesk",
        options,
        Box::new(|cc| {
            // egui's `include_image!` macro emits an ImageSource that needs a
            // registered loader at runtime — without this call the heading
            // image just renders as an "unable to load image" placeholder.
            egui_extras::install_image_loaders(&cc.egui_ctx);
            // winit/eframe's NSApp init can leave the Dock with a generic
            // exec icon a couple seconds after launch even when the bundle's
            // AppIcon.icns is correct. Force-loading the bundle icon and
            // re-applying via setApplicationIconImage on the main thread
            // (which is where eframe creator callbacks run) overrides
            // whatever winit did and pins the W to the Dock for the whole
            // process lifetime.
            #[cfg(target_os = "macos")]
            unsafe {
                force_dock_icon_from_bundle();
            }
            Ok(Box::new(app))
        }),
    ) {
        log::error!("eframe error: {e}");
    }
}

#[cfg(target_os = "macos")]
pub(crate) unsafe fn force_dock_icon_from_bundle() {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    let bundle: *mut AnyObject = msg_send![class!(NSBundle), mainBundle];
    if bundle.is_null() {
        return;
    }
    let name = NSString::from_str("AppIcon");
    let typ = NSString::from_str("icns");
    let path: *mut AnyObject = msg_send![bundle, pathForResource: &*name, ofType: &*typ];
    if path.is_null() {
        log::warn!("force_dock_icon: AppIcon.icns not in bundle");
        return;
    }
    let alloc: *mut AnyObject = msg_send![class!(NSImage), alloc];
    let image: *mut AnyObject = msg_send![alloc, initWithContentsOfFile: path];
    if image.is_null() {
        log::warn!("force_dock_icon: NSImage failed to load AppIcon.icns");
        return;
    }
    let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
    // 0 = NSApplicationActivationPolicyRegular — guarantee the bundle stays
    // visible in the Dock; without this, winit/eframe sometimes leaves the
    // policy in an in-between state that drops us out of the Dock.
    let _: () = msg_send![app, setActivationPolicy: 0_i64];
    let _: () = msg_send![app, setApplicationIconImage: image];
    // Drop the image — NSApplication retains it internally.
    let _: Retained<AnyObject> = Retained::from_raw(image).expect("image was just constructed");
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
    incoming_progress: Arc<AtomicU64>,
    incoming_total: Arc<AtomicU64>,
) {
    let mut incoming_clip = clipboard::IncomingClipboard::new(
        clipboard_state,
        incoming_progress,
        incoming_total,
    );
    loop {
        match transport.recv() {
            Ok(p) => match p.message {
                Message::HelloAck { host_name, screen_w, screen_h, .. } => {
                    log::info!("connected to '{host_name}' ({screen_w}x{screen_h})");
                    // New session — drop any stale partial reassembly from a
                    // previous (now-defunct) connection so the progress UI
                    // doesn't carry over old counters.
                    incoming_clip.reset();
                    let _ = events_tx.send(TransportEvent::Connected {
                        host_name,
                        screen_w,
                        screen_h,
                    });
                }
                Message::Heartbeat => {
                    let _ = events_tx.send(TransportEvent::Heartbeat);
                }
                Message::ClipOffer { format, total_len } => {
                    incoming_clip.on_offer(format, total_len);
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
                    incoming_clip.reset();
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
                incoming_clip.reset();
                let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                return;
            }
        }
    }
}
