// UI layer for the Windows host. Most of the surface lives behind
// `#[cfg(windows)]` because nwg only compiles there; only `format`,
// `autostart`, `single_instance`, and `status_bridge` are cross-platform
// so the validation / glue logic can be unit-tested on macOS.
//
// On non-Windows targets the `format::*` helpers are exercised only by
// their own unit tests, so we silence dead-code lints there.
#[cfg_attr(not(windows), allow(dead_code))]
pub mod format;

// On non-Windows targets the autostart / single-instance / tray glue is
// dead code (no tray loop calls into them), but we keep the modules
// compiled so the cross-platform helpers stay honest.
#[cfg_attr(not(windows), allow(dead_code))]
pub mod autostart;
#[cfg_attr(not(windows), allow(dead_code))]
pub mod single_instance;
pub mod status_bridge;

#[cfg(windows)]
pub mod settings_window;
#[cfg(windows)]
pub mod tray;
