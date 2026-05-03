mod app;
mod clipboard;
mod config;
mod input;
mod keyboard_tap;
mod monitor;
mod restart;
mod status_bar;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

    // Runtime image-clipboard toggles (Settings panel). Initial values come
    // from the loaded config; UI flips them at runtime — no restart required.
    // Text clipboard is unaffected.
    let send_images = Arc::new(std::sync::atomic::AtomicBool::new(cfg.send_images));
    let receive_images = Arc::new(std::sync::atomic::AtomicBool::new(cfg.receive_images));
    let send_text = Arc::new(std::sync::atomic::AtomicBool::new(cfg.send_text));
    let receive_text = Arc::new(std::sync::atomic::AtomicBool::new(cfg.receive_text));
    // Karabiner-Elements `left_command ↔ left_option` compensation (see
    // ClientConfig::swap_option_command). Read once on startup and surfaced
    // through Settings; flipping the checkbox at runtime takes effect on the
    // next FlagsChanged / KeyDown the tap sees.
    let swap_option_command = Arc::new(std::sync::atomic::AtomicBool::new(cfg.swap_option_command));

    // Writer thread — owns one half of the port. Drains outgoing channel,
    // sends heartbeats, sends Hello on startup. UI never blocks because this
    // thread has zero shared locks with the egui thread.
    //
    // Owns outgoing_progress/total updates (M3 fix): increments AFTER each
    // successful transport.send so the UI reflects real wire-state progress
    // (≥2 visible increments during typical 500 KB transfers, AC5).
    // Cancel atomics need to be in scope before writer/reader spawn — both
    // threads observe them to drop in-flight clipboard packets when the UI
    // hits the Cancel button. (See `outgoing_text_in_flight` block below
    // for the full set of clipboard-orchestration atomics.)
    let outgoing_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let incoming_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let writer_events_tx = events_tx.clone();
    let client_name = cfg.client_name.clone();
    let writer_outgoing_progress = outgoing_progress.clone();
    let writer_outgoing_total = outgoing_total.clone();
    let writer_outgoing_cancel = outgoing_cancel.clone();
    thread::spawn(move || {
        writer_thread(
            writer_transport,
            outgoing_rx,
            writer_events_tx,
            client_name,
            writer_outgoing_progress,
            writer_outgoing_total,
            writer_outgoing_cancel,
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
    let reader_receive_images = receive_images.clone();
    let reader_receive_text = receive_text.clone();
    let reader_incoming_cancel = incoming_cancel.clone();
    thread::spawn(move || {
        reader_thread(
            reader_transport,
            events_tx,
            reader_clipboard,
            reader_incoming_progress,
            reader_incoming_total,
            reader_outgoing_progress,
            reader_outgoing_total,
            reader_receive_images,
            reader_receive_text,
            reader_incoming_cancel,
        );
    });

    // Synthetic-combo dispatcher pieces. Whispr Flow / TextExpander send
    // Cmd+V via CGEventPost, which races against Mac→Host clipboard sync —
    // without deferral the synthesized paste lands on the previous
    // clipboard. The poll thread flips `outgoing_text_in_flight` true at
    // the start of every text-send and clears it on the next tick;
    // meanwhile the tap shoves all synthetic combos through `synth_tx`,
    // and the dispatcher below drains the channel, waiting on the flag.
    let outgoing_text_in_flight =
        Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (synth_tx, synth_rx) = std::sync::mpsc::channel::<keyboard_tap::SyntheticCombo>();

    // Clipboard poll thread — pushes Mac clipboard changes to host.
    // Outgoing progress counters are updated by writer_thread now (M3 fix),
    // not by the poll thread, so the UI sees real wire-state progress
    // instead of an instant jump to 100% as packets queue.
    clipboard::spawn_poll_thread(
        clipboard_state,
        outgoing_tx.clone(),
        poll_events_tx,
        send_images.clone(),
        send_text.clone(),
        outgoing_text_in_flight.clone(),
    );

    // Synthetic dispatcher thread — see comment above. Holds each combo
    // while a clipboard sync is in flight (max 2 s), then waits a short
    // grace for Host to commit before emitting on the wire.
    {
        let outgoing_tx = outgoing_tx.clone();
        let in_flight = outgoing_text_in_flight.clone();
        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
            const GRACE: std::time::Duration = std::time::Duration::from_millis(150);
            const POLL: std::time::Duration = std::time::Duration::from_millis(25);
            while let Ok(combo) = synth_rx.recv() {
                let start = std::time::Instant::now();
                while in_flight.load(Ordering::Acquire) && start.elapsed() < MAX_WAIT {
                    std::thread::sleep(POLL);
                }
                std::thread::sleep(GRACE);
                for packet in combo {
                    let _ = outgoing_tx.send(packet);
                }
            }
        });
    }

    // Keyboard tap (macOS only — no-op elsewhere). Initially disabled;
    // enable() is called when the user enters capture-mode.
    let tap_handle = keyboard_tap::start(
        outgoing_tx.clone(),
        tap_events_tx,
        swap_option_command.clone(),
        synth_tx,
    );

    // Status bar item — same Arcs the egui status row reads from. Idle
    // shows "W"; in-flight transfer shows "↑ N%" / "↓ N%". Initialised
    // inside the eframe creator below to satisfy AppKit's main-thread
    // invariant (eframe creator runs on the main thread on macOS).
    let status_bar_counters = status_bar::StatusBarCounters {
        outgoing_progress: outgoing_progress.clone(),
        outgoing_total: outgoing_total.clone(),
        incoming_progress: incoming_progress.clone(),
        incoming_total: incoming_total.clone(),
    };

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
        send_images,
        receive_images,
        send_text,
        receive_text,
        swap_option_command,
        outgoing_cancel,
        incoming_cancel,
    );

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 760.0])
            .with_title("WireDesk"),
        ..Default::default()
    };

    // Move status bar counters into the creator closure — it runs on the
    // main thread on macOS, satisfying NSStatusBar's threading invariant.
    let creator_status_bar_counters = status_bar_counters;
    if let Err(e) = eframe::run_native(
        "WireDesk",
        options,
        Box::new(move |cc| {
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
            // Stash the StatusBarHandle inside the egui app via a Box leak
            // so it lives for the program's lifetime. The handle's only job
            // is to keep the NSStatusItem alive — once dropped, AppKit
            // removes the menu bar item.
            let _handle = status_bar::init(creator_status_bar_counters);
            std::mem::forget(_handle);
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
    outgoing_cancel: Arc<std::sync::atomic::AtomicBool>,
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
                let is_clip = matches!(
                    packet.message,
                    Message::ClipOffer { .. } | Message::ClipChunk { .. }
                );
                let cancelling = outgoing_cancel.load(Ordering::Acquire);
                if is_clip && cancelling {
                    // User pressed Cancel mid-transfer. Drop the queued
                    // clip packet without writing it to the wire so Host
                    // never sees the rest of the offer. Counters are
                    // cleared by the UI thread; here we just keep eating
                    // packets until the next non-clip packet (or queue
                    // drains naturally) — at which point we clear the
                    // flag so the next ClipOffer flows normally.
                    log::info!("clipboard.send CANCELLED — dropping queued clip packet");
                    continue;
                }
                if cancelling && !is_clip {
                    // Drained the cancelled batch. Re-arm for next transfer.
                    outgoing_cancel.store(false, Ordering::Release);
                }
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
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Queue empty. If a cancel was pending, the only way to
                // be here is that we've dropped every queued clip packet —
                // safe to clear the flag.
                if outgoing_cancel.load(Ordering::Acquire) {
                    outgoing_cancel.store(false, Ordering::Release);
                }
            }
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
#[allow(clippy::too_many_arguments)]
fn reader_thread(
    mut transport: Box<dyn Transport>,
    events_tx: mpsc::Sender<TransportEvent>,
    clipboard_state: clipboard::ClipboardState,
    incoming_progress: Arc<AtomicU64>,
    incoming_total: Arc<AtomicU64>,
    outgoing_progress: Arc<AtomicU64>,
    outgoing_total: Arc<AtomicU64>,
    receive_images: Arc<std::sync::atomic::AtomicBool>,
    receive_text: Arc<std::sync::atomic::AtomicBool>,
    incoming_cancel: Arc<std::sync::atomic::AtomicBool>,
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
        receive_images,
        receive_text,
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
                    if incoming_cancel.swap(false, Ordering::AcqRel) {
                        // Stale cancel from before this offer — clear and
                        // accept the new offer normally.
                    }
                    incoming_clip.on_offer(format, total_len);
                }
                Message::ClipChunk { index, data } => {
                    if incoming_cancel.load(Ordering::Acquire) {
                        log::info!("clipboard.recv CANCELLED — dropping chunk {index}");
                        incoming_clip.reset();
                        // Keep the flag set: more chunks from the same offer
                        // are still on the wire, drop them too. Cleared on
                        // next ClipOffer (above).
                        continue;
                    }
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
