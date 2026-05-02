//! NSEvent local monitor — catches keystrokes when WireDesk is the active
//! macOS app, *before* AppKit's normal key-equivalent dispatch swallows
//! them. We need this for one specific job: detecting Cmd+Esc with the
//! window focused but capture-mode OFF.
//!
//! Why not eframe / winit input?
//! eframe sometimes drops the command-modifier bit by the time
//! `key_pressed(Esc)` is sampled (observed live), and Cmd+Esc on macOS is
//! intercepted by the system menu's accelerator dispatch before winit
//! even sees it. NSEvent local monitor sits at exactly the right level —
//! AppKit calls our block first.
//!
//! Why not `CGEventTap` (the existing keyboard_tap.rs)?
//! That tap is gated by `capturing` so it can't see Cmd+Esc when capture
//! is off — and we don't want to make it always-on (would intercept
//! every keystroke even outside capture, breaking Cmd+V to other apps).
//! NSEvent local monitor is the right tool here: app-scoped, lightweight.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use block2::RcBlock;
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags};

const VK_ESCAPE: u16 = 53;
const VK_RETURN: u16 = 36;

/// Install AppKit local-monitor for KeyDown events. When the user presses
/// Cmd+Esc while WireDesk is the active app, `toggle_capture_flag` is
/// flipped to true; when Cmd+Enter, `toggle_fullscreen_flag` is flipped.
/// The flags are read+cleared by the egui update loop in `WireDeskApp`.
///
/// Returns the monitor handle. Keeping it alive keeps the monitor
/// installed; dropping it removes the monitor. We `forget` it to make
/// the monitor permanent for the process lifetime — there's no removal
/// path needed.
pub fn install(
    toggle_capture_flag: Arc<AtomicBool>,
    toggle_fullscreen_flag: Arc<AtomicBool>,
) {
    let block = RcBlock::new(move |event: *mut NSEvent| -> *mut NSEvent {
        if event.is_null() {
            return event;
        }
        // SAFETY: AppKit hands us a live NSEvent for the duration of the
        // block call. We only read modifier bits and the keyCode — both
        // pure-getters, no mutation, no thread hop.
        unsafe {
            let event_ref = &*event;
            let mods = event_ref.modifierFlags();
            // Match Cmd-something keystrokes. Reject Shift+Cmd+Esc etc.
            // — those are different shortcuts and the user might be using
            // them for other apps' purposes.
            let cmd_only = mods.contains(NSEventModifierFlags::NSEventModifierFlagCommand)
                && !mods.contains(NSEventModifierFlags::NSEventModifierFlagShift)
                && !mods.contains(NSEventModifierFlags::NSEventModifierFlagOption)
                && !mods.contains(NSEventModifierFlags::NSEventModifierFlagControl);
            if !cmd_only {
                return event;
            }
            let kc = event_ref.keyCode();
            match kc {
                VK_ESCAPE => {
                    toggle_capture_flag.store(true, Ordering::SeqCst);
                    // Returning null intercepts the event so it doesn't
                    // propagate to AppKit's default Cmd+Esc handling
                    // (which would otherwise try to close any active
                    // sheet/dialog or beep).
                    std::ptr::null_mut()
                }
                VK_RETURN => {
                    toggle_fullscreen_flag.store(true, Ordering::SeqCst);
                    std::ptr::null_mut()
                }
                _ => event,
            }
        }
    });

    unsafe {
        // [NSEvent addLocalMonitorForEventsMatchingMask:NSEventMaskKeyDown
        //                                       handler:block]
        let _: *mut AnyObject = msg_send![
            class!(NSEvent),
            addLocalMonitorForEventsMatchingMask: NSEventMask::KeyDown,
            handler: &*block,
        ];
    }
    // Leak the block — the monitor lives for the lifetime of the app and
    // AppKit holds an internal reference. Dropping the block now would
    // crash the next time AppKit tries to call it.
    std::mem::forget(block);
}
