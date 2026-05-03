//! Self-relaunch helper for the Save & Restart flow in Settings.
//!
//! Save & Restart applies config changes that the running session can't
//! pick up live (window size, serial port, baud, client name). Without
//! it the user has to `killall wiredesk-client` and re-launch from
//! Spotlight / dock — Save & Restart streamlines that into one click.
//!
//! Spawns a new instance, then exits the current process. We use
//! `std::process::exit` rather than going through eframe shutdown
//! because eframe owns the egui context and runloop; calling exit lets
//! the OS reclaim the serial port FD and any spawned threads.

#[cfg(target_os = "macos")]
pub fn restart_app() -> ! {
    // When running inside a `.app` bundle, relaunch via `open -n <bundle>`
    // so macOS handles Dock/Launch Services activation correctly. When
    // running the bare binary (cargo run / dev), spawn it directly.
    if let Ok(exe) = std::env::current_exe() {
        let bundle = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"));
        let spawned = if let Some(bundle_path) = bundle {
            std::process::Command::new("open")
                .arg("-n")
                .arg(bundle_path)
                .spawn()
                .is_ok()
        } else {
            std::process::Command::new(&exe).spawn().is_ok()
        };
        if !spawned {
            log::warn!("restart_app: failed to spawn replacement — exiting anyway");
        }
    } else {
        log::warn!("restart_app: current_exe() failed — exiting without relaunch");
    }
    std::process::exit(0);
}

#[cfg(not(target_os = "macos"))]
pub fn restart_app() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(&exe).spawn();
    }
    std::process::exit(0);
}
