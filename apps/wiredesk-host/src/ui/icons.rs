//! Embedded PNG bytes for the three tray / settings status icons, plus
//! shared builders that map a `SessionStatus` to a freshly-built nwg icon
//! / bitmap. Both the tray (`TrayNotification::set_icon`) and the settings
//! window (`ImageFrame::set_bitmap`) need the same status→bytes mapping;
//! co-locating the build step here keeps the bitmap-builder duplication
//! to one site.
//!
//! The PNGs are tiny (≤ 500 B each); inlining via `include_bytes!` keeps
//! the binary self-contained without a runtime resource path lookup.

#[cfg(windows)]
use native_windows_gui as nwg;

#[cfg(windows)]
use crate::session_thread::SessionStatus;
#[cfg(windows)]
use crate::ui::format::{self, StatusColor};

pub const ICON_GREEN_BYTES: &[u8] = include_bytes!("../../../../assets/tray-green.png");
pub const ICON_YELLOW_BYTES: &[u8] = include_bytes!("../../../../assets/tray-yellow.png");
pub const ICON_GRAY_BYTES: &[u8] = include_bytes!("../../../../assets/tray-gray.png");

/// Pick the embedded PNG bytes that match the given session status.
#[cfg(windows)]
fn status_bytes(status: &SessionStatus) -> &'static [u8] {
    match format::status_color(status) {
        StatusColor::Green => ICON_GREEN_BYTES,
        StatusColor::Yellow => ICON_YELLOW_BYTES,
        StatusColor::Gray => ICON_GRAY_BYTES,
    }
}

/// Build a fresh `nwg::Icon` for the tray notification reflecting `status`.
#[cfg(windows)]
pub fn build_status_icon(status: &SessionStatus) -> Result<nwg::Icon, nwg::NwgError> {
    let mut icon = nwg::Icon::default();
    nwg::Icon::builder()
        .source_bin(Some(status_bytes(status)))
        .strict(true)
        .build(&mut icon)?;
    Ok(icon)
}

/// Rebuild the given bitmap in-place to reflect `status`. Used by the
/// settings window's `ImageFrame`, which keeps a stable handle to the
/// owned bitmap field across status changes (re-pointing the layout would
/// shift the row).
#[cfg(windows)]
pub fn build_status_bitmap(
    status: &SessionStatus,
    dst: &mut nwg::Bitmap,
) -> Result<(), nwg::NwgError> {
    nwg::Bitmap::builder()
        .source_bin(Some(status_bytes(status)))
        .strict(true)
        .build(dst)
}
