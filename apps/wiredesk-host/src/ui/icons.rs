//! Embedded PNG bytes for the three tray / settings status icons.
//!
//! Shared between `tray.rs` (system tray notification icon) and
//! `settings_window.rs` (`ImageFrame` next to the status label) so a
//! status change is a single source-of-truth swap on both surfaces.
//!
//! The PNGs are tiny (≤ 500 B each); inlining via `include_bytes!` keeps
//! the binary self-contained without a runtime resource path lookup.

pub const ICON_GREEN_BYTES: &[u8] = include_bytes!("../../../../assets/tray-green.png");
pub const ICON_YELLOW_BYTES: &[u8] = include_bytes!("../../../../assets/tray-yellow.png");
pub const ICON_GRAY_BYTES: &[u8] = include_bytes!("../../../../assets/tray-gray.png");
