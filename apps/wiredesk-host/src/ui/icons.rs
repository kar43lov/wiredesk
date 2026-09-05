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

/// The multi-size application icon, as embedded in the `.exe` by `build.rs`.
pub const APP_ICON_BYTES: &[u8] = include_bytes!("../../../../assets/app-icon.ico");

/// Extract the PNG bytes of the largest image inside an `.ico`.
///
/// nwg builds an icon from a byte blob through WIC's `frame(0)` — the
/// *first* directory entry, which in our file is 16×16. A 16×16 `HICON` set
/// as the window icon is what made the taskbar button noticeably smaller
/// than every neighbour: the taskbar asks for `ICON_BIG` (32×32 at 100 %
/// DPI, more when scaled) and draws whatever it gets at its native size.
/// Feeding WIC a single large PNG instead lets it scale *down* to whatever
/// size the system asks for, which is both sharp and correctly sized.
///
/// Returns `None` for a malformed file or one with no PNG-compressed entry
/// (a pure BMP `.ico`); callers then fall back to the whole blob.
///
/// `.ico` layout: a 6-byte `ICONDIR` (reserved, type=1, count) followed by
/// `count` 16-byte `ICONDIRENTRY` records; each carries width/height (0
/// meaning 256), a byte length and an offset into the file. Entries are PNG
/// when their payload starts with the PNG signature.
pub fn largest_png_frame(ico: &[u8]) -> Option<&[u8]> {
    const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\x00";
    const HEADER: usize = 6;
    const ENTRY: usize = 16;

    if ico.len() < HEADER || ico[0] != 0 || ico[1] != 0 || ico[2] != 1 || ico[3] != 0 {
        return None; // not an ICONDIR of type 1
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;

    let mut best: Option<(u32, &[u8])> = None;
    for i in 0..count {
        let base = HEADER + i * ENTRY;
        let entry = ico.get(base..base + ENTRY)?;
        // 0 encodes 256 — the dimension byte cannot hold it.
        let width = if entry[0] == 0 { 256 } else { entry[0] as u32 };
        let size = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let Some(payload) = ico.get(offset..offset.checked_add(size)?) else {
            continue; // truncated or lying entry — skip it, don't fail the file
        };
        // The PNG signature's last byte is 0x0A; compare the first seven and
        // that byte separately so the constant above stays readable.
        let is_png = payload.len() > 8 && payload[..7] == PNG_MAGIC[..7] && payload[7] == 0x0A;
        if !is_png {
            continue;
        }
        if best.map(|(w, _)| width > w).unwrap_or(true) {
            best = Some((width, payload));
        }
    }
    best.map(|(_, payload)| payload)
}

/// The best source bytes to hand nwg for the application icon: the largest
/// PNG frame if the `.ico` has one, the whole file otherwise.
pub fn app_icon_source() -> &'static [u8] {
    largest_png_frame(APP_ICON_BYTES).unwrap_or(APP_ICON_BYTES)
}

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

/// Build an application icon at an explicit pixel size.
///
/// Both sizes matter: Windows keeps `ICON_SMALL` (title bar, Alt+Tab list)
/// and `ICON_BIG` (taskbar button) per window, and a window that only sets
/// the small one gets a 16×16 image stretched — or, worse, drawn at native
/// size — in the taskbar.
#[cfg(windows)]
pub fn build_app_icon(size: (u32, u32)) -> Result<nwg::Icon, nwg::NwgError> {
    let mut icon = nwg::Icon::default();
    nwg::Icon::builder()
        .source_bin(Some(app_icon_source()))
        .size(Some(size))
        .strict(true)
        .build(&mut icon)?;
    Ok(icon)
}

/// The sizes Windows wants for `ICON_BIG` / `ICON_SMALL` on this machine,
/// as `(big, small)`. DPI-aware: on a 150 % display these come back 48/24
/// rather than 32/16, so the icon is built sharp instead of upscaled.
#[cfg(windows)]
pub fn system_icon_sizes() -> ((u32, u32), (u32, u32)) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON,
    };

    // GetSystemMetrics returns 0 only if the metric is unavailable; fall
    // back to the classic 100 %-DPI values so we never ask for a 0×0 icon.
    let dim = |v: i32, fallback: u32| if v > 0 { v as u32 } else { fallback };
    unsafe {
        (
            (
                dim(GetSystemMetrics(SM_CXICON), 32),
                dim(GetSystemMetrics(SM_CYICON), 32),
            ),
            (
                dim(GetSystemMetrics(SM_CXSMICON), 16),
                dim(GetSystemMetrics(SM_CYSMICON), 16),
            ),
        )
    }
}

/// Attach both icon sizes to a window.
///
/// nwg's `Window::builder().icon()` only ever sends `WM_SETICON` with
/// `ICON_SMALL`, so the taskbar is left to guess. We send both messages
/// ourselves; the caller must keep the two `nwg::Icon`s alive for as long as
/// the window exists, because their `Drop` destroys the `HICON`.
#[cfg(windows)]
pub fn apply_window_icons(hwnd: isize, big: &nwg::Icon, small: &nwg::Icon) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, ICON_BIG, ICON_SMALL, WM_SETICON};

    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETICON,
            WPARAM(ICON_BIG as usize),
            LPARAM(big.handle as isize),
        );
        SendMessageW(
            hwnd,
            WM_SETICON,
            WPARAM(ICON_SMALL as usize),
            LPARAM(small.handle as isize),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn largest_png_frame_picks_the_biggest_entry() {
        let frame = largest_png_frame(APP_ICON_BYTES).expect("app-icon.ico must carry PNG frames");
        // PNG signature, then IHDR with the width in the first 4 bytes of
        // the chunk body (offset 16).
        assert_eq!(&frame[..8], b"\x89PNG\r\n\x1a\n");
        let width = u32::from_be_bytes([frame[16], frame[17], frame[18], frame[19]]);
        assert_eq!(
            width, 256,
            "expected the 256×256 frame, got {width}px — regenerate assets/app-icon.ico"
        );
    }

    #[test]
    fn app_icon_source_is_the_large_frame_not_the_container() {
        let src = app_icon_source();
        assert!(src.len() < APP_ICON_BYTES.len(), "still the whole .ico");
        assert_eq!(&src[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn garbage_and_truncated_input_yield_none() {
        assert!(largest_png_frame(&[]).is_none());
        assert!(largest_png_frame(b"not an icon at all").is_none());
        // Valid header claiming one entry, but the entry itself is missing.
        assert!(largest_png_frame(&[0, 0, 1, 0, 1, 0]).is_none());
    }

    #[test]
    fn entry_pointing_past_the_end_is_skipped_not_fatal() {
        // One entry, 64×64, claiming 9999 bytes at offset 22 (file is 22
        // bytes long). Must not panic and must report "no usable frame".
        let mut ico = vec![0u8, 0, 1, 0, 1, 0];
        ico.extend_from_slice(&[64, 64, 0, 0, 1, 0, 32, 0]);
        ico.extend_from_slice(&9999u32.to_le_bytes());
        ico.extend_from_slice(&22u32.to_le_bytes());
        assert!(largest_png_frame(&ico).is_none());
    }

    #[test]
    fn bmp_only_icon_has_no_png_frame() {
        // Same shape, a real in-bounds payload that simply is not a PNG.
        let mut ico = vec![0u8, 0, 1, 0, 1, 0];
        ico.extend_from_slice(&[16, 16, 0, 0, 1, 0, 32, 0]);
        ico.extend_from_slice(&16u32.to_le_bytes());
        ico.extend_from_slice(&22u32.to_le_bytes());
        ico.extend_from_slice(&[0x28; 16]); // BITMAPINFOHEADER-ish bytes
        assert!(largest_png_frame(&ico).is_none());
        // …and the caller falls back to the container.
    }
}
