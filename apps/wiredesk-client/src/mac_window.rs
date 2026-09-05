//! Direct AppKit access to the app's own window.
//!
//! egui/winit report window geometry through `ViewportInfo`, and around a
//! native-fullscreen transition that report cannot be trusted: it echoes the
//! position we asked for rather than the one AppKit actually applied. A
//! restore loop that verifies against it therefore congratulates itself
//! while the window is somewhere else entirely — which is exactly the
//! "window is active but nowhere on screen" symptom.
//!
//! These helpers ask AppKit instead. All of them are no-ops off macOS.

#[cfg(target_os = "macos")]
mod imp {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::{CGFloat, CGPoint, CGRect, CGSize};

    /// `NSApp.mainWindow`, falling back to the first entry of `NSApp.windows`.
    ///
    /// The fallback matters right after a fullscreen exit: the window can
    /// briefly stop being "main" while the Space collapses, and that is the
    /// exact moment we need to inspect it.
    unsafe fn app_window() -> *mut AnyObject {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return std::ptr::null_mut();
        }
        let main: *mut AnyObject = msg_send![app, mainWindow];
        if !main.is_null() {
            return main;
        }
        let windows: *mut AnyObject = msg_send![app, windows];
        if windows.is_null() {
            return std::ptr::null_mut();
        }
        let count: usize = msg_send![windows, count];
        if count == 0 {
            return std::ptr::null_mut();
        }
        msg_send![windows, objectAtIndex: 0usize]
    }

    /// Height of the primary screen — the baseline for converting AppKit's
    /// y-up coordinates into winit's y-down ones. `NSScreen.screens[0]` is
    /// the screen carrying the menu bar, which is what winit uses as origin.
    unsafe fn primary_height() -> Option<f32> {
        let screens: *mut AnyObject = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            return None;
        }
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return None;
        }
        let screen: *mut AnyObject = msg_send![screens, objectAtIndex: 0usize];
        if screen.is_null() {
            return None;
        }
        let frame: CGRect = msg_send![screen, frame];
        Some(frame.size.height as f32)
    }

    /// The window's real outer rect, in winit coordinates (y down, origin at
    /// the primary screen's top-left) so it can be compared against what we
    /// asked `ViewportCommand::OuterPosition` for.
    pub fn real_outer_rect() -> Option<(f32, f32, f32, f32)> {
        unsafe {
            let win = app_window();
            if win.is_null() {
                return None;
            }
            let frame: CGRect = msg_send![win, frame];
            let primary_h = primary_height()?;
            let x = frame.origin.x as f32;
            let w = frame.size.width as f32;
            let h = frame.size.height as f32;
            let y = super::appkit_origin_to_winit_y(frame.origin.y as f32, h, primary_h);
            Some((x, y, w, h))
        }
    }

    /// Everything AppKit will tell us about why a window might not be on
    /// screen, as one log line.
    ///
    /// The position turned out to be a red herring — AppKit agrees with the
    /// coordinates we set — so the answer has to be in one of these flags:
    /// the window being miniaturised, not visible, transparent, or living on
    /// a Space other than the one in front of the user.
    pub fn diagnostics() -> Option<String> {
        unsafe {
            let win = app_window();
            if win.is_null() {
                return Some("no NSWindow".to_string());
            }
            let frame: CGRect = msg_send![win, frame];
            let visible: bool = msg_send![win, isVisible];
            let miniaturized: bool = msg_send![win, isMiniaturized];
            let on_active_space: bool = msg_send![win, isOnActiveSpace];
            let key: bool = msg_send![win, isKeyWindow];
            let main: bool = msg_send![win, isMainWindow];
            let alpha: CGFloat = msg_send![win, alphaValue];
            let level: isize = msg_send![win, level];
            let behavior: usize = msg_send![win, collectionBehavior];
            let occlusion: usize = msg_send![win, occlusionState];
            let screen: *mut AnyObject = msg_send![win, screen];
            let screen_desc = if screen.is_null() {
                // A window whose frame lies on no screen returns nil here —
                // the single most telling flag if the frame looks sane.
                "nil".to_string()
            } else {
                let sf: CGRect = msg_send![screen, frame];
                format!(
                    "({},{}) {}x{}",
                    sf.origin.x, sf.origin.y, sf.size.width, sf.size.height
                )
            };
            Some(format!(
                "frame=({},{}) {}x{} visible={visible} miniaturized={miniaturized} \
                 onActiveSpace={on_active_space} key={key} main={main} alpha={alpha} \
                 level={level} collectionBehavior={behavior:#x} occlusion={occlusion:#x} \
                 screen={screen_desc}",
                frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
            ))
        }
    }

    /// Pull the window into the active Space and focus it.
    ///
    /// After a native-fullscreen exit the window can stay associated with the
    /// Space that just collapsed: Mission Control still lists it, it still
    /// answers as the active window, but nothing paints on the current
    /// desktop.
    ///
    /// Order matters. A miniaturised window ignores ordering messages, so it
    /// has to be deminiaturised first. `NSWindowCollectionBehaviorMoveToActiveSpace`
    /// (0x02) is what actually re-homes a window stranded on a dead Space —
    /// `makeKeyAndOrderFront:` alone will happily "raise" it on the Space it
    /// is already stuck to. `orderFrontRegardless` skips the check that
    /// suppresses ordering for a not-quite-active app.
    pub fn bring_to_current_space() {
        const MOVE_TO_ACTIVE_SPACE: usize = 1 << 1;

        unsafe {
            let win = app_window();
            if win.is_null() {
                return;
            }
            let miniaturized: bool = msg_send![win, isMiniaturized];
            if miniaturized {
                let _: () = msg_send![win, deminiaturize: std::ptr::null::<AnyObject>()];
            }

            let behavior: usize = msg_send![win, collectionBehavior];
            let _: () = msg_send![win, setCollectionBehavior: behavior | MOVE_TO_ACTIVE_SPACE];

            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if !app.is_null() {
                let _: () = msg_send![app, activateIgnoringOtherApps: true];
            }
            let _: () = msg_send![win, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
            let _: () = msg_send![win, orderFrontRegardless];
        }
    }

    /// Set the window's frame through AppKit, taking winit coordinates.
    ///
    /// Used when `ViewportCommand::OuterPosition` has demonstrably not
    /// landed. Going straight to `setFrame:display:` skips winit's bookkeeping
    /// and is the same call a window manager makes.
    pub fn set_outer_rect(x: f32, y: f32, w: f32, h: f32) -> bool {
        unsafe {
            let win = app_window();
            if win.is_null() {
                return false;
            }
            let Some(primary_h) = primary_height() else {
                return false;
            };
            let ns_y = super::winit_y_to_appkit_origin(y, h, primary_h);
            let rect = CGRect {
                origin: CGPoint {
                    x: x as CGFloat,
                    y: ns_y as CGFloat,
                },
                size: CGSize {
                    width: w as CGFloat,
                    height: h as CGFloat,
                },
            };
            let _: () = msg_send![win, setFrame: rect, display: true];
            true
        }
    }

    /// Lift the window above the menu bar, or put it back at the normal
    /// level.
    ///
    /// This is how borderless fullscreen keeps the menu bar off its display.
    /// The obvious alternative — `NSApplicationPresentationHideMenuBar |
    /// HideDock` — is **application-wide, not per-display**: it hides the
    /// Dock on every screen, so covering a secondary display with WireDesk
    /// took the Dock away from the user's main one for as long as our window
    /// stayed key. Window level is a property of this one window, so only
    /// the display it covers is affected.
    ///
    /// `NSMainMenuWindowLevel` is 24; 25 sits just above it and below
    /// `NSScreenSaverWindowLevel`. Level 0 (`NSNormalWindowLevel`) is what
    /// every ordinary window uses — a fullscreen window drops back to it
    /// when it loses focus, so Cmd+Tab still reveals whatever sits under it.
    pub fn set_above_menu_bar(above: bool) {
        const NORMAL_LEVEL: isize = 0;
        const ABOVE_MAIN_MENU_LEVEL: isize = 25;

        unsafe {
            let win = app_window();
            if win.is_null() {
                return;
            }
            let level: isize = if above {
                ABOVE_MAIN_MENU_LEVEL
            } else {
                NORMAL_LEVEL
            };
            let _: () = msg_send![win, setLevel: level];
        }
    }

    /// Restore the default presentation options.
    ///
    /// Only needed to undo a state an older build could have left behind:
    /// until 2026-09-05 fullscreen hid the Dock through
    /// `setPresentationOptions`, and a crash mid-fullscreen left the user's
    /// Dock hidden. Called once at startup; harmless when nothing was set.
    pub fn clear_presentation_options() {
        unsafe {
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            if app.is_null() {
                return;
            }
            let _: () = msg_send![app, setPresentationOptions: 0usize];
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn set_above_menu_bar(_above: bool) {}
    pub fn clear_presentation_options() {}
    pub fn real_outer_rect() -> Option<(f32, f32, f32, f32)> {
        None
    }
    pub fn diagnostics() -> Option<String> {
        None
    }
    pub fn bring_to_current_space() {}
    pub fn set_outer_rect(_x: f32, _y: f32, _w: f32, _h: f32) -> bool {
        false
    }
}

pub use imp::{
    bring_to_current_space, clear_presentation_options, diagnostics, real_outer_rect,
    set_above_menu_bar, set_outer_rect,
};

/// Convert an AppKit window origin (y-up, from the bottom of the primary
/// screen) into winit's y-down top-left origin.
///
/// Pure counterpart of the math inside [`real_outer_rect`], split out so the
/// conversion is testable without a live AppKit context.
// Only the macOS window path calls these; kept unconditional so the
// round-trip test runs on any host.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn appkit_origin_to_winit_y(ns_y: f32, window_height: f32, primary_height: f32) -> f32 {
    primary_height - (ns_y + window_height)
}

/// Inverse of [`appkit_origin_to_winit_y`]: winit top edge → AppKit bottom edge.
// Only the macOS window path calls these; kept unconditional so the
// round-trip test runs on any host.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn winit_y_to_appkit_origin(winit_y: f32, window_height: f32, primary_height: f32) -> f32 {
    primary_height - winit_y - window_height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winit_and_appkit_y_round_trip() {
        // 1707-tall primary, a 760-tall window whose top edge sits 823 points
        // below the top of the screen — the geometry from the live bug report.
        let ns = winit_y_to_appkit_origin(823.0, 760.0, 1707.0);
        assert_eq!(ns, 124.0);
        assert_eq!(appkit_origin_to_winit_y(ns, 760.0, 1707.0), 823.0);
    }

    #[test]
    fn window_flush_with_primary_top_is_zero_in_winit() {
        // A window occupying the full height of the primary screen starts at
        // AppKit y=0 and winit y=0.
        assert_eq!(appkit_origin_to_winit_y(0.0, 1707.0, 1707.0), 0.0);
        assert_eq!(winit_y_to_appkit_origin(0.0, 1707.0, 1707.0), 0.0);
    }

    #[test]
    fn window_below_primary_bottom_gives_negative_appkit_origin() {
        // Dragged off the bottom edge: winit y greater than the screen height
        // means a negative AppKit origin, and the round trip still holds.
        let ns = winit_y_to_appkit_origin(1700.0, 760.0, 1707.0);
        assert!(ns < 0.0, "expected negative AppKit origin, got {ns}");
        assert_eq!(appkit_origin_to_winit_y(ns, 760.0, 1707.0), 1700.0);
    }
}
