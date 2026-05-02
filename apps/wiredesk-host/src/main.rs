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
    // `Option<SingleInstanceGuard>` so the Error fallback can simply hold
    // `None` — start without a lock rather than panic. Duplicate-launch is
    // a worse symptom than refusing to start, but on a check failure we'd
    // rather still come up.
    let _instance_guard: Option<ui::single_instance::SingleInstanceGuard> =
        match ui::single_instance::try_acquire_with_retry(
            "WireDeskHostSingleton",
            5,
            100,
        ) {
            ui::single_instance::SingleInstanceResult::Acquired(g) => Some(g),
            ui::single_instance::SingleInstanceResult::AlreadyRunning => {
                log::warn!("another wiredesk-host instance is already running");
                #[cfg(windows)]
                {
                    let _ = native_windows_gui::init();
                    native_windows_gui::simple_message(
                        "WireDesk",
                        "WireDesk Host is already running — check the tray icon.",
                    );
                }
                #[cfg(not(windows))]
                eprintln!("WireDesk Host is already running.");
                return;
            }
            ui::single_instance::SingleInstanceResult::Error(e) => {
                log::warn!("single-instance check failed: {e} (continuing without lock)");
                None
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
    let _session = session_thread::spawn(cfg.clone(), status_tx);

    let last_status = Arc::new(Mutex::new(SessionStatus::Waiting));

    #[cfg(windows)]
    run_windows(cfg, status_rx, last_status);

    #[cfg(not(windows))]
    run_dev_loop(status_rx, last_status);
}

#[cfg(not(windows))]
fn run_dev_loop(
    status_rx: mpsc::Receiver<SessionStatus>,
    last: Arc<Mutex<SessionStatus>>,
) {
    let _bridge = ui::status_bridge::spawn_no_notice(status_rx, last.clone());
    log::info!("session thread spawned; running dev-mode foreground loop (no tray)");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        if let Ok(g) = last.lock() {
            log::info!("session status: {}", g.label());
        }
    }
}

#[cfg(windows)]
fn run_windows(
    cfg: HostConfig,
    status_rx: mpsc::Receiver<SessionStatus>,
    last: Arc<Mutex<SessionStatus>>,
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

    // Wire events. nwg's full_bind_event_handler pushes events to a closure
    // that gets RawEvent + control handle; we dispatch manually.
    let tray_handle = tray.borrow().window.handle;
    let settings_handle = settings.borrow().window.handle;

    let tray_clone = tray.clone();
    let settings_clone = settings.clone();
    let last_clone = last.clone();

    // CRITICAL: don't hold a `tray_clone.borrow()` across the whole match —
    // `OnNotice` arm needs `borrow_mut()` to update the icon, and a second
    // borrow on a RefCell already-borrowed → panic → process abort. Take
    // the borrow lazily inside each arm.
    let event_handler = nwg::full_bind_event_handler(&tray_handle, move |evt, _evt_data, handle| {
        use nwg::Event as E;
        match evt {
            E::OnNotice => {
                // Status bridge fired — update tray icon + settings status.
                if let Ok(g) = last_clone.lock() {
                    if let Err(e) = tray_clone.borrow_mut().update_status(&g) {
                        log::warn!("tray icon update failed: {e}");
                    }
                    settings_clone.borrow_mut().set_status(&g);
                }
            }
            E::OnContextMenu => {
                let t = tray_clone.borrow();
                if handle == t.tray.handle {
                    t.show_popup();
                }
            }
            E::OnMenuItemSelected => {
                let t = tray_clone.borrow();
                if handle == t.menu_show_settings.handle {
                    drop(t);
                    settings_clone.borrow().show();
                } else if handle == t.menu_open_logs.handle {
                    t.open_logs();
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
            // borrow across the match arms.
            let (is_save, is_copy_mac, is_restart, is_detect) = {
                let s = settings_clone2.borrow();
                (
                    handle == s.save_btn.handle,
                    handle == s.copy_mac_btn.handle,
                    handle == s.restart_btn.handle,
                    handle == s.detect_btn.handle,
                )
            };
            match evt {
                E::OnButtonClick => {
                    if is_save {
                        let s = settings_clone2.borrow();
                        match s.read_form() {
                            Ok(new_cfg) => {
                                if let Err(e) = new_cfg.save() {
                                    s.set_message(&format!("Save failed: {e}"));
                                    return;
                                }
                                // Sync autostart with the checkbox.
                                let want_startup = new_cfg.run_on_startup;
                                let r = if want_startup {
                                    ui::autostart::enable()
                                } else {
                                    ui::autostart::disable()
                                };
                                if let Err(e) = r {
                                    s.set_message(&format!(
                                        "Saved, but autostart toggle failed: {e}"
                                    ));
                                } else {
                                    s.set_message("Saved. Restart WireDesk Host to apply.");
                                }
                                if let Ok(mut g) = cfg_holder.lock() {
                                    *g = new_cfg;
                                }
                            }
                            Err(e) => s.set_message(&e),
                        }
                    } else if is_copy_mac {
                        let snapshot = cfg_holder.lock().ok().map(|g| g.clone());
                        if let Some(c) = snapshot {
                            let cmd = ui::format::format_mac_launch_command(&c);
                            // `set_data_text` pumps the Win32 message loop —
                            // grab a fresh borrow only for the arguments and
                            // release before the call. Then re-borrow for the
                            // status message.
                            {
                                let s = settings_clone2.borrow();
                                nwg::Clipboard::set_data_text(&s.window, &cmd);
                            }
                            settings_clone2
                                .borrow()
                                .set_message("Copied Mac launch command to clipboard.");
                        }
                    } else if is_restart {
                        // Save & Restart: persist config + autostart, then
                        // spawn a fresh host process and stop our own event
                        // loop. The new process retries the single-instance
                        // mutex acquire (5×100ms in main.rs) so it'll wait
                        // out our shutdown without an artificial sleep here.
                        let s = settings_clone2.borrow();
                        match s.read_form() {
                            Ok(new_cfg) => {
                                if let Err(e) = new_cfg.save() {
                                    s.set_message(&format!("Save failed: {e}"));
                                    return;
                                }
                                let want_startup = new_cfg.run_on_startup;
                                let r = if want_startup {
                                    ui::autostart::enable()
                                } else {
                                    ui::autostart::disable()
                                };
                                if let Err(e) = r {
                                    s.set_message(&format!(
                                        "Saved, but autostart toggle failed: {e}"
                                    ));
                                    return;
                                }
                                // Only update cfg_holder *after* spawn confirms —
                                // if spawn fails, the running process keeps
                                // serving the old config, so copy_mac_btn must
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
                                            s.set_message(&format!(
                                                "Saved, but restart failed to spawn: {e}"
                                            ));
                                        }
                                    },
                                    Err(e) => {
                                        s.set_message(&format!(
                                            "Saved, but couldn't find own exe path: {e}"
                                        ));
                                    }
                                }
                            }
                            Err(e) => s.set_message(&e),
                        }
                    } else if is_detect {
                        // Enumerate USB serial ports and pick the lone CH340
                        // (or report empty / multi). On enumeration failure we
                        // treat it as "no ports" — the user can still type a
                        // port manually.
                        let ports = serialport::available_ports().unwrap_or_else(|e| {
                            log::warn!("serialport::available_ports failed: {e}");
                            Vec::new()
                        });
                        let s = settings_clone2.borrow();
                        match ui::format::detect_ch340_port(&ports) {
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
                                s.set_message(
                                    "No CH340/CH341 detected. Plug the cable in and retry.",
                                );
                            }
                        }
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
}

