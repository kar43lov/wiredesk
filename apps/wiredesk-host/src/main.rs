// Hide the console window on Windows — the host runs as a background tray
// agent and there's nothing to see in stdout anyway (logs go to
// %APPDATA%\WireDesk\host.log via tracing-appender). Applied to both
// debug and release: cfg_attr with windows_subsystem and debug_assertions
// is finicky in the linker — being unconditional avoids surprises.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod clipboard;
mod config;
mod injector;
mod logging;
mod session;
mod session_thread;
mod shell;
mod ui;

use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use clap::{CommandFactory, Parser};

use clipboard::ProgressCounters;
use config::HostConfig;
use session_thread::SessionStatus;

#[derive(Parser)]
#[command(name = "wiredesk-host", about = "WireDesk host agent")]
pub struct Args {
    /// Serial port (e.g., COM3 on Windows, /dev/ttyUSB0 on Linux). Overrides config.toml.
    #[arg(short, long, default_value = "COM3")]
    port: String,

    /// Baud rate. Overrides config.toml.
    #[arg(short, long, default_value = "115200")]
    baud: u32,

    /// Host display name. Overrides config.toml.
    #[arg(long, default_value = "wiredesk-host")]
    name: String,

    /// Screen width. Overrides config.toml.
    #[arg(long, default_value = "2560")]
    width: u16,

    /// Screen height. Overrides config.toml.
    #[arg(long, default_value = "1440")]
    height: u16,
}

fn main() {
    let _log_guard = match logging::init_logging() {
        Ok(g) => {
            logging::install_panic_hook();
            Some(g)
        }
        Err(e) => {
            eprintln!("warning: file logging unavailable ({e}); logs will be lost");
            None
        }
    };

    // Single-instance lock — second launch shows a message box and exits.
    // On non-Windows targets this is a no-op (always Acquired).
    //
    // 5 attempts × 100ms (~500ms total budget) — covers the Save & Restart
    // race where the freshly spawned process may briefly overlap with the
    // outgoing one before the old process drops its mutex handle.
    //
    // Fail closed on `Error`: a `CreateMutexW` failure is rare in practice
    // (would need an OS-level resource exhaustion or a malformed name) but
    // when it does happen, silently starting without a lock can race a
    // legitimate first instance over the tray icon and the serial port —
    // both visible-bad failure modes. Better to surface the error and
    // refuse to start than to fan out into duplicate hosts.
    let _instance_guard = match ui::single_instance::try_acquire_with_retry(
        "WireDeskHostSingleton",
        5,
        100,
    ) {
            ui::single_instance::SingleInstanceResult::Acquired(g) => g,
            ui::single_instance::SingleInstanceResult::AlreadyRunning => {
                log::warn!("another wiredesk-host instance is already running");
                // Instead of nagging with a message box, ask the running
                // process to surface its Settings window so a double-click
                // on the .exe behaves like "open the app I already have".
                #[cfg(windows)]
                {
                    let _ = ui::single_instance::signal_show_settings(
                        ui::single_instance::SHOW_SETTINGS_EVENT_NAME,
                    );
                }
                #[cfg(not(windows))]
                eprintln!("WireDesk Host is already running.");
                return;
            }
            ui::single_instance::SingleInstanceResult::Error(e) => {
                log::error!("single-instance check failed: {e}");
                #[cfg(windows)]
                {
                    let _ = native_windows_gui::init();
                    native_windows_gui::simple_message(
                        "WireDesk",
                        &format!(
                            "WireDesk Host failed to acquire single-instance lock: {e}\n\n\
                             Refusing to start to avoid running two hosts at once."
                        ),
                    );
                }
                #[cfg(not(windows))]
                eprintln!("WireDesk Host single-instance check failed: {e}");
                return;
            }
        };

    // Resolve config: defaults → config.toml → CLI args (override).
    let toml_cfg = HostConfig::load();
    let matches = Args::command().get_matches();
    let cfg = config::merge_args(&matches, toml_cfg);

    log::info!("WireDesk Host Agent");
    log::info!("log dir: {}", logging::log_dir().display());
    log::info!("config: {}", HostConfig::config_path().display());
    log::info!("serial: {} @ {} baud", cfg.port, cfg.baud);
    log::info!("screen: {}x{}", cfg.width, cfg.height);

    let (status_tx, status_rx) = mpsc::channel();

    // Shared progress atomics — session thread writes; overlay UI thread
    // (Windows) reads. Default-initialised so dev loop on macOS still gets
    // a valid bundle (no overlay there, but Session needs the structure).
    let counters = ProgressCounters::default();
    let _session = session_thread::spawn(cfg.clone(), status_tx, counters.clone());

    let last_status = Arc::new(Mutex::new(ui::status_bridge::StatusState::default()));

    #[cfg(windows)]
    run_windows(cfg, status_rx, last_status, counters);

    #[cfg(not(windows))]
    {
        let _ = counters; // unused in dev loop, keep clone for symmetry
        run_dev_loop(status_rx, last_status);
    }
}

#[cfg(not(windows))]
fn run_dev_loop(
    status_rx: mpsc::Receiver<SessionStatus>,
    last: Arc<Mutex<ui::status_bridge::StatusState>>,
) {
    let _bridge = ui::status_bridge::spawn_no_notice(status_rx, last.clone());
    log::info!("session thread spawned; running dev-mode foreground loop (no tray)");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        if let Ok(g) = last.lock() {
            log::info!(
                "session status: {}",
                g.persistent.to_session_status().label()
            );
        }
    }
}

#[cfg(windows)]
fn run_windows(
    cfg: HostConfig,
    status_rx: mpsc::Receiver<SessionStatus>,
    last: Arc<Mutex<ui::status_bridge::StatusState>>,
    counters: ProgressCounters,
) {
    use native_windows_gui as nwg;

    fn fatal(stage: &str, err: impl std::fmt::Display) {
        log::error!("FATAL @ {stage}: {err}");
        // MessageBox so the user sees something even when the tray icon
        // failed to appear (or appeared and vanished).
        nwg::simple_message(
            "WireDesk Host — startup failed",
            &format!("{stage}: {err}"),
        );
    }

    log::info!("run_windows: nwg::init");
    if let Err(e) = nwg::init() {
        fatal("nwg::init", e);
        return;
    }

    log::info!("run_windows: setting default font (Segoe UI 16px)");
    // Segoe UI is the standard Win11 dialog font. nwg's Font::size is in
    // pixels, not points — 16px ≈ 9pt at 96 DPI, matching the system default.
    // Set this BEFORE building any windows so all controls inherit it.
    let mut font = nwg::Font::default();
    if let Err(e) = nwg::Font::builder()
        .family("Segoe UI")
        .size(16)
        .build(&mut font)
    {
        log::warn!("Segoe UI font builder failed: {e}; falling back to system default");
    } else {
        let _ = nwg::Font::set_global_default(Some(font));
    }

    let log_dir = logging::log_dir();

    log::info!("run_windows: building TrayUi (log_dir={})", log_dir.display());
    let tray = match ui::tray::TrayUi::build(log_dir) {
        Ok(t) => t,
        Err(e) => {
            fatal("TrayUi::build", e);
            return;
        }
    };

    log::info!("run_windows: building SettingsWindow");
    let settings = match ui::settings_window::SettingsWindow::build(&cfg) {
        Ok(s) => s,
        Err(e) => {
            fatal("SettingsWindow::build", e);
            return;
        }
    };

    // TransferOverlay disabled — even when hidden, the topmost popup
    // window interfered with z-order/focus on the user's setup (Total
    // Commander couldn't be activated, mouse input froze when TC took
    // focus). The progress UI lives on the Mac client only for now;
    // host-side surfacing is reserved for a follow-up that uses a
    // less-intrusive mechanism (tray tooltip update, balloon
    // notification, or a non-topmost minimal status window).
    //
    // Counters keep flowing through `ProgressCounters` for the Mac
    // status bar / progress bar to consume — this only removes the
    // host's own visualization.
    log::info!("run_windows: TransferOverlay disabled (focus interference workaround)");
    let _ = &counters; // silence unused-variable when overlay is off
    let overlay: Option<std::rc::Rc<std::cell::RefCell<ui::transfer_overlay::TransferOverlay>>> =
        None;
    let _overlay_event_handler: Option<nwg::EventHandler> = None;
    let _ = (&overlay, &_overlay_event_handler);

    log::info!("run_windows: building cross-thread Notice");
    let mut notice = nwg::Notice::default();
    if let Err(e) = nwg::Notice::builder()
        .parent(&tray.borrow().window)
        .build(&mut notice)
    {
        fatal("Notice::build", e);
        return;
    }

    let _bridge = ui::status_bridge::spawn(status_rx, last.clone(), notice.sender());

    // Cross-process "show settings" pipe: a second-instance launch
    // fires SetEvent on the named event; this thread blocks on
    // WaitForSingleObject and pokes the existing status-bridge
    // Notice via `notice.sender().notice()` after raising a shared
    // pending flag. The OnNotice handler then checks both the
    // status-bridge state AND the pending flag.
    //
    // We piggyback on the existing Notice instead of creating a
    // second one because nwg 1.0.13 panics on a second Notice
    // anywhere in the same window tree (observed on both
    // MessageWindow and a regular Window as parent — the bind
    // check rejects "Cannot bind control with an handle of type").
    let show_settings_pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
    #[cfg(windows)]
    let _show_event_thread = {
        match ui::single_instance::create_show_settings_event(
            ui::single_instance::SHOW_SETTINGS_EVENT_NAME,
        ) {
            Some(handle) => {
                let sender = notice.sender();
                let pending = show_settings_pending.clone();
                Some(std::thread::spawn(move || loop {
                    if !handle.wait() {
                        break;
                    }
                    pending.store(true, std::sync::atomic::Ordering::Release);
                    sender.notice();
                }))
            }
            None => {
                log::warn!("create_show_settings_event failed — second-instance launch will be silent");
                None
            }
        }
    };

    // Wire events. nwg's full_bind_event_handler pushes events to a closure
    // that gets RawEvent + control handle; we dispatch manually.
    let tray_handle = tray.borrow().window.handle;
    let settings_handle = settings.borrow().window.handle;

    let tray_clone = tray.clone();
    let settings_clone = settings.clone();
    let last_clone = last.clone();
    let show_settings_pending_clone = show_settings_pending.clone();

    // Tray icon does NOT raise WM_LBUTTONDBLCLK as a distinct nwg event —
    // it only delivers WM_LBUTTONUP / WM_LBUTTONDOWN as
    // `OnMousePress(MousePressLeftUp/Down)`. Track the previous left-up
    // timestamp and treat two left-ups within `DOUBLE_CLICK_WINDOW` as a
    // double-click. Win32's default double-click window is 500 ms (via
    // `GetDoubleClickTime`); we hard-code that here — querying the API
    // would just add a syscall to a hot path that's already user-driven.
    use std::cell::Cell;
    use std::time::{Duration, Instant};
    const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);
    let last_left_up_for_handler: Cell<Option<Instant>> = Cell::new(None);

    // CRITICAL: don't hold a `tray_clone.borrow()` across the whole match —
    // `OnNotice` arm needs `borrow_mut()` to update the icon, and a second
    // borrow on a RefCell already-borrowed → panic → process abort. Take
    // the borrow lazily inside each arm.
    let event_handler = nwg::full_bind_event_handler(&tray_handle, move |evt, _evt_data, handle| {
        use nwg::Event as E;
        use nwg::MousePressEvent as MP;
        match evt {
            E::OnNotice => {
                // Status bridge fired. The Notice multiplexes three signals:
                //   1) cross-process "show settings" (set by the wait
                //      thread for the named event when a second-instance
                //      launch fires SetEvent),
                //   2) pending notification (transient balloon, doesn't
                //      change persistent UI state),
                //   3) persistent status (tray icon color + settings row).
                // Handle (1) first so a stacked event still surfaces it,
                // then drop into the existing notification + persistent
                // pipeline.
                if show_settings_pending_clone
                    .swap(false, std::sync::atomic::Ordering::AcqRel)
                {
                    settings_clone.borrow().show();
                }
                let notification = if let Ok(mut g) = last_clone.lock() {
                    g.pending_notification.take()
                } else {
                    None
                };
                if let Some(msg) = notification {
                    if let Err(e) = tray_clone
                        .borrow_mut()
                        .update_status(&SessionStatus::Notification(msg))
                    {
                        log::warn!("tray balloon failed: {e}");
                    }
                }
                let persistent = last_clone
                    .lock()
                    .ok()
                    .map(|g| g.persistent.to_session_status());
                if let Some(s) = persistent {
                    if let Err(e) = tray_clone.borrow_mut().update_status(&s) {
                        log::warn!("tray icon update failed: {e}");
                    }
                    settings_clone.borrow_mut().set_status(&s);
                }
            }
            E::OnContextMenu => {
                let t = tray_clone.borrow();
                if handle == t.tray.handle {
                    t.show_popup();
                }
            }
            E::OnMousePress(MP::MousePressLeftUp) => {
                // Synthetic double-click detection. Only react when the
                // event came from the tray icon — `MousePressLeftUp` also
                // fires for left-clicks on the popup menu host window.
                let is_tray = {
                    let t = tray_clone.borrow();
                    handle == t.tray.handle
                };
                if !is_tray {
                    return;
                }
                let now = Instant::now();
                let prev = last_left_up_for_handler.replace(Some(now));
                let is_double = matches!(prev, Some(p) if now.duration_since(p) <= DOUBLE_CLICK_WINDOW);
                if is_double {
                    // Reset so a third quick click doesn't immediately
                    // count as another double-click.
                    last_left_up_for_handler.set(None);
                    settings_clone.borrow().show();
                }
            }
            E::OnMenuItemSelected => {
                let t = tray_clone.borrow();
                if handle == t.menu_show_settings.handle {
                    drop(t);
                    settings_clone.borrow().show();
                } else if handle == t.menu_open_logs.handle {
                    t.open_logs();
                } else if handle == t.menu_restart.handle {
                    drop(t);
                    // Spawn a fresh host process, then ask the current
                    // event loop to exit. Same pattern as Save & Restart
                    // in the Settings window — single-instance retry-loop
                    // covers the brief window where both processes hold
                    // the named mutex.
                    match std::env::current_exe() {
                        Ok(exe) => match std::process::Command::new(exe).spawn() {
                            Ok(_) => {
                                log::info!("restart: spawned new host process from tray");
                                nwg::stop_thread_dispatch();
                            }
                            Err(e) => {
                                log::warn!("restart from tray: spawn failed: {e}");
                            }
                        },
                        Err(e) => {
                            log::warn!("restart from tray: current_exe failed: {e}");
                        }
                    }
                } else if handle == t.menu_quit.handle {
                    nwg::stop_thread_dispatch();
                }
            }
            _ => {}
        }
    });

    // Wire settings-window events.
    //
    // CRITICAL: never hold a `settings_clone2.borrow()` across the whole
    // match. The OnNotice tray handler in the *other* event handler does
    // `settings_clone.borrow_mut().set_status(&g)` — Win32 message pumping
    // inside e.g. `nwg::Clipboard::set_data_text` can re-enter and trigger
    // OnNotice while we're mid-click. A second borrow_mut on a RefCell
    // already-borrowed → panic → process abort. Take the borrow lazily
    // inside each `if handle == ...` arm so it's released before any nwg
    // call that pumps messages.
    let settings_clone2 = settings.clone();
    let cfg_holder = Arc::new(Mutex::new(cfg.clone()));
    let settings_event_handler =
        nwg::full_bind_event_handler(&settings_handle, move |evt, _evt_data, handle| {
            use nwg::Event as E;
            // Resolve which control fired this event without holding any
            // borrow across the match arms — the handlers below take their
            // own borrows internally.
            //
            // CRITICAL: use `try_borrow()` instead of `borrow()`. nwg's
            // `set_status` / `set_text` calls during the OnNotice tray
            // handler (which holds `settings_clone.borrow_mut()`) pump Win32
            // messages, and a re-entrant settings event arrives mid-pump
            // → second borrow on a borrowed RefCell → panic → process
            // crash. Bailing out of the re-entrant event is harmless: nwg
            // will re-fire the original interaction once the outer borrow
            // drops, OR the event was a phantom (e.g., focus shifts during
            // status-bar updates) we don't need to handle.
            let probe = match settings_clone2.try_borrow() {
                Ok(s) => (
                    handle == s.save_btn.handle,
                    handle == s.copy_mac_btn.handle,
                    handle == s.restart_btn.handle,
                    handle == s.detect_btn.handle,
                    handle == s.quit_btn.handle,
                ),
                Err(_) => {
                    log::debug!(
                        "settings_event_handler skipped: settings RefCell busy (re-entrant)"
                    );
                    return;
                }
            };
            let (is_save, is_copy_mac, is_restart, is_detect, is_quit) = probe;
            match evt {
                E::OnButtonClick => {
                    if is_save {
                        handle_save(&settings_clone2, &cfg_holder);
                    } else if is_copy_mac {
                        handle_copy_mac(&settings_clone2, &cfg_holder);
                    } else if is_restart {
                        handle_restart(&settings_clone2, &cfg_holder);
                    } else if is_detect {
                        handle_detect(&settings_clone2);
                    } else if is_quit {
                        // Same effect as the tray's Quit menu — drop out
                        // of the nwg event loop. Drop'ed guards (mutex,
                        // session thread join handle, etc.) clean up.
                        nwg::stop_thread_dispatch();
                    }
                }
                E::OnWindowClose => {
                    settings_clone2.borrow().hide();
                }
                _ => {}
            }
        });

    log::info!("entering nwg event loop");
    nwg::dispatch_thread_events();
    log::info!("nwg event loop exited");

    nwg::unbind_event_handler(&event_handler);
    nwg::unbind_event_handler(&settings_event_handler);
    if let Some(h) = _overlay_event_handler.as_ref() {
        nwg::unbind_event_handler(h);
    }
    let _ = overlay; // keep alive until shutdown
}

// ---- Settings-window button handlers --------------------------------------
//
// Each handler owns its own borrow lifecycle so the event-loop closure stays
// a pure dispatch. They read the form, persist config / autostart, and write
// status back via `set_message` — the same flow as before, just lifted out
// of a 150-line `match` arm. Shared invariant: never hold a `borrow()` of
// `settings` across an nwg call that pumps the Win32 message loop (e.g.
// `Clipboard::set_data_text`), because the OnNotice tray handler may
// re-enter `borrow_mut()` and abort the process.

#[cfg(windows)]
fn handle_save(
    settings: &std::rc::Rc<std::cell::RefCell<ui::settings_window::SettingsWindow>>,
    cfg_holder: &Arc<Mutex<HostConfig>>,
) {
    let s = settings.borrow();
    match s.read_form() {
        Ok(new_cfg) => {
            if let Err(e) = new_cfg.save() {
                s.set_message(&format!("Save failed: {e}"));
                return;
            }
            // Sync autostart with the checkbox.
            let r = if new_cfg.run_on_startup {
                ui::autostart::enable()
            } else {
                ui::autostart::disable()
            };
            if let Err(e) = r {
                s.set_message(&format!("Saved, but autostart toggle failed: {e}"));
            } else {
                s.set_message("Saved. Restart WireDesk Host to apply.");
            }
            if let Ok(mut g) = cfg_holder.lock() {
                *g = new_cfg;
            }
        }
        Err(e) => s.set_message(&e),
    }
}

#[cfg(windows)]
fn handle_copy_mac(
    settings: &std::rc::Rc<std::cell::RefCell<ui::settings_window::SettingsWindow>>,
    cfg_holder: &Arc<Mutex<HostConfig>>,
) {
    use native_windows_gui as nwg;
    let snapshot = cfg_holder.lock().ok().map(|g| g.clone());
    if let Some(c) = snapshot {
        let cmd = ui::format::format_mac_launch_command(&c);
        // `set_data_text` pumps the Win32 message loop — grab a fresh
        // borrow only for the arguments and release before the call. Then
        // re-borrow for the status message.
        {
            let s = settings.borrow();
            nwg::Clipboard::set_data_text(&s.window, &cmd);
        }
        settings
            .borrow()
            .set_message("Copied Mac launch command to clipboard.");
    }
}

#[cfg(windows)]
fn handle_restart(
    settings: &std::rc::Rc<std::cell::RefCell<ui::settings_window::SettingsWindow>>,
    cfg_holder: &Arc<Mutex<HostConfig>>,
) {
    use native_windows_gui as nwg;
    // Save & Restart: persist config + autostart, then spawn a fresh host
    // process and stop our own event loop. The new process retries the
    // single-instance mutex acquire (5×100ms in main.rs) so it'll wait
    // out our shutdown without an artificial sleep here.
    let s = settings.borrow();
    let new_cfg = match s.read_form() {
        Ok(c) => c,
        Err(e) => {
            s.set_message(&e);
            return;
        }
    };
    if let Err(e) = new_cfg.save() {
        s.set_message(&format!("Save failed: {e}"));
        return;
    }
    let r = if new_cfg.run_on_startup {
        ui::autostart::enable()
    } else {
        ui::autostart::disable()
    };
    if let Err(e) = r {
        s.set_message(&format!("Saved, but autostart toggle failed: {e}"));
        return;
    }
    // Only update cfg_holder *after* spawn confirms — if spawn fails, the
    // running process keeps serving the old config, so copy_mac_btn must
    // also keep formatting the old command.
    match std::env::current_exe() {
        Ok(exe) => match std::process::Command::new(exe).spawn() {
            Ok(_) => {
                log::info!("restart: spawned new host process");
                if let Ok(mut g) = cfg_holder.lock() {
                    *g = new_cfg;
                }
                nwg::stop_thread_dispatch();
            }
            Err(e) => {
                s.set_message(&format!("Saved, but restart failed to spawn: {e}"));
            }
        },
        Err(e) => {
            s.set_message(&format!("Saved, but couldn't find own exe path: {e}"));
        }
    }
}

#[cfg(windows)]
fn handle_detect(
    settings: &std::rc::Rc<std::cell::RefCell<ui::settings_window::SettingsWindow>>,
) {
    let s = settings.borrow();
    match ui::format::detect_serial_port_now() {
        ui::format::DetectResult::Found(name) => {
            s.port_input.set_text(&name);
            s.set_message(&format!("Detected CH340 on {name}."));
        }
        ui::format::DetectResult::Multiple(names) => {
            s.set_message(&format!(
                "Multiple CH340 found: {} — pick one.",
                names.join(", ")
            ));
        }
        ui::format::DetectResult::NotFound => {
            s.set_message("No CH340/CH341 detected. Plug the cable in and retry.");
        }
        ui::format::DetectResult::EnumerationFailed(msg) => {
            // The OS port-enumeration API failed (driver issue, permission
            // denied, etc.) — surface the underlying error instead of the
            // misleading "No CH340 detected" message.
            s.set_message(&format!("Port enumeration failed: {msg}"));
        }
    }
}

