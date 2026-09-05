//! Physical monitor enumeration — for «Display X» selection in Settings +
//! per-monitor fullscreen orchestration. `NSScreen` on macOS,
//! `EnumDisplayMonitors` on Windows.
//!
//! On every other target [`list_monitors`] returns an empty `Vec`, which the
//! callers already treat as "fullscreen on whatever display holds the
//! window".
//!
//! ## Coordinate-system note (macOS)
//!
//! `NSScreen.frame()` reports rectangles in AppKit's **bottom-left, y-up**
//! global coordinate space, with the primary screen's bottom-left at
//! `(0, 0)`. egui / winit (and therefore `ViewportCommand::OuterPosition`)
//! expect **top-left, y-down** coordinates with the primary screen's
//! top-left at `(0, 0)`. We convert at enumeration time so every consumer
//! downstream — Settings combo labels, fullscreen orchestration — works in
//! a single coordinate system. The math:
//!
//! ```text
//! winit_y = primary_height - (nsscreen_y + nsscreen_height)
//! ```
//!
//! where `primary_height` is the height of `NSScreen::screens()[0]`.
//! Width and X are unchanged. Without this flip, a monitor stacked above
//! the primary (positive Y in NSScreen) would be rendered with a negative
//! winit Y — wrong direction — and `OuterPosition` would land on the
//! wrong physical display before fullscreen kicks in.
//!
//! ## Coordinate-system note (Windows)
//!
//! Win32 already uses top-left y-down, so there is no flip. What there *is*
//! instead is DPI: `GetMonitorInfoW` reports **physical** pixels, while
//! `ViewportCommand::OuterPosition` multiplies what we give it by the
//! window's `pixels_per_point` before handing it to winit. So we divide each
//! monitor's rectangle by that monitor's own scale factor
//! (`GetDpiForMonitor / 96`), which round-trips exactly as long as the
//! window's scale matches the target monitor's.
//!
//! Mixed-DPI caveat: with two monitors at different scaling (say 100% and
//! 150%), the window's `pixels_per_point` still belongs to the display it is
//! *leaving*, so the computed position is off by the ratio between the two
//! scales and fullscreen can land on the wrong display. Documented in
//! `docs/known-limitations.md`; the fix would be for the fullscreen path to
//! carry physical pixels end-to-end, which is a larger change than this
//! module.

#![allow(dead_code)]

use eframe::egui;

/// Snapshot of one physical display: stable index in `NSScreen::screens()`,
/// human-readable name, and global-coordinate frame **already converted to
/// winit's top-left y-down system** (see module docs). Suitable input for
/// `ViewportCommand::OuterPosition` (use `frame.min`) and for rendering
/// "Display N — Name (W×H)" labels in the Settings combo-box.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Position in the enumeration order at snapshot time (primary display
    /// first, then left-to-right, top-to-bottom). Useful only for
    /// "Display N" labels — the index is **not** stable across reboots,
    /// dock events, or hot-plugs, so config persistence keys off the
    /// human-readable `name` instead.
    pub index: usize,
    /// Human-readable name: `NSScreen.localizedName` on macOS ("Studio
    /// Display", "Built-in Retina Display", …), the monitor's
    /// `EnumDisplayDevicesW` device string on Windows ("Generic PnP
    /// Monitor", "DELL U2720Q", …). This is the persistence key for
    /// `ClientConfig::preferred_monitor` — survives reboots, robust against
    /// re-ordering. Best-effort against renames in System Settings; if the
    /// user renames the display the saved preference falls back to "active
    /// monitor" until they re-pick.
    pub name: String,
    /// Global-coordinate frame in **winit / egui** (top-left, y-down)
    /// coordinates after conversion from NSScreen's bottom-left y-up. Pass
    /// `frame.min` directly to `ViewportCommand::OuterPosition`.
    pub frame: egui::Rect,
}

/// Enumerate physical displays connected to the system.
///
/// macOS implementation walks `NSScreen::screens(MainThreadMarker)`, then
/// converts each frame from NSScreen's bottom-left y-up coordinates to
/// winit's top-left y-down using the primary screen's height as the
/// baseline (see [`flip_nsscreen_y`]). Must be called from the main thread
/// — egui's `update()` callback satisfies that.
#[cfg(target_os = "macos")]
pub fn list_monitors() -> Vec<MonitorInfo> {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::MainThreadMarker;

    // Main-thread check: `NSScreen::screens()` and `localizedName` both
    // require the main thread. egui's `update()` runs on the main thread
    // on macOS, which is the only call site — log + return empty if that
    // ever changes rather than panicking.
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("monitor::list_monitors called off main thread; returning empty list");
        return Vec::new();
    };

    let screens = NSScreen::screens(mtm);
    // Primary screen height anchors the y-flip — `screens()` documents that
    // index 0 is the primary screen (the one with the menu bar). Without a
    // primary screen there's nothing to enumerate against; fall through to
    // an empty Vec rather than guess a height.
    let primary_height = match screens.iter().next() {
        Some(s) => s.frame().size.height as f32,
        None => return Vec::new(),
    };
    screens
        .iter()
        .enumerate()
        .map(|(i, screen)| {
            let frame = screen.frame();
            // localizedName is marked unsafe in objc2-app-kit 0.2.x — calling
            // it requires the main thread (already checked) and a live
            // NSScreen reference (we have one from the array).
            let name = unsafe { screen.localizedName() }.to_string();
            let ns_x = frame.origin.x as f32;
            let ns_y = frame.origin.y as f32;
            let w = frame.size.width as f32;
            let h = frame.size.height as f32;
            let winit_y = flip_nsscreen_y(ns_y, h, primary_height);
            let origin = egui::Pos2::new(ns_x, winit_y);
            let size = egui::Vec2::new(w, h);
            MonitorInfo {
                index: i,
                name,
                frame: egui::Rect::from_min_size(origin, size),
            }
        })
        .collect()
}

/// Windows implementation: `EnumDisplayMonitors` for the rectangles,
/// `GetDpiForMonitor` for the per-display scale factor, and
/// `EnumDisplayDevicesW` for a name a human recognises.
///
/// Unlike `NSScreen::screens()`, `EnumDisplayMonitors` promises no
/// particular order — not even that the primary display comes first — so
/// [`order_monitors`] sorts the snapshot before indices are assigned.
///
/// Safe to call from any thread (unlike the macOS path); it is only ever
/// called from `update()` in practice.
#[cfg(target_os = "windows")]
pub fn list_monitors() -> Vec<MonitorInfo> {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, DISPLAY_DEVICEW, HDC, HMONITOR,
        MONITORINFOEXW,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    // Lives under WindowsAndMessaging in windows-rs 0.58, not Gdi, even
    // though the flag belongs to MONITORINFO.
    use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

    /// Callback accumulator — `EnumDisplayMonitors` hands us one display at
    /// a time through an `LPARAM`, so the Vec lives on the caller's stack
    /// and is reached through a raw pointer.
    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let out = unsafe { &mut *(lparam.0 as *mut Vec<RawMonitor>) };

        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        // GetMonitorInfoW takes a MONITORINFO*; MONITORINFOEXW is the same
        // struct with the device-name array appended, which is exactly how
        // the API is meant to be called (cbSize tells it which one it got).
        let ok = unsafe { GetMonitorInfoW(hmon, std::ptr::addr_of_mut!(info) as *mut _) };
        if !ok.as_bool() {
            // Skip this display rather than abort the enumeration — one
            // unreadable monitor should not cost us the others.
            return TRUE;
        }

        let mut dpi_x = 96u32;
        let mut dpi_y = 96u32;
        // A failure here is not fatal: 96 DPI (scale 1.0) is the right
        // fallback and matches what a DPI-unaware process would see.
        let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };

        let device = wide_to_string(&info.szDevice);
        out.push(RawMonitor {
            rect: info.monitorInfo.rcMonitor,
            dpi: dpi_x.max(1),
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            name: monitor_device_name(&device).unwrap_or(device),
        });
        TRUE
    }

    /// Resolve `\\.\DISPLAY1` into a name a human can pick out of a list.
    ///
    /// `EnumDisplayDevicesW` on an adapter name enumerates the monitors
    /// attached to it; index 0 is the one we want. Its `DeviceString` is
    /// usually the driver's idea of the model — and on most setups that is
    /// the literal string "Generic PnP Monitor" for *every* display, which
    /// would leave the Settings dropdown showing two identical rows for the
    /// one feature (per-monitor fullscreen) that depends on telling them
    /// apart. So the adapter's display number goes in as well: "Generic PnP
    /// Monitor (DISPLAY2)" matches what Windows shows in Display Settings.
    ///
    /// Returns `None` when the call fails or both parts are empty, so the
    /// caller can fall back to the raw adapter path.
    fn monitor_device_name(adapter: &str) -> Option<String> {
        use windows::core::PCWSTR;

        // "\\.\DISPLAY2" → "DISPLAY2"; anything unexpected is left alone.
        let short = adapter.rsplit('\\').next().unwrap_or(adapter);

        let wide: Vec<u16> = adapter.encode_utf16().chain(std::iter::once(0)).collect();
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &mut dd, 0) };
        let model = if ok.as_bool() {
            wide_to_string(&dd.DeviceString)
        } else {
            String::new()
        };
        Some(compose_monitor_name(&model, short))
    }

    let mut raw: Vec<RawMonitor> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(std::ptr::addr_of_mut!(raw) as isize),
        )
    };
    if !ok.as_bool() {
        log::warn!("monitor: EnumDisplayMonitors failed; returning empty list");
        return Vec::new();
    }
    order_monitors(raw)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn list_monitors() -> Vec<MonitorInfo> {
    Vec::new()
}

/// One display as Win32 reports it, before ordering and DPI conversion.
/// Split out from [`list_monitors`] so the ordering and scaling logic is
/// testable on any platform.
#[derive(Debug, Clone)]
pub struct RawMonitor {
    /// Rectangle in **physical** pixels of the virtual desktop.
    pub rect: PhysRect,
    /// Effective DPI of this display; 96 means scale 1.0.
    pub dpi: u32,
    /// Whether Windows marks this as the primary display.
    pub primary: bool,
    pub name: String,
}

/// Platform-independent stand-in for Win32's `RECT`, so [`order_monitors`]
/// can be exercised without the `windows` crate.
#[cfg(not(target_os = "windows"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhysRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[cfg(target_os = "windows")]
pub type PhysRect = windows::Win32::Foundation::RECT;

/// Sort a raw snapshot into a stable order and convert it to egui points.
///
/// Order: primary display first, then by `top`, then by `left`. Win32
/// enumerates in an order that depends on the graphics driver and can
/// change between boots; without this, "Display 1" in Settings would drift.
/// Conversion: physical pixels ÷ (dpi / 96), matching what
/// `ViewportCommand::OuterPosition` expects (see module docs).
pub fn order_monitors(mut raw: Vec<RawMonitor>) -> Vec<MonitorInfo> {
    raw.sort_by(|a, b| {
        b.primary
            .cmp(&a.primary)
            .then(a.rect.top.cmp(&b.rect.top))
            .then(a.rect.left.cmp(&b.rect.left))
    });
    raw.into_iter()
        .enumerate()
        .map(|(index, m)| {
            let scale = m.dpi as f32 / 96.0;
            let origin = egui::Pos2::new(m.rect.left as f32 / scale, m.rect.top as f32 / scale);
            let size = egui::Vec2::new(
                (m.rect.right - m.rect.left) as f32 / scale,
                (m.rect.bottom - m.rect.top) as f32 / scale,
            );
            MonitorInfo {
                index,
                name: m.name,
                frame: egui::Rect::from_min_size(origin, size),
            }
        })
        .collect()
}

/// Join the driver's model string and the adapter's display number into the
/// name shown in Settings.
///
/// Either half may be missing: an empty model leaves just "DISPLAY2", an
/// empty display number leaves just the model, and with neither the caller
/// falls back to the raw adapter path.
///
/// Pure, so the formatting is testable off Windows.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn compose_monitor_name(model: &str, display: &str) -> String {
    match (model.trim(), display.trim()) {
        ("", "") => String::new(),
        ("", d) => d.to_string(),
        (m, "") => m.to_string(),
        (m, d) => format!("{m} ({d})"),
    }
}

/// Decode a fixed-size, NUL-padded UTF-16 buffer as Win32 hands them out.
#[cfg(target_os = "windows")]
fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Convert an NSScreen y-coordinate (bottom-left y-up) to winit's top-left
/// y-down using the primary screen's height as the baseline.
///
/// Pure function — extracted so the math is unit-testable without a live
/// AppKit context. See module docs for the formula's derivation.
pub fn flip_nsscreen_y(ns_y: f32, ns_height: f32, primary_height: f32) -> f32 {
    primary_height - (ns_y + ns_height)
}

/// Build the user-facing label for a monitor: name + size in the form
/// `"Studio Display (5120×2880)"`. Used **only for ComboBox display** —
/// what the user reads in Settings. May collide between two physically
/// identical displays (same `localizedName`, same resolution, different
/// origins); for persistence use [`monitor_identity`] instead.
///
/// The width and height come from the already-converted `frame.size()`
/// (winit coordinates), so the label matches what the user sees on screen.
pub fn monitor_label(m: &MonitorInfo) -> String {
    let size = m.frame.size();
    format!("{} ({}×{})", m.name, size.x as u32, size.y as u32)
}

/// Build the **persistence identity** for a monitor: name + size + origin
/// in the form `"Studio Display (5120×2880 @ 0,0)"`. Used as the saved
/// `preferred_monitor` value and as the lookup key in
/// [`resolve_target_monitor`].
///
/// Including the global-coordinate origin disambiguates two physically
/// identical displays (same name, same resolution) that a name-only or
/// label-only key would collide on. macOS / NSScreen never lets two
/// screens overlap at the same `(x, y)` — every connected display has
/// a unique origin in the global coordinate space — so `(name, size,
/// origin)` is a stable unique key. Origins come from the already-flipped
/// winit coordinates, which is fine: they remain unique per display, and
/// the round-trip Save → load → Settings → resolve uses the same value
/// throughout.
///
/// Real-world case this fixes: dual Studio Display setup at `(0, 0)` and
/// `(5120, 0)`. Pre-fix both displays produced
/// `"Studio Display (5120×2880)"` and the saved preference always
/// resolved to monitor 0. Post-fix the second display saves as
/// `"Studio Display (5120×2880 @ 5120,0)"` and round-trips correctly.
pub fn monitor_identity(m: &MonitorInfo) -> String {
    let size = m.frame.size();
    let origin = m.frame.min;
    format!(
        "{} ({}×{} @ {},{})",
        m.name, size.x as u32, size.y as u32, origin.x as i32, origin.y as i32,
    )
}

/// Resolve a stored `preferred_monitor` identity against the live monitor
/// list.
///
/// * `None` → caller wants "current display" semantics, return `None`.
/// * `Some(id)` with no matching monitor → log a warning and return `None`
///   (caller falls back to fullscreen on the active display). This happens
///   when the saved display has been unplugged, renamed, resolution-changed,
///   moved to a different physical position, or the user moved the config
///   between machines.
/// * `Some(id)` matching `monitor_identity(m)` → that `MonitorInfo`.
///
/// Identity-based (name + size + origin) — name-only or name+size collide
/// on dual identical-display setups (e.g. two Studio Displays). Origin is
/// guaranteed unique because NSScreen disallows overlapping frames.
/// Index-based was tried earlier and rejected: NSScreen ordinals aren't
/// stable across reboot / dock / hot-plug — a saved index stays in-range
/// but silently points at a different physical display.
pub fn resolve_target_monitor<'a>(
    preferred: Option<&str>,
    monitors: &'a [MonitorInfo],
) -> Option<&'a MonitorInfo> {
    let id = preferred?;
    match monitors.iter().find(|m| monitor_identity(m) == id) {
        Some(m) => Some(m),
        None => {
            log::warn!(
                "preferred_monitor {id:?} not found among {} monitor(s)",
                monitors.len()
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_monitor(idx: usize, name: &str, x: f32, y: f32, w: f32, h: f32) -> MonitorInfo {
        MonitorInfo {
            index: idx,
            name: name.to_string(),
            frame: egui::Rect::from_min_size(egui::Pos2::new(x, y), egui::Vec2::new(w, h)),
        }
    }

    #[test]
    fn resolve_target_monitor_none_returns_none() {
        let monitors = vec![make_monitor(0, "Built-in", 0.0, 0.0, 1920.0, 1080.0)];
        assert!(resolve_target_monitor(None, &monitors).is_none());
    }

    #[test]
    fn resolve_target_monitor_unknown_label_returns_none() {
        let monitors = vec![
            make_monitor(0, "Built-in", 0.0, 0.0, 1920.0, 1080.0),
            make_monitor(1, "Studio Display", 1920.0, 0.0, 5120.0, 2880.0),
        ];
        assert!(
            resolve_target_monitor(Some("Unplugged Display (4096×2160 @ 0,0)"), &monitors)
                .is_none()
        );
        // Bare name no longer matches — must include the full identity
        // (name + size + origin) to resolve.
        assert!(resolve_target_monitor(Some("Studio Display"), &monitors).is_none());
        // Old label-only format (without origin) also no longer matches:
        // resolution is identity-based now to disambiguate identical displays.
        assert!(resolve_target_monitor(Some("Studio Display (5120×2880)"), &monitors).is_none());
    }

    #[test]
    fn resolve_target_monitor_known_label_returns_monitor() {
        let monitors = vec![
            make_monitor(0, "Built-in", 0.0, 0.0, 1920.0, 1080.0),
            make_monitor(1, "Studio Display", 1920.0, 0.0, 5120.0, 2880.0),
        ];

        let m0 = resolve_target_monitor(Some("Built-in (1920×1080 @ 0,0)"), &monitors)
            .expect("Built-in present");
        assert_eq!(m0.index, 0);
        assert_eq!(m0.name, "Built-in");

        let m1 = resolve_target_monitor(Some("Studio Display (5120×2880 @ 1920,0)"), &monitors)
            .expect("Studio Display present");
        assert_eq!(m1.index, 1);
        assert_eq!(m1.name, "Studio Display");
    }

    #[test]
    fn monitor_label_format_matches_combo_box() {
        // User-facing label: localized name + size. This is what shows up
        // in the ComboBox; the persisted value uses `monitor_identity` —
        // see `monitor_identity_includes_origin`.
        let m = make_monitor(0, "Studio Display", 0.0, 0.0, 5120.0, 2880.0);
        assert_eq!(monitor_label(&m), "Studio Display (5120×2880)");

        // Built-in retina, sub-1080p height — the cast to u32 truncates
        // fractional points (`frame.size()` returns f32 in egui units), which
        // matches what the user reads in the ComboBox.
        let m = make_monitor(0, "Built-in Retina Display", 0.0, 0.0, 1728.0, 1117.0);
        assert_eq!(monitor_label(&m), "Built-in Retina Display (1728×1117)");
    }

    #[test]
    fn monitor_identity_includes_origin() {
        // Persistence identity: name + size + origin. Origin is the
        // already-flipped winit coordinate (top-left, y-down) — fine for
        // disambiguation as long as both Save and resolve use the same
        // helper, which they do.
        let m = make_monitor(0, "Studio Display", 0.0, 0.0, 5120.0, 2880.0);
        assert_eq!(monitor_identity(&m), "Studio Display (5120×2880 @ 0,0)");

        // Negative-y origin (display stacked above primary in winit space).
        let m = make_monitor(1, "Studio Display", 0.0, -1440.0, 2560.0, 1440.0);
        assert_eq!(monitor_identity(&m), "Studio Display (2560×1440 @ 0,-1440)");
    }

    #[test]
    fn resolve_target_monitor_disambiguates_duplicate_names() {
        // Two physically distinct monitors with the same `localizedName`
        // and **different** resolutions (real-world case: dual external
        // displays of different models that happen to share a localized
        // name) — identity-based resolution must pick the right one.
        let monitors = vec![
            make_monitor(0, "Studio Display", 0.0, 0.0, 5120.0, 2880.0),
            make_monitor(1, "Studio Display", 5120.0, 0.0, 2560.0, 1440.0),
        ];

        let m0 = resolve_target_monitor(Some("Studio Display (5120×2880 @ 0,0)"), &monitors)
            .expect("first Studio Display present");
        assert_eq!(m0.index, 0);

        let m1 = resolve_target_monitor(Some("Studio Display (2560×1440 @ 5120,0)"), &monitors)
            .expect("second Studio Display present");
        assert_eq!(m1.index, 1);
    }

    #[test]
    fn resolve_target_monitor_disambiguates_identical_displays() {
        // The edge case the previous label-only key collided on: two
        // **physically identical** displays (same name, same resolution)
        // at different origins — e.g. dual Studio Display 5K side-by-side.
        // Identity includes origin, so each saves and resolves distinctly.
        let monitors = vec![
            make_monitor(0, "Studio Display", 0.0, 0.0, 5120.0, 2880.0),
            make_monitor(1, "Studio Display", 5120.0, 0.0, 5120.0, 2880.0),
        ];

        // Sanity: the two `monitor_label` strings collide (the old bug).
        assert_eq!(monitor_label(&monitors[0]), monitor_label(&monitors[1]));
        // But the identities don't.
        assert_ne!(
            monitor_identity(&monitors[0]),
            monitor_identity(&monitors[1])
        );

        let m0 = resolve_target_monitor(Some("Studio Display (5120×2880 @ 0,0)"), &monitors)
            .expect("first Studio Display present");
        assert_eq!(m0.index, 0);

        let m1 = resolve_target_monitor(Some("Studio Display (5120×2880 @ 5120,0)"), &monitors)
            .expect("second Studio Display present");
        assert_eq!(m1.index, 1);
    }

    // ---- flip_nsscreen_y ---------------------------------------------------

    #[test]
    fn flip_nsscreen_y_primary_screen_origin_unchanged() {
        // Primary screen sits at (0, 0) in NSScreen and (0, 0) in winit —
        // the flip math should be a no-op for the primary's top edge.
        // ns_y=0, height=1080, primary_height=1080 → 1080 - (0 + 1080) = 0
        assert_eq!(flip_nsscreen_y(0.0, 1080.0, 1080.0), 0.0);
    }

    #[test]
    fn flip_nsscreen_y_secondary_above_primary() {
        // Real-world layout: 2560×1440 external display physically stacked
        // above a 1920×1080 primary. NSScreen reports the external's
        // bottom-left at (0, 1080) — its top edge is at y=1080+1440=2520
        // in NSScreen's y-up world. In winit's y-down world the external's
        // top edge sits at y=−1440 (1440 above the primary's top).
        // primary_height=1080, ns_y=1080, ns_height=1440
        //   → 1080 - (1080 + 1440) = -1440
        assert_eq!(flip_nsscreen_y(1080.0, 1440.0, 1080.0), -1440.0);
    }

    #[test]
    fn flip_nsscreen_y_secondary_below_primary() {
        // Display physically stacked below the primary: NSScreen reports
        // the external's bottom-left at (0, -1080) (negative Y because
        // it's below the primary). In winit's y-down world its top edge
        // sits at y=1080 (1080 below the primary's top).
        // primary_height=1080, ns_y=-1080, ns_height=1080
        //   → 1080 - (-1080 + 1080) = 1080
        assert_eq!(flip_nsscreen_y(-1080.0, 1080.0, 1080.0), 1080.0);
    }

    #[test]
    fn flip_nsscreen_y_secondary_to_the_side() {
        // Display side-by-side at the same height: NSScreen y=0 — same as
        // primary. Flip preserves y=0 because the bottom edges align.
        assert_eq!(flip_nsscreen_y(0.0, 1080.0, 1080.0), 0.0);
    }

    #[test]
    fn flip_nsscreen_y_two_monitor_layout_full_round_trip() {
        // End-to-end shape check: 1920×1080 primary at NSScreen (0, 0)
        // and a 2560×1440 secondary stacked above at NSScreen (0, 1080).
        // After flip, winit positions should be (0, 0) and (0, -1440).
        let primary = flip_nsscreen_y(0.0, 1080.0, 1080.0);
        let secondary = flip_nsscreen_y(1080.0, 1440.0, 1080.0);
        assert_eq!(primary, 0.0);
        assert_eq!(secondary, -1440.0);
        // Sanity: secondary's bottom is exactly at primary's top.
        assert_eq!(secondary + 1440.0, primary);
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn list_monitors_unsupported_platform_returns_empty() {
        assert!(list_monitors().is_empty());
    }

    // --- Windows ordering / DPI conversion (pure, tested everywhere) ----

    fn raw(name: &str, l: i32, t: i32, r: i32, b: i32, dpi: u32, primary: bool) -> RawMonitor {
        RawMonitor {
            rect: PhysRect {
                left: l,
                top: t,
                right: r,
                bottom: b,
            },
            dpi,
            primary,
            name: name.to_string(),
        }
    }

    #[test]
    fn compose_monitor_name_keeps_both_halves() {
        // The common case: identical model strings across displays, told
        // apart by the adapter number Windows itself shows.
        assert_eq!(
            compose_monitor_name("Generic PnP Monitor", "DISPLAY2"),
            "Generic PnP Monitor (DISPLAY2)"
        );
    }

    #[test]
    fn compose_monitor_name_survives_a_missing_half() {
        assert_eq!(compose_monitor_name("", "DISPLAY1"), "DISPLAY1");
        assert_eq!(compose_monitor_name("DELL U2720Q", ""), "DELL U2720Q");
        assert_eq!(compose_monitor_name("  ", " "), "");
    }

    #[test]
    fn identical_models_stay_distinguishable() {
        // Two displays of the same model must not collapse into one label —
        // picking a monitor in Settings would be a coin flip.
        let a = compose_monitor_name("Generic PnP Monitor", "DISPLAY1");
        let b = compose_monitor_name("Generic PnP Monitor", "DISPLAY2");
        assert_ne!(a, b);
    }

    #[test]
    fn order_monitors_puts_primary_first() {
        // Driver order with the primary last — what EnumDisplayMonitors is
        // free to hand us, and what would otherwise make "Display 1" point
        // at a secondary screen.
        let out = order_monitors(vec![
            raw("Left", -1920, 0, 0, 1080, 96, false),
            raw("Main", 0, 0, 2560, 1440, 96, true),
        ]);
        assert_eq!(out[0].name, "Main");
        assert_eq!(out[0].index, 0);
        assert_eq!(out[1].name, "Left");
        assert_eq!(out[1].index, 1);
    }

    #[test]
    fn order_monitors_sorts_secondaries_top_then_left() {
        let out = order_monitors(vec![
            raw("Right", 2560, 0, 4480, 1080, 96, false),
            raw("Above", 0, -1080, 1920, 0, 96, false),
            raw("Main", 0, 0, 2560, 1440, 96, true),
        ]);
        let names: Vec<&str> = out.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Main", "Above", "Right"]);
    }

    #[test]
    fn order_monitors_divides_by_scale_factor() {
        // 150% scaling: 2880×1620 physical is 1920×1080 in egui points, and
        // an origin of 2880 physical is 1920 in points.
        let out = order_monitors(vec![raw("Scaled", 2880, 0, 5760, 1620, 144, true)]);
        let f = out[0].frame;
        assert_eq!(f.min, egui::Pos2::new(1920.0, 0.0));
        assert_eq!(f.size(), egui::Vec2::new(1920.0, 1080.0));
    }

    #[test]
    fn order_monitors_treats_96_dpi_as_unscaled() {
        let out = order_monitors(vec![raw("Plain", 0, 0, 1920, 1080, 96, true)]);
        assert_eq!(
            out[0].frame,
            egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(1920.0, 1080.0))
        );
    }

    #[test]
    fn order_monitors_negative_origin_survives_scaling() {
        // A display to the left of the primary has a negative origin; the
        // division must not flip its sign or clamp it to zero.
        let out = order_monitors(vec![
            raw("Main", 0, 0, 1920, 1080, 96, true),
            raw("Left", -2560, 0, 0, 1440, 192, false),
        ]);
        assert_eq!(out[1].frame.min, egui::Pos2::new(-1280.0, 0.0));
        assert_eq!(out[1].frame.size(), egui::Vec2::new(1280.0, 720.0));
    }

    #[test]
    fn order_monitors_identity_round_trips_through_resolve() {
        // End-to-end: what Windows enumeration produces must be findable by
        // the identity string Settings persists.
        let out = order_monitors(vec![
            raw("Main", 0, 0, 1920, 1080, 96, true),
            raw("Capture", 1920, 0, 3840, 1080, 96, false),
        ]);
        let saved = monitor_identity(&out[1]);
        let found = resolve_target_monitor(Some(&saved), &out).expect("identity must resolve");
        assert_eq!(found.name, "Capture");
    }
}
