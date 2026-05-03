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
    cg_flag_change_to_scancodes, cg_flag_change_to_scancodes_swapped, CG_FLAG_ALT,
    CG_FLAG_COMMAND, CG_FLAG_CONTROL, CG_FLAG_SHIFT,
};

/// Mac VK code constants used for hotkey detection.
const CG_KEY_RETURN: u16 = 0x24;
const CG_KEY_ESCAPE: u16 = 0x35;

/// Mask of all modifier bits we care about — used to reject combos with
/// "extra" modifiers (e.g., Cmd+Shift+Enter shouldn't match Cmd+Enter).
const CG_MODIFIER_MASK: u64 = CG_FLAG_COMMAND | CG_FLAG_CONTROL | CG_FLAG_ALT | CG_FLAG_SHIFT;

/// `true` if the modifier bitmap matches "Cmd, no other modifiers" for the
/// purposes of the local Cmd+Esc / Cmd+Enter hotkeys.
///
/// Without Karabiner-Elements compensation: requires `CG_FLAG_COMMAND`
/// only. With swap on: accepts **either** `CG_FLAG_COMMAND` *or*
/// `CG_FLAG_ALT` (but not both, and no extra modifiers). The user's
/// Karabiner rule typically remaps `left_command ↔ left_option` on a
/// specific external keyboard but leaves the built-in MacBook keyboard
/// alone — so pressing the labeled ⌘ key produces an Option flag on
/// one keyboard and a Cmd flag on the other. Accepting both keeps the
/// "Cmd+Enter" muscle memory working everywhere; the false-positive of
/// triggering on a true Option+Enter is acceptable because Option+Enter
/// has no default macOS binding and isn't a common combo.
fn matches_cmd_only(flags: u64, swap: bool) -> bool {
    let masked = flags & CG_MODIFIER_MASK;
    if swap {
        masked == CG_FLAG_COMMAND || masked == CG_FLAG_ALT
    } else {
        masked == CG_FLAG_COMMAND
    }
}

/// `true` if the event matches Cmd+Enter exactly (no extra modifiers).
fn is_cmd_enter(keycode: u16, flags: u64, swap: bool) -> bool {
    keycode == CG_KEY_RETURN && matches_cmd_only(flags, swap)
}

/// `true` if the event matches Cmd+Esc — the release-capture combo.
/// Picked over Ctrl+Alt+G because that one collides with common
/// window-management apps (Rectangle, Hammerspoon binds, etc.) and
/// because Cmd+Esc is unbound on default macOS.
fn is_release_capture(keycode: u16, flags: u64, swap: bool) -> bool {
    keycode == CG_KEY_ESCAPE && matches_cmd_only(flags, swap)
}

/// Events from the tap thread back to the UI thread.
#[allow(dead_code)] // variants used in later tasks
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapEvent {
    /// User pressed Cmd+Esc inside capture-mode — release capture.
    ReleaseCapture,
    /// User pressed Cmd+Esc with capture OFF — engage capture.
    /// Caught only in passive mode (capture-off but window focused).
    EngageCapture,
    /// User pressed Cmd+Enter — toggle fullscreen.
    ToggleFullscreen,
}

/// A batched synthetic key combo deferred to the dispatcher thread so
/// it can wait for any in-flight clipboard sync to finish before
/// emitting to Host. Used for Whispr Flow's Cmd+V (and any other
/// `CGEventPost`-driven paste tool) — without the deferral the synthesized
/// paste fires before Mac→Host clipboard sync catches up, and Host pastes
/// the *previous* clipboard content. The dispatcher holds the combo for
/// up to 2 s while `outgoing_text_in_flight` is true, plus a short grace
/// for Host to commit, then emits the packets in order.
pub type SyntheticCombo = Vec<Packet>;

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
    /// Passive mode — tap is running but only watches for the toggle
    /// hotkeys (Cmd+Esc → EngageCapture, Cmd+Enter → ToggleFullscreen).
    /// Used when WireDesk is focused but capture is OFF, so the user can
    /// engage capture from the keyboard. Other keystrokes pass through to
    /// macOS (Cmd+V, Cmd+Tab etc. still work normally on the Mac side).
    passive: Arc<AtomicBool>,
    /// Karabiner-Elements `left_command ↔ left_option` compensation.
    /// When true the tap pre-swaps Cmd↔Option bits before mapping to Win
    /// scancodes (so Host receives the user-intended modifier) and uses
    /// the swapped flag for local hotkey detection (so the same physical
    /// key the user pressed before still triggers Cmd+Esc/Cmd+Enter).
    swap_om_cmd: Arc<AtomicBool>,
    prev_flags: Arc<AtomicU64>,
    outgoing_tx: mpsc::Sender<Packet>,
    #[cfg(target_os = "macos")]
    inner: Option<macos::Inner>,
}

impl TapHandle {
    /// Activate the tap — incoming key events are intercepted and forwarded.
    pub fn enable(&self) {
        self.passive.store(false, Ordering::SeqCst);
        self.enabled.store(true, Ordering::SeqCst);
    }

    /// Switch the tap into passive mode: window is focused but capture is
    /// OFF. Tap stays running and watches for Cmd+Esc / Cmd+Enter to
    /// toggle modes — everything else passes through.
    pub fn enable_passive(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        self.passive.store(true, Ordering::SeqCst);
    }

    /// Is the tap in passive mode? (window focused, capture off, only
    /// listening for Cmd+Esc / Cmd+Enter).
    #[allow(dead_code)]
    pub fn is_passive(&self) -> bool {
        self.passive.load(Ordering::SeqCst)
    }

    /// Deactivate the tap. Emits KeyUp events for any modifiers that were
    /// held at the moment of disable so the Host doesn't stay stuck with
    /// Ctrl/Shift/Alt pressed.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        self.passive.store(false, Ordering::SeqCst);
        // Sticky-modifier cleanup. Whatever was in prev_flags is now released.
        let prev = self.prev_flags.swap(0, Ordering::SeqCst);
        let pairs = if self.swap_om_cmd.load(Ordering::SeqCst) {
            cg_flag_change_to_scancodes_swapped(0, prev)
        } else {
            cg_flag_change_to_scancodes(0, prev)
        };
        for (sc, pressed) in pairs {
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
    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Is the tap thread actually running? On macOS this is `true` only when
    /// Accessibility permission was granted at startup; on other platforms
    /// it's always `false` (no tap implementation). UI uses this to decide
    /// whether egui-side key forwarding should be skipped (when the tap is
    /// active, it's the sole source of key events to avoid double KeyDown).
    pub fn is_active(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.inner.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
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
    swap_om_cmd: Arc<AtomicBool>,
    synth_tx: mpsc::Sender<SyntheticCombo>,
) -> TapHandle {
    let enabled = Arc::new(AtomicBool::new(false));
    let passive = Arc::new(AtomicBool::new(false));
    let prev_flags = Arc::new(AtomicU64::new(0));

    #[cfg(target_os = "macos")]
    {
        if !is_permission_granted() {
            log::warn!(
                "keyboard_tap: Accessibility permission not granted — tap will not start"
            );
            return TapHandle {
                enabled,
                passive,
                swap_om_cmd,
                prev_flags,
                outgoing_tx,
                inner: None,
            };
        }
        let inner = macos::Inner::start(
            Arc::clone(&enabled),
            Arc::clone(&passive),
            Arc::clone(&swap_om_cmd),
            Arc::clone(&prev_flags),
            outgoing_tx.clone(),
            _tap_events_tx,
            synth_tx,
        );
        TapHandle {
            enabled,
            passive,
            swap_om_cmd,
            prev_flags,
            outgoing_tx,
            inner: Some(inner),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = _tap_events_tx;
        let _ = synth_tx;
        TapHandle {
            enabled,
            passive,
            swap_om_cmd,
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
    use crate::input::keymap::{
        cg_flag_change_to_scancodes, cg_flag_change_to_scancodes_swapped, cgkeycode_to_scancode,
    };

    // CGEventTapEnable(tap, true) — re-enable a tap that was disabled by
    // the system (timeout or user input). Not exposed by core-graphics
    // directly in a callback-friendly way, so we declare the FFI here.
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn CGEventTapEnable(tap: *mut std::ffi::c_void, enable: bool);
    }

    pub(super) struct Inner {
        runloop: Arc<Mutex<Option<CFRunLoop>>>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl Inner {
        #[allow(clippy::too_many_arguments)]
        pub(super) fn start(
            enabled: Arc<AtomicBool>,
            passive: Arc<AtomicBool>,
            swap_om_cmd: Arc<AtomicBool>,
            prev_flags: Arc<AtomicU64>,
            outgoing_tx: mpsc::Sender<Packet>,
            tap_events_tx: mpsc::Sender<TapEvent>,
            synth_tx: mpsc::Sender<super::SyntheticCombo>,
        ) -> Self {
            let runloop = Arc::new(Mutex::new(None::<CFRunLoop>));
            let runloop_for_thread = Arc::clone(&runloop);

            let tap_port_addr = Arc::new(AtomicUsize::new(0));
            let tap_port_for_cb = Arc::clone(&tap_port_addr);

            let enabled_cb = Arc::clone(&enabled);
            let passive_cb = Arc::clone(&passive);
            let swap_cb = Arc::clone(&swap_om_cmd);
            let prev_flags_cb = Arc::clone(&prev_flags);
            let outgoing_cb = outgoing_tx.clone();
            let tap_events_cb = tap_events_tx.clone();
            let synth_tx_cb = synth_tx.clone();

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

                            let is_active = enabled_cb.load(Ordering::SeqCst);
                            let is_passive = passive_cb.load(Ordering::SeqCst);
                            let swap_setting = swap_cb.load(Ordering::SeqCst);

                            // Karabiner-Elements remaps modifiers at the HID
                            // layer, so events that *originated* from the
                            // physical keyboard carry the post-Karabiner
                            // bitmap. Synthetic events from `CGEventPost`
                            // (Whispr Flow's Cmd+V, TextExpander, AppleScript
                            // keystroke) bypass the HID layer — they always
                            // carry the literal modifier the app intended.
                            // Apply our swap only to physical events; for
                            // synthetic events forward the modifier as-is.
                            // This makes Whispr's Cmd+V land on Host as
                            // Ctrl+V even when swap is on.
                            //
                            // EVENT_SOURCE_STATE_ID:
                            //   1  = HIDSystemState (real keyboard)
                            //   0  = CombinedSessionState (synthetic from app)
                            //  -1  = Private (rare)
                            const HID_SYSTEM_STATE_ID: i64 = 1;
                            let state_id = event
                                .get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
                            let is_physical = state_id == HID_SYSTEM_STATE_ID;
                            let swap = swap_setting && is_physical;

                            // Passive mode: window focused but capture is OFF.
                            // Watch only for the toggle hotkeys (Cmd+Esc /
                            // Cmd+Enter); pass everything else through to
                            // macOS so the user's normal Mac shortcuts (Cmd+V,
                            // Cmd+Tab etc.) still work.
                            if !is_active && is_passive {
                                if matches!(event_type, CGEventType::KeyDown) {
                                    let kc = event.get_integer_value_field(
                                        EventField::KEYBOARD_EVENT_KEYCODE,
                                    ) as u16;
                                    let flags = event.get_flags().bits();
                                    if super::is_cmd_enter(kc, flags, swap) {
                                        let _ =
                                            tap_events_cb.send(TapEvent::ToggleFullscreen);
                                        return CallbackResult::Drop;
                                    }
                                    if super::is_release_capture(kc, flags, swap) {
                                        let _ =
                                            tap_events_cb.send(TapEvent::EngageCapture);
                                        return CallbackResult::Drop;
                                    }
                                }
                                return CallbackResult::Keep;
                            }

                            // If tap is fully off (no focus), let macOS handle
                            // the event normally — we don't intercept outside
                            // capture-mode and outside passive-mode.
                            if !is_active {
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
                                    if super::is_cmd_enter(kc, flags, swap) {
                                        let _ = tap_events_cb.send(TapEvent::ToggleFullscreen);
                                        return CallbackResult::Drop;
                                    }
                                    if super::is_release_capture(kc, flags, swap) {
                                        let _ = tap_events_cb.send(TapEvent::ReleaseCapture);
                                        return CallbackResult::Drop;
                                    }

                                    // Synthetic Cmd+V (Whispr Flow paste,
                                    // TextExpander, AppleScript "keystroke v
                                    // using command down") arrives as a
                                    // single KeyDown carrying the modifier
                                    // in `flags` WITHOUT a preceding
                                    // FlagsChanged. We batch the implied
                                    // modifier press + key press into a
                                    // SyntheticCombo and hand it to the
                                    // dispatcher thread — which holds it
                                    // until any in-flight Mac→Host clipboard
                                    // sync finishes (otherwise the paste
                                    // lands on the *previous* clipboard).
                                    // The global `prev_flags` (physical
                                    // modifier state) is NOT touched —
                                    // synthetic and physical state stay
                                    // independent. Synthetic events use
                                    // the literal modifier bitmap (no
                                    // swap) because Karabiner doesn't
                                    // touch CGEventPost'ed events.
                                    if !is_physical {
                                        let mut combo: super::SyntheticCombo =
                                            Vec::new();
                                        if (flags & super::CG_MODIFIER_MASK) != 0 {
                                            for (sc, _) in
                                                cg_flag_change_to_scancodes(flags, 0)
                                            {
                                                combo.push(Packet::new(
                                                    Message::KeyDown {
                                                        scancode: sc,
                                                        modifiers: 0,
                                                    },
                                                    0,
                                                ));
                                            }
                                        }
                                        if let Some(sc) = cgkeycode_to_scancode(kc) {
                                            combo.push(Packet::new(
                                                Message::KeyDown {
                                                    scancode: sc,
                                                    modifiers: 0,
                                                },
                                                0,
                                            ));
                                        }
                                        if !combo.is_empty() {
                                            let _ = synth_tx_cb.send(combo);
                                        }
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
                                    let flags = event.get_flags().bits();

                                    // Pair with the synthetic KeyDown — we
                                    // batched its modifier press into the
                                    // dispatcher queue, so the matching
                                    // release goes through the same queue
                                    // (in arrival order: V-up then Ctrl-up).
                                    if !is_physical {
                                        let mut combo: super::SyntheticCombo =
                                            Vec::new();
                                        if let Some(sc) = cgkeycode_to_scancode(kc) {
                                            combo.push(Packet::new(
                                                Message::KeyUp {
                                                    scancode: sc,
                                                    modifiers: 0,
                                                },
                                                0,
                                            ));
                                        }
                                        if (flags & super::CG_MODIFIER_MASK) != 0 {
                                            for (sc, _) in
                                                cg_flag_change_to_scancodes(0, flags)
                                            {
                                                combo.push(Packet::new(
                                                    Message::KeyUp {
                                                        scancode: sc,
                                                        modifiers: 0,
                                                    },
                                                    0,
                                                ));
                                            }
                                        }
                                        if !combo.is_empty() {
                                            let _ = synth_tx_cb.send(combo);
                                        }
                                        return CallbackResult::Drop;
                                    }

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
                                    let pairs = if swap {
                                        cg_flag_change_to_scancodes_swapped(cur, prev)
                                    } else {
                                        cg_flag_change_to_scancodes(cur, prev)
                                    };
                                    for (sc, pressed) in pairs {
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
                                    // Pass-through to macOS so modifier-only
                                    // hotkey apps (Whispr Flow's Ctrl+Option,
                                    // push-to-talk dictation tools, etc.)
                                    // still trigger while WireDesk is in
                                    // capture mode. The modifier alone is
                                    // harmless for native shortcuts —
                                    // letter keys are still intercepted on
                                    // KeyDown so combos like Cmd+C don't
                                    // fire on Mac.
                                    CallbackResult::Keep
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
        CG_FLAG_ALT, CG_FLAG_COMMAND, CG_FLAG_CONTROL, CG_FLAG_SHIFT, WIN_SCAN_LALT,
        WIN_SCAN_LCTRL, WIN_SCAN_LSHIFT,
    };
    use std::sync::mpsc;

    fn make_swap_flag() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn make_synth_tx() -> mpsc::Sender<SyntheticCombo> {
        let (tx, _rx) = mpsc::channel();
        tx
    }

    #[test]
    fn handle_starts_disabled() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(out_tx, tap_tx, make_swap_flag(), make_synth_tx());
        assert!(!h.is_enabled());
    }

    #[test]
    fn enable_disable_toggles_flag() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(out_tx, tap_tx, make_swap_flag(), make_synth_tx());
        h.enable();
        assert!(h.is_enabled());
        h.disable();
        assert!(!h.is_enabled());
    }

    #[test]
    fn drop_does_not_panic() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let _h = start(out_tx, tap_tx, make_swap_flag(), make_synth_tx());
    }

    #[test]
    fn permission_query_returns_bool() {
        let _ = is_permission_granted();
    }

    #[test]
    fn disable_emits_keyup_for_held_modifiers() {
        let (out_tx, out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(out_tx, tap_tx, make_swap_flag(), make_synth_tx());

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
        let h = start(out_tx, tap_tx, make_swap_flag(), make_synth_tx());

        h.disable();
        assert!(out_rx.try_recv().is_err(), "no modifiers held → no KeyUp packets");
    }

    #[test]
    fn disable_with_swap_emits_alt_for_held_cmd() {
        // Karabiner-compensation mode: physical Cmd is held → Mac sees
        // Cmd flag (Karabiner remap pre-applied), but with swap on we
        // forward as Alt to Host. Disable must emit Alt-up, not Ctrl-up.
        let (out_tx, out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let swap = Arc::new(AtomicBool::new(true));
        let h = start(out_tx, tap_tx, swap, make_synth_tx());

        h.prev_flags.store(CG_FLAG_COMMAND, Ordering::SeqCst);
        h.disable();

        let mut keyups = Vec::new();
        while let Ok(packet) = out_rx.try_recv() {
            if let wiredesk_protocol::message::Message::KeyUp { scancode, .. } = packet.message {
                keyups.push(scancode);
            }
        }
        assert_eq!(keyups, vec![WIN_SCAN_LALT]);
    }

    // Hotkey detection table tests (swap=false — default behaviour)

    #[test]
    fn cmd_enter_matches() {
        assert!(super::is_cmd_enter(CG_KEY_RETURN, CG_FLAG_COMMAND, false));
    }

    #[test]
    fn cmd_enter_rejects_extra_modifier() {
        // Cmd+Shift+Enter must NOT match (extra modifier).
        assert!(!super::is_cmd_enter(
            CG_KEY_RETURN,
            CG_FLAG_COMMAND | CG_FLAG_SHIFT,
            false
        ));
        // Cmd+Ctrl+Enter must NOT match.
        assert!(!super::is_cmd_enter(
            CG_KEY_RETURN,
            CG_FLAG_COMMAND | CG_FLAG_CONTROL,
            false
        ));
    }

    #[test]
    fn cmd_enter_rejects_no_cmd() {
        assert!(!super::is_cmd_enter(CG_KEY_RETURN, 0, false));
        assert!(!super::is_cmd_enter(CG_KEY_RETURN, CG_FLAG_CONTROL, false));
    }

    #[test]
    fn cmd_enter_rejects_wrong_key() {
        // Some other key with Cmd held — not Cmd+Enter.
        assert!(!super::is_cmd_enter(0x00, CG_FLAG_COMMAND, false)); // Cmd+A
    }

    #[test]
    fn release_capture_matches_cmd_esc() {
        assert!(super::is_release_capture(CG_KEY_ESCAPE, CG_FLAG_COMMAND, false));
    }

    #[test]
    fn release_capture_rejects_extra_modifiers() {
        // Cmd+Shift+Esc must NOT match.
        assert!(!super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_COMMAND | CG_FLAG_SHIFT,
            false
        ));
        // Cmd+Ctrl+Esc must NOT match.
        assert!(!super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_COMMAND | CG_FLAG_CONTROL,
            false
        ));
        // Cmd+Opt+Esc (Force Quit) — must NOT match.
        assert!(!super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_COMMAND | CG_FLAG_ALT,
            false
        ));
    }

    #[test]
    fn release_capture_rejects_no_cmd() {
        // Plain Esc.
        assert!(!super::is_release_capture(CG_KEY_ESCAPE, 0, false));
        // Ctrl+Esc.
        assert!(!super::is_release_capture(CG_KEY_ESCAPE, CG_FLAG_CONTROL, false));
    }

    #[test]
    fn release_capture_rejects_wrong_key() {
        // Cmd+something-else — not Cmd+Esc.
        assert!(!super::is_release_capture(0x00, CG_FLAG_COMMAND, false));
    }

    // Hotkey detection — swap=true (Karabiner-Elements compensation).
    // Physical Cmd-key produces Option flag in the bitmap; the user expects
    // hotkeys triggered by their muscle memory of "Cmd+Esc / Cmd+Enter" to
    // still fire.

    #[test]
    fn swap_mode_cmd_enter_matches_either_flag() {
        // Karabiner remap covers one keyboard (physical ⌘ → Option flag),
        // but the built-in keyboard isn't remapped (physical ⌘ → Cmd flag).
        // Accept either so the user's Cmd+Enter works on both.
        assert!(super::is_cmd_enter(CG_KEY_RETURN, CG_FLAG_ALT, true));
        assert!(super::is_cmd_enter(CG_KEY_RETURN, CG_FLAG_COMMAND, true));
    }

    #[test]
    fn swap_mode_release_capture_matches_either_flag() {
        assert!(super::is_release_capture(CG_KEY_ESCAPE, CG_FLAG_ALT, true));
        assert!(super::is_release_capture(CG_KEY_ESCAPE, CG_FLAG_COMMAND, true));
    }

    #[test]
    fn swap_mode_rejects_no_modifier() {
        assert!(!super::is_cmd_enter(CG_KEY_RETURN, 0, true));
        assert!(!super::is_release_capture(CG_KEY_ESCAPE, 0, true));
    }

    #[test]
    fn swap_mode_rejects_both_cmd_and_option() {
        // If both Cmd AND Option are held simultaneously, that's a real
        // user combo (e.g. Force Quit Cmd+Opt+Esc), not the hotkey.
        assert!(!super::is_cmd_enter(
            CG_KEY_RETURN,
            CG_FLAG_COMMAND | CG_FLAG_ALT,
            true
        ));
        assert!(!super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_COMMAND | CG_FLAG_ALT,
            true
        ));
    }

    #[test]
    fn swap_mode_rejects_extra_modifier() {
        assert!(!super::is_cmd_enter(
            CG_KEY_RETURN,
            CG_FLAG_ALT | CG_FLAG_SHIFT,
            true
        ));
        assert!(!super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_COMMAND | CG_FLAG_SHIFT,
            true
        ));
        assert!(!super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_ALT | CG_FLAG_CONTROL,
            true
        ));
    }
}
