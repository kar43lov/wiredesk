//! OS-level keyboard hijack for capture-mode.
//!
//! On macOS: spawns a dedicated thread with a CFRunLoop. A session-level
//! CGEventTap intercepts all keyboard events. When the enable flag is true,
//! events are decoded into `Packet`s and forwarded to `outgoing_tx`; when
//! false the callback returns the event untouched and macOS handles it
//! normally.
//!
//! On Windows: spawns a dedicated thread with a message loop and installs a
//! `WH_KEYBOARD_LL` hook. Same contract — same `TapHandle`, same
//! `TapEvent`s — but the decoding is much shorter, because the hook already
//! reports the Set-1 scancode the protocol carries, so no keycode table is
//! involved. See [`windows_impl`] for the differences that are visible to a
//! user (hotkeys, and the two key combos the OS refuses to hand over).
//!
//! On every other platform all functions are no-ops, and
//! `is_permission_granted` returns true (no permission system to check).
//!
//! Permission requirement (macOS only): System Settings → Privacy & Security
//! → Accessibility must list this binary. Without it CGEventTap creation
//! succeeds but the tap never fires. We don't auto-prompt — UX guides the
//! user instead. Windows needs no permission for a low-level hook.

// This whole module is macOS CGEventTap machinery with no-op stubs on other
// targets, so its hotkey constants/helpers are dead in a non-macOS build.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use wiredesk_protocol::packet::Packet;

// The modifier-bitmap machinery is CGEventTap's; the Windows hook reports
// plain scancodes and needs none of it.
#[cfg(target_os = "macos")]
use crate::input::keymap::{cg_flag_change_to_scancodes, cg_flag_change_to_scancodes_swapped};
use crate::input::keymap::{CG_FLAG_ALT, CG_FLAG_COMMAND, CG_FLAG_CONTROL, CG_FLAG_SHIFT};
#[cfg(target_os = "macos")]
use wiredesk_protocol::message::Message;

/// Mac VK code constants used for hotkey detection.
const CG_KEY_RETURN: u16 = 0x24;
const CG_KEY_ESCAPE: u16 = 0x35;
const CG_KEY_V: u16 = 0x09;

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

/// `true` if a synthetic KeyDown is a Cmd+V paste (Whispr Flow / TextExpander
/// / AppleScript). Only a real paste should kick the clipboard poll thread out
/// of its outbound-text debounce — kicking on *any* synthetic key would let an
/// unrelated synthetic keystroke ship a half-formed copy-on-select fragment
/// before it settles. Synthetic events carry the literal Cmd bit (Karabiner
/// doesn't touch CGEventPost'ed events), so no swap handling is needed.
fn is_synthetic_paste(keycode: u16, flags: u64) -> bool {
    keycode == CG_KEY_V && (flags & CG_FLAG_COMMAND) != 0
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
    /// macOS-only: nothing remaps modifiers below the OS on Windows, so the
    /// Windows hook ignores this flag entirely.
    /// When true the tap pre-swaps Cmd↔Option bits before mapping to Win
    /// scancodes (so Host receives the user-intended modifier) and uses
    /// the swapped flag for local hotkey detection (so the same physical
    /// key the user pressed before still triggers Cmd+Esc/Cmd+Enter).
    swap_om_cmd: Arc<AtomicBool>,
    prev_flags: Arc<AtomicU64>,
    outgoing_tx: mpsc::Sender<Packet>,
    #[cfg(target_os = "macos")]
    inner: Option<macos::Inner>,
    #[cfg(target_os = "windows")]
    inner: Option<windows_impl::Inner>,
    /// Channels the tap thread needs, parked here when the tap could not
    /// start at launch (no Accessibility yet) so `try_start_late` can spin
    /// it up once the grant lands. `None` once consumed or when the tap
    /// started normally.
    #[cfg(target_os = "macos")]
    late: Option<LateStart>,
}

/// Senders a deferred tap start still needs — see `TapHandle::late`.
#[cfg(target_os = "macos")]
struct LateStart {
    tap_events_tx: mpsc::Sender<TapEvent>,
    synth_tx: mpsc::Sender<SyntheticCombo>,
    poll_kick_tx: mpsc::Sender<()>,
}

impl TapHandle {
    /// macOS: start the tap in place once Accessibility is granted to the
    /// running process. The permission screen used to demand a relaunch —
    /// macOS applies the grant immediately, the tap simply had never been
    /// created. Returns `true` when a tap thread is running (already or
    /// just now).
    #[cfg(target_os = "macos")]
    pub fn try_start_late(&mut self) -> bool {
        if self.inner.is_some() {
            return true;
        }
        if !is_permission_granted() {
            return false;
        }
        let Some(late) = self.late.take() else {
            return false;
        };
        self.inner = Some(macos::Inner::start(
            Arc::clone(&self.enabled),
            Arc::clone(&self.passive),
            Arc::clone(&self.swap_om_cmd),
            Arc::clone(&self.prev_flags),
            self.outgoing_tx.clone(),
            late.tap_events_tx,
            late.synth_tx,
            late.poll_kick_tx,
        ));
        log::info!("keyboard_tap: Accessibility granted at runtime — tap started without relaunch");
        true
    }

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

        // Sticky-key cleanup. The two platforms track "what is currently
        // held" differently — macOS keeps a modifier bitmap fed by
        // FlagsChanged, Windows keeps the set of scancodes the hook saw go
        // down — so the release path differs even though the intent (leave
        // no key stuck on the host) is identical.
        #[cfg(target_os = "macos")]
        {
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
        #[cfg(target_os = "windows")]
        {
            windows_impl::release_held_keys(&self.outgoing_tx);
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
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            self.inner.is_some()
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            false
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
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
    poll_kick_tx: mpsc::Sender<()>,
) -> TapHandle {
    let enabled = Arc::new(AtomicBool::new(false));
    let passive = Arc::new(AtomicBool::new(false));
    let prev_flags = Arc::new(AtomicU64::new(0));

    #[cfg(target_os = "macos")]
    {
        if !is_permission_granted() {
            log::warn!(
                "keyboard_tap: Accessibility permission not granted — tap deferred until it is"
            );
            return TapHandle {
                enabled,
                passive,
                swap_om_cmd,
                prev_flags,
                outgoing_tx,
                inner: None,
                late: Some(LateStart {
                    tap_events_tx: _tap_events_tx,
                    synth_tx,
                    poll_kick_tx,
                }),
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
            poll_kick_tx,
        );
        TapHandle {
            enabled,
            passive,
            swap_om_cmd,
            prev_flags,
            outgoing_tx,
            inner: Some(inner),
            late: None,
        }
    }

    #[cfg(target_os = "windows")]
    {
        let _ = synth_tx;
        let _ = poll_kick_tx;
        let inner = windows_impl::Inner::start(
            Arc::clone(&enabled),
            Arc::clone(&passive),
            outgoing_tx.clone(),
            _tap_events_tx,
        );
        if inner.is_none() {
            log::warn!("keyboard_tap: low-level keyboard hook unavailable — capture disabled");
        }
        TapHandle {
            enabled,
            passive,
            swap_om_cmd,
            prev_flags,
            outgoing_tx,
            inner,
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = _tap_events_tx;
        let _ = synth_tx;
        let _ = poll_kick_tx;
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
        let opts = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);

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
            poll_kick_tx: mpsc::Sender<()>,
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
            let poll_kick_cb = poll_kick_tx.clone();

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
                            let state_id =
                                event.get_integer_value_field(EventField::EVENT_SOURCE_STATE_ID);
                            let is_physical = state_id == HID_SYSTEM_STATE_ID;
                            let swap = swap_setting && is_physical;

                            // Passive mode: window focused but capture is OFF.
                            // Watch only for the toggle hotkeys (Cmd+Esc /
                            // Cmd+Enter); pass everything else through to
                            // macOS so the user's normal Mac shortcuts (Cmd+V,
                            // Cmd+Tab etc.) still work.
                            if !is_active && is_passive {
                                if matches!(event_type, CGEventType::KeyDown) {
                                    let kc = event
                                        .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                        as u16;
                                    let flags = event.get_flags().bits();
                                    if super::is_cmd_enter(kc, flags, swap) {
                                        let _ = tap_events_cb.send(TapEvent::ToggleFullscreen);
                                        return CallbackResult::Drop;
                                    }
                                    if super::is_release_capture(kc, flags, swap) {
                                        let _ = tap_events_cb.send(TapEvent::EngageCapture);
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
                                    let kc = event
                                        .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                        as u16;
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
                                        // Wake the clipboard poll thread now —
                                        // Whispr Flow writes the clipboard
                                        // immediately before its synthetic
                                        // Cmd+V, so kicking on KeyDown lets
                                        // the poll catch the new content
                                        // before the dispatcher's wait gate.
                                        // Only a real Cmd+V paste kicks: the
                                        // kick also bypasses the outbound-text
                                        // debounce, so an unrelated synthetic
                                        // key must NOT trigger it (else a
                                        // copy-on-select fragment ships early).
                                        if super::is_synthetic_paste(kc, flags) {
                                            let _ = poll_kick_cb.send(());
                                        }

                                        let mut combo: super::SyntheticCombo = Vec::new();
                                        if (flags & super::CG_MODIFIER_MASK) != 0 {
                                            for (sc, _) in cg_flag_change_to_scancodes(flags, 0) {
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
                                    let kc = event
                                        .get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE)
                                        as u16;
                                    let flags = event.get_flags().bits();

                                    // Pair with the synthetic KeyDown — we
                                    // batched its modifier press into the
                                    // dispatcher queue, so the matching
                                    // release goes through the same queue
                                    // (in arrival order: V-up then Ctrl-up).
                                    if !is_physical {
                                        let mut combo: super::SyntheticCombo = Vec::new();
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
                                            for (sc, _) in cg_flag_change_to_scancodes(0, flags) {
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
                        CGEventTapEnable(tap.mach_port().as_concrete_TypeRef() as *mut _, true);
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
                    log::warn!("keyboard_tap: thread did not exit within 1s — leaving as daemon");
                }
            }
        }
    }
}

/// Windows hotkey / scancode helpers.
///
/// Compiled on every platform — not behind `cfg(windows)` — so the decoding
/// rules can be unit-tested on the development Mac. They deal only in plain
/// integers (virtual-key codes and Set-1 scancodes), which is all the hook
/// hands us.
pub(crate) mod win_keys {
    // Used by `windows_impl` and by the tests; dead in a non-Windows
    // release build.
    #![cfg_attr(not(target_os = "windows"), allow(dead_code))]

    /// Virtual-key codes we compare against. Values are fixed by Win32
    /// (`winuser.h`) and are the same numbers `windows-rs` wraps in `VIRTUAL_KEY`.
    pub(crate) mod vk {
        pub const RETURN: u32 = 0x0D;
        pub const ESCAPE: u32 = 0x1B;
        pub const SHIFT: u32 = 0x10;
        pub const CONTROL: u32 = 0x11;
        pub const MENU: u32 = 0x12;
        pub const CAPITAL: u32 = 0x14;
        pub const LWIN: u32 = 0x5B;
        pub const RWIN: u32 = 0x5C;
        pub const LSHIFT: u32 = 0xA0;
        pub const RSHIFT: u32 = 0xA1;
        pub const LCONTROL: u32 = 0xA2;
        pub const RCONTROL: u32 = 0xA3;
        pub const LMENU: u32 = 0xA4;
        pub const RMENU: u32 = 0xA5;
    }

    /// Windows counterpart of the Mac's Cmd+Enter: **Ctrl+Enter** toggles
    /// fullscreen. Cmd maps to Ctrl on the Windows side of every other WireDesk
    /// shortcut, so the muscle memory carries over.
    pub(crate) fn is_win_toggle_fullscreen(vk: u32, ctrl: bool) -> bool {
        ctrl && vk == vk::RETURN
    }

    /// Windows counterpart of the Mac's Cmd+Esc: **Ctrl+Esc** releases capture
    /// (or engages it from passive mode).
    ///
    /// Ctrl+Esc is also the Windows shortcut for opening the Start menu. Inside
    /// capture-mode that is exactly what we want to suppress — the keystroke is
    /// meant for the host, not for the local Start menu — and the low-level hook
    /// sees it before the shell does, so returning "handled" swallows it.
    pub(crate) fn is_win_toggle_capture(vk: u32, ctrl: bool) -> bool {
        ctrl && vk == vk::ESCAPE
    }

    /// Is this virtual key a modifier that may also reach the local desktop?
    ///
    /// Such modifiers are forwarded to the host **and** passed on to Windows,
    /// the same compromise the macOS tap makes for `FlagsChanged`: a lone
    /// modifier triggers nothing on its own, and letting it through keeps
    /// modifier-driven tools working on the client machine. Any other key is
    /// swallowed, so combinations like Ctrl+C cannot fire locally while
    /// capture is on.
    ///
    /// **The Windows key is deliberately excluded.** Unlike Shift, Ctrl or
    /// Alt it does do something on its own — releasing it opens the Start
    /// menu, which would pop over WireDesk on every Win keystroke meant for
    /// the host. It is still forwarded, it just does not reach the local
    /// shell as well.
    pub(crate) fn is_modifier_vk(vk: u32) -> bool {
        matches!(
            vk,
            vk::SHIFT
                | vk::CONTROL
                | vk::MENU
                | vk::CAPITAL
                | vk::LSHIFT
                | vk::RSHIFT
                | vk::LCONTROL
                | vk::RCONTROL
                | vk::LMENU
                | vk::RMENU
        )
    }

    /// Fold the hook's `(scanCode, extended)` pair into the single `u16` the
    /// protocol carries: extended keys become `0xE0xx`, which is precisely what
    /// the host's `WindowsInjector` unpacks again before `SendInput`
    /// (`apps/wiredesk-host/src/injector.rs`).
    ///
    /// `scan` is masked to a byte first: the hook occasionally reports the
    /// extended bit inside `scanCode` itself for injected events.
    pub(crate) fn compose_scancode(scan: u32, extended: bool) -> Option<u16> {
        let base = (scan & 0xFF) as u16;
        if base == 0 {
            // No scancode at all — an injected event that only set a virtual
            // key. The caller retries via MapVirtualKeyW; if that fails too the
            // keystroke is dropped rather than sent as scancode 0, which the
            // host would inject as a null key.
            return None;
        }
        Some(if extended { 0xE000 | base } else { base })
    }
}

#[cfg(target_os = "windows")]
mod windows_impl {
    //! `WH_KEYBOARD_LL` hijack — the Windows half of capture-mode.
    //!
    //! ## Why a dedicated thread
    //!
    //! A low-level hook is delivered to the thread that installed it, and
    //! only while that thread pumps messages. eframe owns the main thread's
    //! message loop and we must not add latency to it — Windows silently
    //! removes a hook whose callback exceeds `LowLevelHooksTimeout`
    //! (300 ms by default). So the hook lives on its own thread whose only
    //! job is `GetMessageW`, and the callback does nothing but decode and
    //! `send` on an mpsc channel.
    //!
    //! ## Why global state
    //!
    //! `SetWindowsHookExW` takes a bare `extern "system" fn` — no closure,
    //! no user-data pointer. The state therefore lives in a `OnceLock`
    //! written once at startup. Only one tap is ever created per process
    //! (`main.rs` calls `start` exactly once), so a second `Inner::start`
    //! reuses the existing state rather than racing it.
    //!
    //! ## What the OS keeps for itself
    //!
    //! Ctrl+Alt+Del and Win+L are handled below the hook chain and never
    //! reach us — the same limitation the host already documents for
    //! `SendInput`. Everything else, Ctrl+Esc and Alt+Tab included, is ours
    //! while capture is on.
    //!
    //! ## No Secure Input equivalent
    //!
    //! macOS switches CGEventTap off while a password field has focus, which
    //! is why capture-mode "breaks" over password prompts there — an
    //! annoyance that is also a safety net. Windows has no such thing: a
    //! low-level hook sees password fields like any other input. So while
    //! capture is on, *everything* typed on this machine goes to the host,
    //! including a local password typed by mistake. Capture is explicit and
    //! visibly banner-marked, but the asymmetry is worth knowing (recorded
    //! in `docs/known-limitations.md`).

    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, MapVirtualKeyW, MAPVK_VK_TO_VSC, VK_CONTROL,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
        TranslateMessage, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    use wiredesk_protocol::message::Message;
    use wiredesk_protocol::packet::Packet;

    use super::win_keys::{
        compose_scancode, is_modifier_vk, is_win_toggle_capture, is_win_toggle_fullscreen,
    };
    use super::TapEvent;

    /// Everything the hook callback needs, reachable from a bare `fn`.
    struct HookState {
        enabled: Arc<AtomicBool>,
        passive: Arc<AtomicBool>,
        outgoing_tx: mpsc::Sender<Packet>,
        tap_events_tx: mpsc::Sender<TapEvent>,
        /// Scancodes currently held down as far as the *host* is concerned.
        /// Capture can be released mid-chord (Ctrl+Esc with Shift held), and
        /// without this the host would keep the modifier pressed forever.
        held: Mutex<BTreeSet<u16>>,
        /// Virtual keys whose key-*down* was consumed as a local hotkey, and
        /// whose key-*up* therefore has to be consumed as well.
        ///
        /// Without this the release half leaks: Ctrl+Enter swallows the Enter
        /// press, then forwards the Enter release, and the host injects a
        /// KeyUp for a key it never saw go down.
        hotkey_down: Mutex<BTreeSet<u32>>,
    }

    static HOOK_STATE: OnceLock<HookState> = OnceLock::new();

    /// Release every key the host still believes is down, and forget them.
    ///
    /// Called from `TapHandle::disable`, i.e. on every capture release.
    /// Sends through the caller's channel rather than the stored one so it
    /// works even if the hook never started.
    pub(super) fn release_held_keys(outgoing_tx: &mpsc::Sender<Packet>) {
        let Some(state) = HOOK_STATE.get() else {
            return;
        };
        // Take the set and drop the guard *before* sending: the hook
        // callback locks this same mutex on every keystroke, and Windows
        // uninstalls a low-level hook whose callback overruns
        // `LowLevelHooksTimeout` (300 ms by default). An unbounded send is
        // fast, but "fast" is not a guarantee worth betting the keyboard on.
        let released = {
            let Ok(mut held) = state.held.lock() else {
                log::warn!("keyboard_tap: held-key set poisoned; skipping release");
                return;
            };
            std::mem::take(&mut *held)
        };
        for scancode in released {
            let _ = outgoing_tx.send(Packet::new(
                Message::KeyUp {
                    scancode,
                    modifiers: 0,
                },
                0,
            ));
        }
    }

    /// Is a Ctrl key physically down right now?
    ///
    /// Asked of the OS rather than tracked ourselves: the hook is installed
    /// for the whole process lifetime, but a Ctrl pressed *before* WireDesk
    /// started, or released while another desktop had focus, would leave a
    /// self-maintained flag lying. `GetAsyncKeyState` cannot drift.
    fn ctrl_is_down() -> bool {
        // High bit set = currently down. The low bit ("pressed since last
        // call") is deliberately ignored.
        (unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) } as u16 & 0x8000) != 0
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // Negative codes must be passed straight through, per the hook
        // contract; anything else risks breaking the chain for other apps.
        if code < 0 {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        let Some(state) = HOOK_STATE.get() else {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        };

        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if !is_down && !is_up {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let vk = info.vkCode;
        let active = state.enabled.load(Ordering::SeqCst);
        let passive = state.passive.load(Ordering::SeqCst);

        // The release half of a hotkey, whatever the mode is now: capture
        // may well have been toggled by the press we swallowed a moment ago,
        // so this check comes before any mode test.
        if is_up {
            let was_hotkey = state
                .hotkey_down
                .lock()
                .map(|mut down| down.remove(&vk))
                .unwrap_or(false);
            if was_hotkey {
                return LRESULT(1);
            }
        }

        // Hotkeys fire on key-down only, in both capture and passive mode,
        // and never reach the host or the local desktop.
        if (active || passive) && is_down {
            let ctrl = ctrl_is_down();
            let hotkey = if is_win_toggle_fullscreen(vk, ctrl) {
                Some(TapEvent::ToggleFullscreen)
            } else if is_win_toggle_capture(vk, ctrl) {
                Some(if active {
                    TapEvent::ReleaseCapture
                } else {
                    TapEvent::EngageCapture
                })
            } else {
                None
            };
            if let Some(event) = hotkey {
                if let Ok(mut down) = state.hotkey_down.lock() {
                    down.insert(vk);
                }
                let _ = state.tap_events_tx.send(event);
                return LRESULT(1);
            }
        }

        // Passive mode watches for the hotkeys above and nothing else, so
        // the user's own machine keeps working normally while WireDesk is
        // merely focused. Fully-off behaves the same way.
        if !active {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        let extended = (info.flags.0 & LLKHF_EXTENDED.0) != 0;
        let scancode = compose_scancode(info.scanCode, extended).or_else(|| {
            // Injected events (on-screen keyboard, automation tools) can
            // carry a virtual key with no scancode; derive one.
            let mapped = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) };
            compose_scancode(mapped, extended)
        });
        let Some(scancode) = scancode else {
            // Deliberately without the virtual-key code: this module sees
            // every keystroke on the machine, and client.log.* is a plain
            // file on disk. "A key was dropped" is all a diagnosis needs;
            // *which* key is the beginning of a keylog.
            log::debug!("keyboard_tap: keystroke with no usable scancode; dropping");
            return LRESULT(1);
        };

        if let Ok(mut held) = state.held.lock() {
            if is_down {
                held.insert(scancode);
            } else {
                held.remove(&scancode);
            }
        }

        let message = if is_down {
            Message::KeyDown {
                scancode,
                modifiers: 0,
            }
        } else {
            Message::KeyUp {
                scancode,
                modifiers: 0,
            }
        };
        let _ = state.outgoing_tx.send(Packet::new(message, 0));

        if is_modifier_vk(vk) {
            // Forwarded *and* passed through — see `is_modifier_vk`.
            unsafe { CallNextHookEx(None, code, wparam, lparam) }
        } else {
            // Swallowed: the keystroke belongs to the host.
            LRESULT(1)
        }
    }

    pub(super) struct Inner {
        thread_id: u32,
        join: Option<thread::JoinHandle<()>>,
    }

    impl Inner {
        /// Install the hook on a dedicated thread. `None` means the hook
        /// could not be installed, in which case capture-mode stays off and
        /// the UI falls back to forwarding egui key events.
        pub(super) fn start(
            enabled: Arc<AtomicBool>,
            passive: Arc<AtomicBool>,
            outgoing_tx: mpsc::Sender<Packet>,
            tap_events_tx: mpsc::Sender<TapEvent>,
        ) -> Option<Self> {
            // A second call would silently keep the first call's channels,
            // which is a confusing failure mode; refuse instead. The refusal
            // hangs off `set` rather than a preceding `get`, so two threads
            // racing here cannot both proceed to install a hook — the loser
            // of the `set` gets the `Err` and backs out.
            if HOOK_STATE
                .set(HookState {
                    enabled,
                    passive,
                    outgoing_tx,
                    tap_events_tx,
                    held: Mutex::new(BTreeSet::new()),
                    hotkey_down: Mutex::new(BTreeSet::new()),
                })
                .is_err()
            {
                log::warn!("keyboard_tap: hook state already initialised; refusing second start");
                return None;
            }

            let thread_id = Arc::new(AtomicU32::new(0));
            let installed = Arc::new(AtomicBool::new(false));
            let ready = Arc::new(AtomicBool::new(false));
            let thread_id_thread = Arc::clone(&thread_id);
            let installed_thread = Arc::clone(&installed);
            let ready_thread = Arc::clone(&ready);

            let join = thread::Builder::new()
                .name("wiredesk-keyboard-hook".into())
                .spawn(move || {
                    thread_id_thread.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);

                    // hmod = None is correct for WH_KEYBOARD_LL when the
                    // hook procedure lives in the calling process.
                    let hook =
                        unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) };
                    let hook: HHOOK = match hook {
                        Ok(h) => {
                            installed_thread.store(true, Ordering::SeqCst);
                            h
                        }
                        Err(e) => {
                            log::error!("keyboard_tap: SetWindowsHookExW failed: {e}");
                            ready_thread.store(true, Ordering::SeqCst);
                            return;
                        }
                    };
                    ready_thread.store(true, Ordering::SeqCst);
                    log::debug!("keyboard_tap: low-level hook installed");

                    // Pumping messages is not optional: it is what lets
                    // Windows deliver the hook callbacks at all.
                    let mut msg = MSG::default();
                    loop {
                        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                        // 0 = WM_QUIT, -1 = error; both end the loop.
                        if got.0 <= 0 {
                            break;
                        }
                        unsafe {
                            let _ = TranslateMessage(&msg);
                            DispatchMessageW(&msg);
                        }
                    }

                    if let Err(e) = unsafe { UnhookWindowsHookEx(hook) } {
                        log::warn!("keyboard_tap: UnhookWindowsHookEx failed: {e}");
                    }
                    log::debug!("keyboard_tap: hook thread exited");
                })
                .ok()?;

            // Wait for the install attempt to resolve so `is_active()` is
            // truthful by the time `start` returns — the UI reads it
            // immediately to decide whether to forward egui key events.
            let deadline = Instant::now() + Duration::from_secs(1);
            while !ready.load(Ordering::SeqCst) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            if !installed.load(Ordering::SeqCst) {
                return None;
            }

            Some(Self {
                thread_id: thread_id.load(Ordering::SeqCst),
                join: Some(join),
            })
        }

        pub(super) fn shutdown(self) {
            if self.thread_id != 0 {
                // WM_QUIT drops GetMessageW out of its loop, after which the
                // thread unhooks itself.
                let _ =
                    unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
            }
            if let Some(handle) = self.join {
                let start = Instant::now();
                while !handle.is_finished() && start.elapsed() < Duration::from_secs(1) {
                    thread::sleep(Duration::from_millis(20));
                }
                if handle.is_finished() {
                    let _ = handle.join();
                } else {
                    log::warn!("keyboard_tap: hook thread did not exit within 1s");
                }
            }
        }
    }
}

#[cfg(test)]
mod win_keys_tests {
    use super::win_keys::{compose_scancode, is_modifier_vk, vk};
    use super::win_keys::{is_win_toggle_capture, is_win_toggle_fullscreen};

    #[test]
    fn ctrl_enter_toggles_fullscreen() {
        assert!(is_win_toggle_fullscreen(vk::RETURN, true));
        assert!(!is_win_toggle_fullscreen(vk::RETURN, false));
        assert!(!is_win_toggle_fullscreen(vk::ESCAPE, true));
    }

    #[test]
    fn ctrl_escape_toggles_capture() {
        assert!(is_win_toggle_capture(vk::ESCAPE, true));
        assert!(!is_win_toggle_capture(vk::ESCAPE, false));
        assert!(!is_win_toggle_capture(vk::RETURN, true));
    }

    #[test]
    fn hotkeys_do_not_overlap() {
        // A single key-down must never fire both events.
        for key in [vk::RETURN, vk::ESCAPE] {
            assert!(!(is_win_toggle_fullscreen(key, true) && is_win_toggle_capture(key, true)));
        }
    }

    #[test]
    fn modifiers_are_recognised_both_sided() {
        for key in [
            vk::SHIFT,
            vk::CONTROL,
            vk::MENU,
            vk::CAPITAL,
            vk::LSHIFT,
            vk::RSHIFT,
            vk::LCONTROL,
            vk::RCONTROL,
            vk::LMENU,
            vk::RMENU,
        ] {
            assert!(is_modifier_vk(key), "vk {key:#x} should be a modifier");
        }
    }

    #[test]
    fn windows_key_is_swallowed_not_passed_through() {
        // Passing the Win key on to the local desktop opens the Start menu
        // over WireDesk on every Win keystroke meant for the host.
        assert!(!is_modifier_vk(vk::LWIN));
        assert!(!is_modifier_vk(vk::RWIN));
    }

    #[test]
    fn letters_and_arrows_are_not_modifiers() {
        // 'A' = 0x41, Left arrow = 0x25 — both must be swallowed and sent,
        // not passed through to the local desktop.
        for key in [0x41, 0x25, vk::RETURN, vk::ESCAPE] {
            assert!(!is_modifier_vk(key), "vk {key:#x} must not be a modifier");
        }
    }

    #[test]
    fn plain_scancode_passes_through() {
        // 'A' is 0x1E in Set 1, and the host expects it unprefixed.
        assert_eq!(compose_scancode(0x1E, false), Some(0x1E));
    }

    #[test]
    fn extended_scancode_gets_e0_prefix() {
        // Right Ctrl: scancode 0x1D with the extended flag → 0xE01D, which
        // the host splits back into 0x1D + KEYEVENTF_EXTENDEDKEY.
        assert_eq!(compose_scancode(0x1D, true), Some(0xE01D));
        // Arrow Left: 0x4B extended → 0xE04B.
        assert_eq!(compose_scancode(0x4B, true), Some(0xE04B));
    }

    #[test]
    fn scancode_high_bits_are_masked_off() {
        // Some injected events carry the extended bit inside scanCode
        // itself; only the low byte is a Set-1 scancode.
        assert_eq!(compose_scancode(0xE01D, false), Some(0x1D));
        assert_eq!(compose_scancode(0xE01D, true), Some(0xE01D));
    }

    #[test]
    fn zero_scancode_is_rejected() {
        // Sending scancode 0 would make the host inject a null key.
        assert_eq!(compose_scancode(0, false), None);
        assert_eq!(compose_scancode(0, true), None);
        assert_eq!(compose_scancode(0xE000, false), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::keymap::{CG_FLAG_ALT, CG_FLAG_COMMAND, CG_FLAG_CONTROL, CG_FLAG_SHIFT};
    // Only the macOS-gated `disable_*` tests below compare against Windows
    // scancodes; on the Windows target the import would be unused.
    #[cfg(target_os = "macos")]
    use crate::input::keymap::{WIN_SCAN_LALT, WIN_SCAN_LCTRL, WIN_SCAN_LSHIFT};
    use std::sync::mpsc;

    fn make_swap_flag() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn make_synth_tx() -> mpsc::Sender<SyntheticCombo> {
        let (tx, _rx) = mpsc::channel();
        tx
    }

    fn make_kick_tx() -> mpsc::Sender<()> {
        let (tx, _rx) = mpsc::channel();
        tx
    }

    #[test]
    fn synthetic_paste_detects_cmd_v_only() {
        // Cmd+V → paste.
        assert!(is_synthetic_paste(CG_KEY_V, CG_FLAG_COMMAND));
        // Cmd+V with extra modifiers still counts (paste apps vary).
        assert!(is_synthetic_paste(
            CG_KEY_V,
            CG_FLAG_COMMAND | CG_FLAG_SHIFT
        ));
        // V without Cmd → not a paste.
        assert!(!is_synthetic_paste(CG_KEY_V, 0));
        assert!(!is_synthetic_paste(CG_KEY_V, CG_FLAG_CONTROL));
        // Cmd + other key (e.g. Cmd+C, keycode 0x08) → not a paste.
        assert!(!is_synthetic_paste(0x08, CG_FLAG_COMMAND));
    }

    #[test]
    fn handle_starts_disabled() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(
            out_tx,
            tap_tx,
            make_swap_flag(),
            make_synth_tx(),
            make_kick_tx(),
        );
        assert!(!h.is_enabled());
    }

    #[test]
    fn enable_disable_toggles_flag() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(
            out_tx,
            tap_tx,
            make_swap_flag(),
            make_synth_tx(),
            make_kick_tx(),
        );
        h.enable();
        assert!(h.is_enabled());
        h.disable();
        assert!(!h.is_enabled());
    }

    #[test]
    fn drop_does_not_panic() {
        let (out_tx, _out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let _h = start(
            out_tx,
            tap_tx,
            make_swap_flag(),
            make_synth_tx(),
            make_kick_tx(),
        );
    }

    #[test]
    fn permission_query_returns_bool() {
        let _ = is_permission_granted();
    }

    // `disable()` derives the KeyUps from `prev_flags` (CGEventFlags) only on
    // macOS; the Windows hook keeps its own held-key set, so these two tests
    // are macOS-specific (they failed on the Windows CI runner).
    #[cfg(target_os = "macos")]
    #[test]
    fn disable_emits_keyup_for_held_modifiers() {
        let (out_tx, out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let h = start(
            out_tx,
            tap_tx,
            make_swap_flag(),
            make_synth_tx(),
            make_kick_tx(),
        );

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
        let h = start(
            out_tx,
            tap_tx,
            make_swap_flag(),
            make_synth_tx(),
            make_kick_tx(),
        );

        h.disable();
        assert!(
            out_rx.try_recv().is_err(),
            "no modifiers held → no KeyUp packets"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn disable_with_swap_emits_alt_for_held_cmd() {
        // Karabiner-compensation mode: physical Cmd is held → Mac sees
        // Cmd flag (Karabiner remap pre-applied), but with swap on we
        // forward as Alt to Host. Disable must emit Alt-up, not Ctrl-up.
        let (out_tx, out_rx) = mpsc::channel();
        let (tap_tx, _tap_rx) = mpsc::channel();
        let swap = Arc::new(AtomicBool::new(true));
        let h = start(out_tx, tap_tx, swap, make_synth_tx(), make_kick_tx());

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
        assert!(super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_COMMAND,
            false
        ));
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
        assert!(!super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_CONTROL,
            false
        ));
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
        assert!(super::is_release_capture(
            CG_KEY_ESCAPE,
            CG_FLAG_COMMAND,
            true
        ));
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
