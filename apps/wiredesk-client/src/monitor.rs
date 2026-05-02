//! Physical monitor enumeration via `NSScreen` (macOS) — for «Display X»
//! selection in Settings + per-monitor fullscreen orchestration.
//!
//! On non-macOS targets [`list_monitors`] returns an empty `Vec` (we don't
//! ship a Linux/Windows client today, but keeping the module cross-platform
//! avoids `#[cfg]` noise at the call sites).

// `MonitorInfo` / `list_monitors` / `resolve_target_monitor` are wired up by
// follow-up tasks (config field + Settings combo + toggle_fullscreen). Until
// then keep the module dead-code-free under `-D warnings`.
#![allow(dead_code)]

use eframe::egui;

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    /// Index in the `NSScreen::screens()` array (stable while the screen
    /// configuration doesn't change).
    pub index: usize,
    /// Human-readable name from `NSScreen.localizedName` ("Studio Display",
    /// "Built-in Retina Display", …).
    pub name: String,
    /// Global-coordinate frame of the screen. `frame.min` is the top-left
    /// corner suitable for `ViewportCommand::OuterPosition`.
    pub frame: egui::Rect,
    /// Convenience size (same as `frame.size()`).
    pub size: egui::Vec2,
}

/// Enumerate physical displays connected to the system.
///
/// macOS implementation walks `NSScreen::screens(MainThreadMarker)`. Must be
/// called from the main thread — egui's `update()` callback satisfies that.
#[cfg(target_os = "macos")]
pub fn list_monitors() -> Vec<MonitorInfo> {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::MainThreadMarker;

    // SAFETY: list_monitors is documented as main-thread-only. egui's
    // `update()` runs on the main thread on macOS, which is the only call
    // site. If the assertion fails we'd rather log + return empty than panic
    // (no caller is set up to handle a panic from here).
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("monitor::list_monitors called off main thread; returning empty list");
        return Vec::new();
    };

    let screens = NSScreen::screens(mtm);
    screens
        .iter()
        .enumerate()
        .map(|(i, screen)| {
            let frame = screen.frame();
            // localizedName is marked unsafe in objc2-app-kit 0.2.x — calling
            // it requires the main thread (already asserted) and a live
            // NSScreen reference (we have one from the array).
            let name = unsafe { screen.localizedName() }.to_string();
            let origin = egui::Pos2::new(frame.origin.x as f32, frame.origin.y as f32);
            let size = egui::Vec2::new(frame.size.width as f32, frame.size.height as f32);
            MonitorInfo {
                index: i,
                name,
                frame: egui::Rect::from_min_size(origin, size),
                size,
            }
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
pub fn list_monitors() -> Vec<MonitorInfo> {
    Vec::new()
}

/// Resolve a stored `preferred_monitor` index against the live monitor list.
///
/// * `None` → caller wants "current display" semantics, return `None`.
/// * `Some(idx)` out of range → log a warning and return `None` (caller falls
///   back to fullscreen on the active display).
/// * `Some(idx)` valid → `Some(&monitors[idx])`.
pub fn resolve_target_monitor(
    preferred: Option<usize>,
    monitors: &[MonitorInfo],
) -> Option<&MonitorInfo> {
    let idx = preferred?;
    if idx >= monitors.len() {
        log::warn!(
            "preferred_monitor index {idx} out of range (have {} monitor(s))",
            monitors.len()
        );
        return None;
    }
    Some(&monitors[idx])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_monitor(idx: usize, name: &str, x: f32, y: f32, w: f32, h: f32) -> MonitorInfo {
        MonitorInfo {
            index: idx,
            name: name.to_string(),
            frame: egui::Rect::from_min_size(egui::Pos2::new(x, y), egui::Vec2::new(w, h)),
            size: egui::Vec2::new(w, h),
        }
    }

    #[test]
    fn resolve_target_monitor_none_returns_none() {
        let monitors = vec![make_monitor(0, "Built-in", 0.0, 0.0, 1920.0, 1080.0)];
        assert!(resolve_target_monitor(None, &monitors).is_none());
    }

    #[test]
    fn resolve_target_monitor_invalid_index_returns_none() {
        let monitors = vec![
            make_monitor(0, "Built-in", 0.0, 0.0, 1920.0, 1080.0),
            make_monitor(1, "Studio Display", 1920.0, 0.0, 5120.0, 2880.0),
        ];
        assert!(resolve_target_monitor(Some(99), &monitors).is_none());
    }

    #[test]
    fn resolve_target_monitor_valid_index_returns_monitor() {
        let monitors = vec![
            make_monitor(0, "Built-in", 0.0, 0.0, 1920.0, 1080.0),
            make_monitor(1, "Studio Display", 1920.0, 0.0, 5120.0, 2880.0),
        ];

        let m0 = resolve_target_monitor(Some(0), &monitors).expect("index 0 valid");
        assert_eq!(m0.index, 0);
        assert_eq!(m0.name, "Built-in");

        let m1 = resolve_target_monitor(Some(1), &monitors).expect("index 1 valid");
        assert_eq!(m1.index, 1);
        assert_eq!(m1.name, "Studio Display");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn list_monitors_non_macos_returns_empty() {
        assert!(list_monitors().is_empty());
    }
}
