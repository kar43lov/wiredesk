//! Physical monitor enumeration via `NSScreen` (macOS) — for «Display X»
//! selection in Settings + per-monitor fullscreen orchestration.
//!
//! On non-macOS targets [`list_monitors`] returns an empty `Vec` (we don't
//! ship a Linux/Windows client today, but keeping the module cross-platform
//! avoids `#[cfg]` noise at the call sites).
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

#![allow(dead_code)]

use eframe::egui;

/// Snapshot of one physical display: stable index in `NSScreen::screens()`,
/// human-readable name, and global-coordinate frame **already converted to
/// winit's top-left y-down system** (see module docs). Suitable input for
/// `ViewportCommand::OuterPosition` (use `frame.min`) and for rendering
/// "Display N — Name (W×H)" labels in the Settings combo-box.
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Index in the `NSScreen::screens()` array at enumeration time. Useful
    /// only for "Display N" labels — the index is **not** stable across
    /// reboots, dock events, or hot-plugs, so config persistence keys off
    /// the human-readable `name` instead.
    pub index: usize,
    /// Human-readable name from `NSScreen.localizedName` ("Studio Display",
    /// "Built-in Retina Display", …). This is the persistence key for
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

#[cfg(not(target_os = "macos"))]
pub fn list_monitors() -> Vec<MonitorInfo> {
    Vec::new()
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
        m.name,
        size.x as u32,
        size.y as u32,
        origin.x as i32,
        origin.y as i32,
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

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn list_monitors_non_macos_returns_empty() {
        assert!(list_monitors().is_empty());
    }
}
