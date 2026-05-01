//! macOS CGEventTap-based keyboard hijack for capture-mode.
//!
//! On macOS: spawns a dedicated thread with a CFRunLoop. A session-level
//! CGEventTap intercepts all keyboard events. When the enable flag is true,
//! events are decoded into `Packet`s and forwarded to `outgoing_tx`; when
//! false the callback returns the event untouched and macOS handles it
//! normally.
//!
//! On non-macOS platforms all functions are no-ops, and
//! `is_permission_granted` returns true (no permission system to check).
//!
//! Permission requirement: System Settings → Privacy & Security → Accessibility
//! must list this binary. Without it CGEventTap creation succeeds but the
//! tap never fires. We don't auto-prompt — UX guides the user instead.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;

use crate::input::keymap::{
    cg_flag_change_to_scancodes, CG_FLAG_ALT, CG_FLAG_COMMAND, CG_FLAG_CONTROL, CG_FLAG_SHIFT,
};

/// Mac VK code constants used for hotkey detection.
const CG_KEY_RETURN: u16 = 0x24;
const CG_KEY_G: u16 = 0x05;

/// Mask of all modifier bits we care about — used to reject combos with
/// "extra" modifiers (e.g., Cmd+Shift+Enter shouldn't match Cmd+Enter).
const CG_MODIFIER_MASK: u64 = CG_FLAG_COMMAND | CG_FLAG_CONTROL | CG_FLAG_ALT | CG_FLAG_SHIFT;

/// `true` if the event matches Cmd+Enter exactly (no extra modifiers).
fn is_cmd_enter(keycode: u16, flags: u64) -> bool {
    keycode == CG_KEY_RETURN && (flags & CG_MODIFIER_MASK) == CG_FLAG_COMMAND
}

/// `true` if the event matches Ctrl+Alt+G exactly (no Cmd, no Shift).
fn is_ctrl_alt_g(keycode: u16, flags: u64) -> bool {
    keycode == CG_KEY_G && (flags & CG_MODIFIER_MASK) == (CG_FLAG_CONTROL | CG_FLAG_ALT)
}

/// Events from the tap thread back to the UI thread.
#[allow(dead_code)] // variants used in later tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapEvent {
    /// User pressed Ctrl+Alt+G inside capture-mode — release capture.
    ReleaseCapture,
    /// User pressed Cmd+Enter — toggle fullscreen.
    ToggleFullscreen,
}

/// Handle to the tap thread.
///
/// Owns the enable flag (so the UI can switch the tap on/off in O(1)) plus
/// the previous-flags state and an outgoing-channel clone so `disable()`
/// can emit KeyUp events for held modifiers (sticky-modifier cleanup —
/// otherwise Host stays with Ctrl/Shift "stuck" until you re-press them).
///
/// On macOS additionally owns a reference to the CFRunLoop and the thread
/// join handle for graceful shutdown via Drop.
pub struct TapHandle {
    enabled: Arc<AtomicBool>,
    prev_flags: Arc<AtomicU64>,
    outgoing_tx: mpsc::Sender<Packet>,
    #[cfg(target_os = "macos")]
    inner: Option<macos::Inner>,
}

impl TapHandle {
    /// Activate the tap — incoming key events are intercepted and forwarded.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    /// Deactivate the tap. Emits KeyUp events for any modifiers that were
    /// held at the moment of disable so the Host doesn't stay stuck with
    /// Ctrl/Shift/Alt pressed.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        // Sticky-modifier cleanup. Whatever was in prev_flags is now released.
        let prev = self.prev_flags.swap(0, Ordering::SeqCst);
        for (sc, pressed) in cg_flag_change_to_scancodes(0, prev) {
            // pressed should always be false here (current = 0).
            debug_assert!(!pressed);
            let _ = self.outgoing_tx.send(Packet::new(
                Message::KeyUp {
                    scancode: sc,
                    modifiers: 0,
                },
                0,
            ));
        }
    }

    /// Is the tap currently intercepting? (Reflects the enable flag, not
    /// macOS-side tap-disabled-by-timeout state.)
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
}

#[cfg(target_os = "macos")]
impl Drop for TapHandle {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.shutdown();
        }
    }
}

/// Start the tap thread. Returns immediately; the tap is initially disabled.
/// On non-macOS this is a no-op — `enable()`/`disable()` work on the flag
/// but nothing is intercepted.
///
/// If macOS Accessibility permission is missing, the function logs a warning
/// and returns a no-op handle. UI is expected to detect this via
/// `is_permission_granted()` and direct the user to System Settings.
pub fn start(
    outgoing_tx: mpsc::Sender<Packet>,
    _tap_events_tx: mpsc::Sender<TapEvent>,
) -> TapHandle {
    let enabled = Arc::new(AtomicBool::new(false));
    let prev_flags = Arc::new(AtomicU64::new(0));

    #[cfg(target_os = "macos")]
    {
        if !is_permission_granted() {
            log::warn!(
                "keyboard_tap: Accessibility permission not granted — tap will not start"
            );
            return TapHandle {
                enabled,
                prev_flags,
                outgoing_tx,
                inner: None,
            };
        }
        let inner = macos::Inner::start(
            Arc::clone(&enabled),
            Arc::clone(&prev_flags),
            outgoing_tx.clone(),
            _tap_events_tx,
        );
        TapHandle {
            enabled,
            prev_flags,
            outgoing_tx,
            inner: Some(inner),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = _tap_events_tx;
        TapHandle {
            enabled,
            prev_flags,
            outgoing_tx,
        }
    }
}

/// Check whether this process has macOS Accessibility permission. Without
/// it, CGEventTap creation succeeds but the tap silently never fires.
///
/// Passes `kAXTrustedCheckOptionPrompt = false` so we *don't* show the
/// system prompt — the UI handles guiding the user to Settings.
///
/// On non-macOS always returns `true` (no permission system).
pub fn is_permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        use accessibility_sys::AXIsProcessTrustedWithOptions;
        use core_foundation::base::TCFType;
        use core_foundation::boolean::CFBoolean;
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::string::CFString;

        let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
        let value = CFBoolean::false_value();
        let opts =
            CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);

        unsafe { AXIsProcessTrustedWithOptions(opts.as_concrete_TypeRef()) }
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use core_foundation::base::TCFType;
    use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult, EventField,
    };
    use wiredesk_protocol::message::Message;
    use wiredesk_protocol::packet::Packet;

    use super::TapEvent;
    use crate::input::keymap::{cg_flag_change_to_scancodes, cgkeycode_to_scancode};

    /// `CGEventTapEnable(tap, true)` — re-enable a tap that was disabled by
    /// the system (timeout or user input). Not exposed by core-graphics
    /// directly in a callback-friendly way, so we declare the FFI here.
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
    }

    pub(super) struct Inner {
        runloop: Arc<Mutex<Option<CFRunLoop>>>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl Inner {
        pub(super) fn start(
            enabled: Arc<AtomicBool>,
            prev_flags: Arc<AtomicU64>,
            outgoing_tx: mpsc::Sender<Packet>,
            tap_events_tx: mpsc::Sender<TapEvent>,
        ) -> Self {
            let runloop = Arc::new(Mutex::new(None::<CFRunLoop>));
            let runloop_for_thread = Arc::clone(&runloop);

            let tap_port_addr = Arc::new(AtomicUsize::new(0));
            let tap_port_for_cb = Arc::clone(&tap_port_addr);

            let enabled_cb = Arc::clone(&enabled);
            let prev_flags_cb = Arc::clone(&prev_flags);
            let outgoing_cb = outgoing_tx.clone();
            let tap_events_cb = tap_events_tx.clone();

            let join = thread::Builder::new()
                .name("wiredesk-keyboard-tap".into())
                .spawn(move || {
                    let mask = vec![
                        CGEventType::KeyDown,
                        CGEventType::KeyUp,
                        CGEventType::FlagsChanged,
                        CGEventType::TapDisabledByTimeout,
                        CGEventType::TapDisabledByUserInput,
                    ];

                    let tap_result = CGEventTap::new(
                        CGEventTapLocation::Session,
                        CGEventTapPlacement::HeadInsertEventTap,
                        CGEventTapOptions::Default,
                        mask,
                        move |_proxy, event_type, event| {
                            // Re-enable handler — fires when macOS auto-disabled
                            // the tap (callback too slow once, user input, etc.).
                            if matches!(
                                event_type,
                                CGEventType::TapDisabledByTimeout
                                    | CGEventType::TapDisabledByUserInput
                            ) {
                                let addr = tap_port_for_cb.load(Ordering::SeqCst);
                                if addr != 0 {
                                    log::warn!(
                                        "keyboard_tap: tap disabled ({event_type:?}), \
                                         re-enabling"
                                    );
                                    unsafe {
                                        CGEventTapEnable(addr as *mut _, true);
                                    }
                                }
                                return CallbackResult::Drop;
                            }

                            // If tap is not enabled, let macOS handle the event
                            // normally — we don't intercept outside capture-mode.
                            if !enabled_cb.load(Ordering::SeqCst) {
                                return CallbackResult::Keep;
                            }

                            match event_type {
                                CGEventType::KeyDown => {
                                    let kc = event.get_integer_value_field(
                                        EventField::KEYBOARD_EVENT_KEYCODE,
                                    ) as u16;
                                    let flags = event.get_flags().bits();

                                    // Local hotkeys — handled in the UI thread,
                                    // never forwarded to Host.
                                    if super::is_cmd_enter(kc, flags) {
                                        let _ = tap_events_cb.send(TapEvent::ToggleFullscreen);
                                        return CallbackResult::Drop;
                                    }
                                    if super::is_ctrl_alt_g(kc, flags) {
                                        let _ = tap_events_cb.send(TapEvent::ReleaseCapture);
                                        return CallbackResult::Drop;
                                    }

                                    if let Some(sc) = cgkeycode_to_scancode(kc) {
                                        let _ = outgoing_cb.send(Packet::new(
                                            Message::KeyDown {
                                                scancode: sc,
                                                modifiers: 0,
                                            },
                                            0,
                                        ));
                                    }
                                    CallbackResult::Drop
                                }
                                CGEventType::KeyUp => {
                                    let kc = event.get_integer_value_field(
                                        EventField::KEYBOARD_EVENT_KEYCODE,
                                    ) as u16;
                                    if let Some(sc) = cgkeycode_to_scancode(kc) {
                                        let _ = outgoing_cb.send(Packet::new(
                                            Message::KeyUp {
                                                scancode: sc,
                                                modifiers: 0,
                                            },
                                            0,
                                        ));
                                    }
                                    CallbackResult::Drop
                                }
                                CGEventType::FlagsChanged => {
                                    let cur = event.get_flags().bits();
                                    let prev = prev_flags_cb.swap(cur, Ordering::SeqCst);
                                    for (sc, pressed) in
                                        cg_flag_change_to_scancodes(cur, prev)
                                    {
                                        let msg = if pressed {
                                            Message::KeyDown {
                                                scancode: sc,
                                                modifiers: 0,
                                            }
                                        } else {
                                            Message::KeyUp {
                                                scancode: sc,
                                                modifiers: 0,
                                            }
                                        };
                                        let _ = outgoing_cb.send(Packet::new(msg, 0));
                                    }
                                    CallbackResult::Drop
                                }
                                _ => CallbackResult::Keep,
                            }
                        },
                    );

                    let tap = match tap_result {
                        Ok(t) => t,
                        Err(_) => {
                            log::error!("keyboard_tap: CGEventTap::new failed");
                            return;
                        }
                    };

                    tap_port_addr.store(
                        tap.mach_port().as_concrete_TypeRef() as usize,
                        Ordering::SeqCst,
                    );

                    unsafe {
                        CGEventTapEnable(
                            tap.mach_port().as_concrete_TypeRef() as *mut _,
                            true,
                        );
                    }

                    let source = tap
                        .mach_port()
                        .create_runloop_source(0)
                        .expect("keyboard_tap: failed to create runloop source");

                    let current = CFRunLoop::get_current();
                    unsafe {
                        current.add_source(&source, kCFRunLoopCommonModes);
                    }

                    if let Ok(mut g) = runloop_for_thread.lock() {
                        *g = Some(current.clone());
                    }

                    log::debug!("keyboard_tap: runloop started on dedicated thread");
                    CFRunLoop::run_current();
                    log::debug!("keyboard_tap: runloop exited");
                })
                .expect("failed to spawn keyboard tap thread");

            Self {
                runloop,
                join: Some(join),
            }
        }

        pub(super) fn shutdown(self) {
            if let Ok(guard) = self.runloop.lock() {
                if let Some(rl) = guard.as_ref() {
                    rl.stop();
                }
            }

            if let Some(handle) = self.join {
                let start = std::time::Instant::now();
                while !handle.is_finished() && start.elapsed() < Duration::from_secs(1) {
                    thread::sleep(Duration::from_millis(20));
                }
                if handle.is_finished() {
                    let _ = handle.join();
                } else {
                    log::warn!(
                        "keyboard_tap: thread did not exit within 1s — leaving as daemon"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::keymap::{
        CG_FLAG_ALT, CG_FLAG_COMMAND, CG_FLAG_CONTROL, CG_FLAG_SHIFT, WIN_SCAN_LCTRL,
        WIN_SCAN_LSHIFT,
    };
    use std::sync::mpsc;

    #[test]
    fn handle_starts_disabled() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(out_tx, tap_tx);
        assert!(!h.is_enabled());
    }

    #[test]
    fn enable_disable_toggles_flag() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(out_tx, tap_tx);
        h.enable();
        assert!(h.is_enabled());
        h.disable();
        assert!(!h.is_enabled());
    }

    #[test]
    fn drop_does_not_panic() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let _h = start(out_tx, tap_tx);
    }

    #[test]
    fn permission_query_returns_bool() {
        let _ = is_permission_granted();
    }

    #[test]
    fn disable_emits_keyup_for_held_modifiers() {
        let (out_tx, out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(out_tx, tap_tx);

        // Pretend Cmd + Shift were held at the moment of disable.
        h.prev_flags
            .store(CG_FLAG_COMMAND | CG_FLAG_SHIFT, Ordering::SeqCst);
        h.disable();

        let mut keyups = Vec::new();
        while let Ok(packet) = out_rx.try_recv() {
            if let wiredesk_protocol::message::Message::KeyUp { scancode, .. } = packet.message {
                keyups.push(scancode);
            }
        }
        keyups.sort();
        let mut expected = vec![WIN_SCAN_LCTRL, WIN_SCAN_LSHIFT];
        expected.sort();
        assert_eq!(keyups, expected, "expected KeyUp for both held modifiers");

        // prev_flags must be cleared.
        assert_eq!(h.prev_flags.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn disable_when_no_modifiers_is_silent() {
        let (out_tx, out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(out_tx, tap_tx);

        h.disable();
        assert!(out_rx.try_recv().is_err(), "no modifiers held → no KeyUp packets");
    }

    // Hotkey detection table tests

    #[test]
    fn cmd_enter_matches() {
        assert!(super::is_cmd_enter(CG_KEY_RETURN, CG_FLAG_COMMAND));
    }

    #[test]
    fn cmd_enter_rejects_extra_modifier() {
        // Cmd+Shift+Enter must NOT match (extra modifier).
        assert!(!super::is_cmd_enter(CG_KEY_RETURN, CG_FLAG_COMMAND | CG_FLAG_SHIFT));
        // Cmd+Ctrl+Enter must NOT match.
        assert!(!super::is_cmd_enter(CG_KEY_RETURN, CG_FLAG_COMMAND | CG_FLAG_CONTROL));
    }

    #[test]
    fn cmd_enter_rejects_no_cmd() {
        assert!(!super::is_cmd_enter(CG_KEY_RETURN, 0));
        assert!(!super::is_cmd_enter(CG_KEY_RETURN, CG_FLAG_CONTROL));
    }

    #[test]
    fn cmd_enter_rejects_wrong_key() {
        // Some other key with Cmd held — not Cmd+Enter.
        assert!(!super::is_cmd_enter(0x00, CG_FLAG_COMMAND)); // Cmd+A
    }

    #[test]
    fn ctrl_alt_g_matches() {
        assert!(super::is_ctrl_alt_g(CG_KEY_G, CG_FLAG_CONTROL | CG_FLAG_ALT));
    }

    #[test]
    fn ctrl_alt_g_rejects_extra_cmd() {
        // Cmd+Ctrl+Alt+G must NOT match — anti-mask.
        assert!(!super::is_ctrl_alt_g(
            CG_KEY_G,
            CG_FLAG_COMMAND | CG_FLAG_CONTROL | CG_FLAG_ALT
        ));
    }

    #[test]
    fn ctrl_alt_g_rejects_partial() {
        // Just Ctrl+G (no Alt) shouldn't match.
        assert!(!super::is_ctrl_alt_g(CG_KEY_G, CG_FLAG_CONTROL));
        // Just Alt+G (no Ctrl) shouldn't match.
        assert!(!super::is_ctrl_alt_g(CG_KEY_G, CG_FLAG_ALT));
    }

    #[test]
    fn ctrl_alt_g_rejects_wrong_key() {
        // Ctrl+Alt+H — wrong letter.
        assert!(!super::is_ctrl_alt_g(0x04, CG_FLAG_CONTROL | CG_FLAG_ALT));
    }
}
