mod app;
mod clipboard;
mod clipboard_files;
mod config;
mod exec_bridge;
#[cfg(target_os = "macos")]
mod ipc;
/// End-to-end interactive-IPC round-trip test (Task 9). Test-only — the crate
/// is binary-only, so this integ suite lives in-tree instead of `tests/`.
#[cfg(all(test, target_os = "macos"))]
mod interactive_ipc_e2e;
mod input;
mod keyboard_tap;
mod link;
mod logging;
mod monitor;
mod restart;
mod shell_channel;
mod status_bar;

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::mpsc;
use std::time::Duration;

use clap::{CommandFactory, Parser};
use eframe::egui;

use app::WireDeskApp;
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

    /// Transport to use: `serial` (USB-Serial null-modem, default) or
    /// `bluetooth` (BLE Central scanning for the WireDesk host). Overrides
    /// config.toml.
    #[arg(long, default_value = "serial")]
    transport: String,
}

fn main() {
    // Daily-rolling file logging at ~/Library/Application Support/WireDesk/client.log
    // plus stderr layer (visible when launched from terminal). Worker guard is
    // tied to `_log_guard`'s lifetime — drop = flush, so we hold it for all of
    // `main()`. Falls back to stderr-only if the file can't be opened (read-only
    // home, permissions error etc) so we never silently lose logs at boot.
    let _log_guard = match logging::init_logging() {
        Ok(g) => {
            logging::install_panic_hook();
            Some(g)
        }
        Err(e) => {
            // File appender unavailable (read-only home, perms, etc) — fall
            // back to a stderr-only subscriber so `log::*` calls aren't
            // silently dropped. Without this fallback we'd be quieter than
            // the pre-tracing env_logger setup.
            eprintln!("warning: file logging unavailable ({e}); logs will go to stderr only");
            logging::init_logging_stderr_only();
            logging::install_panic_hook();
            None
        }
    };

    // Resolve config: defaults → config.toml → CLI args (override).
    let toml_cfg = ClientConfig::load();
    let matches = Args::command().get_matches();
    let cfg = config::merge_args(&matches, toml_cfg);

    log::info!("WireDesk Client");
    log::info!("log dir: {}", logging::log_dir().display());
    log::info!("config: {}", ClientConfig::config_path().display());
    log::info!("transport: {} (port={} baud={})", cfg.transport, cfg.port, cfg.baud);

    // Cache vacuum: clear stale inbound-file cache entries older than 24h.
    // Runs synchronously — the directory is small (≤20 MB per file × few
    // entries), enumeration is fast, and doing it before any IO thread
    // spins up means a file-paste landing within the first poll tick
    // never races a half-finished vacuum on the same directory.
    clipboard::run_startup_vacuum(Duration::from_secs(24 * 3600));

    let transport_cfg = config::to_transport_config(&cfg);

    // Channels go up first so we can ship a Disconnected event into the
    // UI on transport-open failure without crashing the process. The
    // user *needs* the Settings panel reachable to switch transports
    // (e.g., flip from `bluetooth` back to `serial` after the Win host
    // reverted to a wired link) — `process::exit(1)` here would lock
    // them out of the recovery path.
    let (events_tx, events_rx) = mpsc::channel();
    let (outgoing_tx, outgoing_rx) = mpsc::channel();
    let (tap_events_tx, tap_events_rx) = mpsc::channel();
    // Reconnect-request channel + link-up flag drive the LinkSupervisor.
    // The UI thread pushes `()` here on every Disconnected event; the
    // supervisor reopens the transport (with backoff) and respawns the
    // reader/writer pair. The initial open at startup goes through the same
    // path — see the `reconnect_request_tx.send(())` kick below.
    let (reconnect_request_tx, reconnect_request_rx) = mpsc::channel::<()>();
    let link_up = Arc::new(std::sync::atomic::AtomicBool::new(false));

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
    // Task 7d: filename slot for the outgoing FORMAT_FILE transfer. Set
    // by the poll thread before `emit_offer_and_chunks(FORMAT_FILE, ...)`,
    // cleared by `apply_outgoing_progress_with_label` on DONE. UI reads it
    // to switch the status-line from "Sending clipboard" → "Sending file
    // 'X.pdf' — ...". Empty string == fall back to generic label.
    let current_outgoing_label = Arc::new(std::sync::Mutex::new(String::new()));

    // Runtime image-clipboard toggles (Settings panel). Initial values come
    // from the loaded config; UI flips them at runtime — no restart required.
    // Text clipboard is unaffected.
    let send_images = Arc::new(std::sync::atomic::AtomicBool::new(cfg.send_images));
    let receive_images = Arc::new(std::sync::atomic::AtomicBool::new(cfg.receive_images));
    let send_text = Arc::new(std::sync::atomic::AtomicBool::new(cfg.send_text));
    let receive_text = Arc::new(std::sync::atomic::AtomicBool::new(cfg.receive_text));
    // Files runtime toggle. Task 8: wired through ClientConfig +
    // Settings UI — flipping the checkbox calls `store()` here without a
    // session restart, parallel to `receive_images` behaviour. Default
    // value sourced from `cfg.receive_files` (default-on for back-compat
    // with pre-Task-8 TOML configs).
    let receive_files =
        Arc::new(std::sync::atomic::AtomicBool::new(cfg.receive_files));
    // Outbound file toggle. Opt-in (default false): a Cmd+C on a file only
    // reaches the wire when the user explicitly enables "Send files" in
    // Settings. Shared with the poll thread, flipped live by the checkbox.
    let send_files = Arc::new(std::sync::atomic::AtomicBool::new(cfg.send_files));
    // Karabiner-Elements `left_command ↔ left_option` compensation (see
    // ClientConfig::swap_option_command). Read once on startup and surfaced
    // through Settings; flipping the checkbox at runtime takes effect on the
    // next FlagsChanged / KeyDown the tap sees.
    let swap_option_command = Arc::new(std::sync::atomic::AtomicBool::new(cfg.swap_option_command));

    // Cancel atomics — both reader and writer observe them to drop in-flight
    // clipboard packets when the UI hits the Cancel button. Shared with the
    // app and (via LinkContext) every reader/writer spawned across reconnects.
    let outgoing_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let incoming_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Shell-event broadcast slot for the IPC handler (Task 6). None until a
    // `wd --exec` connection arrives; the reader thread checks it on every
    // shell event and fans out a parallel copy when set. Shared (Arc) with
    // every reader spawned across reconnects via LinkContext.
    let exec_slot: exec_bridge::ExecEventSlot = Arc::new(std::sync::Mutex::new(None));

    // Shared host-info cache: populated by the reader on `HelloAck`, cleared
    // on link-down. The interactive-`wd`-over-IPC relay (Task 6/7) reads it to
    // synth an accurate `HelloAck` for a term that connects after the GUI
    // already handshook. Cloned into the IPC acceptor in Task 7.
    let host_info: link::SharedHostInfo = Arc::new(std::sync::Mutex::new(None));

    // Spawn the IPC acceptor so `wd --exec` can run in parallel with an
    // active GUI (Mac-only — non-Mac builds are unsupported by design, but the
    // cfg keeps cross-compilation working). Bind failure is non-fatal: GUI
    // continues, term falls back to direct serial.
    #[cfg(target_os = "macos")]
    {
        let ipc_outgoing_tx = outgoing_tx.clone();
        let ipc_slot = exec_slot.clone();
        // Shared single-owner lock for the host's one shell slot. The exec and
        // interactive handlers fail-fast cross-kind against it (see
        // shell_channel); exec-vs-exec FIFO stays nested under `single_inflight`.
        let shell_owner = shell_channel::new_shared_owner();
        let single_inflight: Arc<std::sync::Mutex<()>> =
            Arc::new(std::sync::Mutex::new(()));
        let ipc_host_info = host_info.clone();
        let socket_path = wiredesk_exec_core::default_socket_path();
        let ipc_link_up = link_up.clone();
        ipc::spawn_ipc_acceptor(
            socket_path,
            ipc_outgoing_tx,
            ipc_slot,
            shell_owner,
            single_inflight,
            ipc_host_info,
            ipc_link_up,
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = &exec_slot; // suppress unused on non-Mac
    }

    // LinkContext bundles every shared value the reader/writer threads need
    // and that must survive a reconnect. The supervisor clones it per link.
    let link_ctx = link::LinkContext {
        client_name: cfg.client_name.clone(),
        clipboard_state: clipboard_state.clone(),
        outgoing_progress: outgoing_progress.clone(),
        outgoing_total: outgoing_total.clone(),
        incoming_progress: incoming_progress.clone(),
        incoming_total: incoming_total.clone(),
        receive_images: receive_images.clone(),
        receive_text: receive_text.clone(),
        receive_files: receive_files.clone(),
        incoming_cancel: incoming_cancel.clone(),
        outgoing_cancel: outgoing_cancel.clone(),
        exec_slot: exec_slot.clone(),
        current_outgoing_label: current_outgoing_label.clone(),
        reader_outgoing_tx: outgoing_tx.clone(),
        link_up: link_up.clone(),
        host_info: host_info.clone(),
    };

    // Spawn the link supervisor. It owns the reader/writer pair and reopens
    // the transport (with 1s→30s backoff) on every reconnect request. The
    // initial open is just the first request, sent right after. The UI boots
    // regardless of whether the transport is up — recovery via Settings stays
    // reachable (memory: feedback_ui_recovery_on_transport_failure).
    {
        let supervisor_transport_cfg = transport_cfg.clone();
        let supervisor_events_tx = events_tx.clone();
        let open_fn =
            move || wiredesk_transport::open_transport(&supervisor_transport_cfg);
        link::spawn_supervisor(
            open_fn,
            link::backoff_delay,
            outgoing_rx,
            supervisor_events_tx,
            reconnect_request_rx,
            link_ctx,
        );
    }
    // Kick the initial connection through the supervisor path.
    let _ = reconnect_request_tx.send(());

    // Clone for the clipboard poll thread (surfaces oversize-image toasts).
    let poll_events_tx = events_tx.clone();

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
    // Wake-up channel: keyboard tap nudges the poll thread on synthetic
    // Cmd+V so we don't wait the full poll interval before noticing the
    // clipboard write Whispr Flow just made.
    let (poll_kick_tx, poll_kick_rx) = std::sync::mpsc::channel::<()>();

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
        send_files.clone(),
        outgoing_text_in_flight.clone(),
        poll_kick_rx,
        current_outgoing_label.clone(),
    );

    // Synthetic dispatcher thread — see comment above. Holds each combo
    // while a clipboard sync is in flight (max 2 s), then waits a short
    // grace for Host to commit before emitting on the wire.
    {
        let outgoing_tx = outgoing_tx.clone();
        let in_flight = outgoing_text_in_flight.clone();
        std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            // Wider envelope than the original guess: Whispr's cloud
            // round-trip can stretch the gap between text-write and
            // Cmd+V well past 1 s, and the grace must outlast both Mac
            // poll period and Host commit. 4 s wait + 400 ms grace
            // covers ~99 % of dictation runs at 11 KB/s wire.
            const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(4);
            const GRACE: std::time::Duration = std::time::Duration::from_millis(400);
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
        poll_kick_tx,
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

    let mut app = WireDeskApp::new(
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
        receive_files,
        send_files,
        swap_option_command,
        outgoing_cancel,
        incoming_cancel,
        current_outgoing_label,
    );
    // Let the UI ask the supervisor to reconnect on each Disconnected event.
    app.set_reconnect_request_tx(reconnect_request_tx);

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
            // On macOS the handle pins the NSStatusItem for the program's
            // lifetime — leak it so AppKit keeps the menu-bar item alive. On
            // other targets the handle is an empty no-Drop placeholder, so
            // there's nothing to keep alive (and `mem::forget` on a non-Drop
            // type is a no-op clippy rejects under `-D warnings`).
            #[cfg(target_os = "macos")]
            std::mem::forget(_handle);
            #[cfg(not(target_os = "macos"))]
            let _ = _handle;
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
