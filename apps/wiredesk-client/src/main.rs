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

    // Clipboard progress counters — read by the UI status-line (wired up in
    // Task 7a). Created here so the same Arc is shared by the writer thread
    // (sole updater of outgoing_*), the reader thread (incoming_*), and the
    // egui app (reader of all four).
    let outgoing_progress = Arc::new(AtomicU64::new(0));
    let outgoing_total = Arc::new(AtomicU64::new(0));
    let incoming_progress = Arc::new(AtomicU64::new(0));
    let incoming_total = Arc::new(AtomicU64::new(0));

    // Writer thread — owns one half of the port. Drains outgoing channel,
    // sends heartbeats, sends Hello on startup. UI never blocks because this
    // thread has zero shared locks with the egui thread.
    //
    // Owns outgoing_progress/total updates (M3 fix): increments AFTER each
    // successful transport.send so the UI reflects real wire-state progress
    // (≥2 visible increments during typical 500 KB transfers, AC5).
    let writer_events_tx = events_tx.clone();
    let client_name = cfg.client_name.clone();
    let writer_outgoing_progress = outgoing_progress.clone();
    let writer_outgoing_total = outgoing_total.clone();
    thread::spawn(move || {
        writer_thread(
            writer_transport,
            outgoing_rx,
            writer_events_tx,
            client_name,
            writer_outgoing_progress,
            writer_outgoing_total,
        );
    });

    // Reader thread — owns the other half. Just receives and dispatches.
    // Clone `events_tx` ahead of moving the original into reader_thread so the
    // clipboard poll thread can surface transient warnings (currently the
    // oversized-image toast, Task 7b) through the same UI event channel.
    //
    // Codex iter5 fix: outgoing_progress/total are also passed in so the
    // reader thread can zero them at the same handshake/disconnect/error
    // sites where it already zeroes incoming counters via
    // `IncomingClipboard::reset()`. Previously the UI thread cleared them on
    // `TransportEvent::Connected` — that raced with a peer ClipOffer arriving
    // between event-emit and next egui frame and wiped `incoming_total` mid
    // transfer, blanking the status line for the rest of that transfer.
    let poll_events_tx = events_tx.clone();
    let reader_clipboard = clipboard_state.clone();
    let reader_incoming_progress = incoming_progress.clone();
    let reader_incoming_total = incoming_total.clone();
    let reader_outgoing_progress = outgoing_progress.clone();
    let reader_outgoing_total = outgoing_total.clone();
    thread::spawn(move || {
        reader_thread(
            reader_transport,
            events_tx,
            reader_clipboard,
            reader_incoming_progress,
            reader_incoming_total,
            reader_outgoing_progress,
            reader_outgoing_total,
        );
    });

    // Clipboard poll thread — pushes Mac clipboard changes to host.
    // Outgoing progress counters are updated by writer_thread now (M3 fix),
    // not by the poll thread, so the UI sees real wire-state progress
    // instead of an instant jump to 100% as packets queue.
    clipboard::spawn_poll_thread(clipboard_state, outgoing_tx.clone(), poll_events_tx);

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
///
/// M3 fix: this thread is the SOLE updater of `outgoing_progress` /
/// `outgoing_total`. Counters are bumped via `clipboard::apply_outgoing_progress`
/// AFTER each successful `transport.send`, so the UI sees real wire-state
/// progress (≥2 increments visible during typical 500 KB transfers, AC5)
/// rather than instant jumps to 100% as packets queue into the unbounded mpsc.
fn writer_thread(
    mut transport: Box<dyn Transport>,
    outgoing_rx: mpsc::Receiver<Packet>,
    events_tx: mpsc::Sender<TransportEvent>,
    client_name: String,
    outgoing_progress: Arc<AtomicU64>,
    outgoing_total: Arc<AtomicU64>,
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
                // Update progress AFTER send returns — atomic reflects bytes
                // actually written to the UART, not bytes queued in mpsc.
                clipboard::apply_outgoing_progress(
                    &packet.message,
                    &outgoing_progress,
                    &outgoing_total,
                );
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
///
/// Codex iter5 fix: also owns the zeroing of `outgoing_progress` /
/// `outgoing_total` at the handshake (HelloAck), Disconnect, and transport
/// error sites. Doing it here — instead of in the UI thread on
/// `TransportEvent::Connected` — closes a race window where a peer ClipOffer
/// arriving between event-emit and the next egui frame would have its freshly
/// stored `incoming_total` wiped to 0, blanking the status line for the rest
/// of the transfer.
fn reader_thread(
    mut transport: Box<dyn Transport>,
    events_tx: mpsc::Sender<TransportEvent>,
    clipboard_state: clipboard::ClipboardState,
    incoming_progress: Arc<AtomicU64>,
    incoming_total: Arc<AtomicU64>,
    outgoing_progress: Arc<AtomicU64>,
    outgoing_total: Arc<AtomicU64>,
) {
    use std::sync::atomic::Ordering;

    // Helper closure — keeps the three reset sites identical and prevents
    // future drift. `IncomingClipboard::reset()` already zeroes incoming_*;
    // we also zero outgoing_* (sole owner is writer_thread, which only ever
    // increments — safe to clobber from here at session boundaries).
    let reset_session_state = |incoming_clip: &mut clipboard::IncomingClipboard| {
        incoming_clip.reset();
        clipboard_state.reset();
        outgoing_progress.store(0, Ordering::Relaxed);
        outgoing_total.store(0, Ordering::Relaxed);
    };

    // Keep our own handle to the sender-side state so we can clear its dedup
    // hash on disconnect (Codex iter4 F1 — without this, a mid-transfer abort
    // leaves `LastKind` stamped and the next poll after reconnect dedups
    // → silent lost-update). `IncomingClipboard` gets a clone for its receive
    // path; both refer to the same `Arc<Mutex<LastKind>>`.
    let mut incoming_clip = clipboard::IncomingClipboard::new(
        clipboard_state.clone(),
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
                    // doesn't carry over old counters. Also clear the
                    // sender-side dedup hash so the post-reconnect poll-tick
                    // resends the current OS-clipboard contents instead of
                    // dedup-skipping (Codex iter4 F1). Outgoing progress is
                    // zeroed here (Codex iter5) instead of in the UI thread,
                    // closing the race vs. an incoming ClipOffer that fires
                    // between Connected emit and the next egui frame.
                    reset_session_state(&mut incoming_clip);
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
                    reset_session_state(&mut incoming_clip);
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
                reset_session_state(&mut incoming_clip);
                let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                return;
            }
        }
    }
}
