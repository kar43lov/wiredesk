//! macOS presentation-options helpers — fully hide the menu bar and
//! Dock during fullscreen capture so they don't reveal-on-hover and
//! occlude the Host's top row.
//!
//! `NSApp.setPresentationOptions(.HideMenuBar | .HideDock | .FullScreen)`
//! is the standard kiosk-mode flag combination. Apple validates the
//! options — `HideMenuBar` requires `FullScreen` to be set too, else
//! it's silently ignored.
//!
//! On non-macOS builds this is all no-ops so the call sites in
//! `app.rs::toggle_fullscreen` stay clean of `cfg(...)`.
//!
//! `MainThreadMarker::new()` returns `Some` only on the main thread.
//! egui's update loop runs on the main thread (eframe enforces it),
//! so the unwrap is safe in this call path. If the call ever happens
//! off-thread we silently no-op rather than panicking — losing the
//! kiosk effect is preferable to crashing the UI.

#[cfg(target_os = "macos")]
pub fn enter_kiosk() {
    use objc2_app_kit::{NSApplication, NSApplicationPresentationOptions};
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("presentation::enter_kiosk: not on main thread, skipping");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let opts = NSApplicationPresentationOptions::NSApplicationPresentationHideMenuBar
        | NSApplicationPresentationOptions::NSApplicationPresentationHideDock
        | NSApplicationPresentationOptions::NSApplicationPresentationFullScreen;
    app.setPresentationOptions(opts);
}

#[cfg(target_os = "macos")]
pub fn exit_kiosk() {
    use objc2_app_kit::{NSApplication, NSApplicationPresentationOptions};
    use objc2_foundation::MainThreadMarker;

    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("presentation::exit_kiosk: not on main thread, skipping");
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.setPresentationOptions(NSApplicationPresentationOptions::NSApplicationPresentationDefault);
}

#[cfg(not(target_os = "macos"))]
pub fn enter_kiosk() {}

#[cfg(not(target_os = "macos"))]
pub fn exit_kiosk() {}
