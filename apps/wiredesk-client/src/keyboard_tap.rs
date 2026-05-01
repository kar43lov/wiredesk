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

use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::Arc;

use wiredesk_protocol::packet::Packet;

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
/// Owns the enable flag so the UI can switch the tap on/off in O(1). On
/// macOS additionally owns a reference to the CFRunLoop and the thread
/// join handle for graceful shutdown via Drop.
pub struct TapHandle {
    enabled: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    inner: Option<macos::Inner>,
}

impl TapHandle {
    /// Activate the tap — incoming key events are intercepted and forwarded.
    pub fn enable(&self) {
        self.enabled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Deactivate the tap — events flow through to macOS as normal.
    pub fn disable(&self) {
        self.enabled
            .store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Is the tap currently intercepting? (Reflects the enable flag, not
    /// macOS-side tap-disabled-by-timeout state.)
    pub fn is_enabled(&self) -> bool {
        self.enabled
            .load(std::sync::atomic::Ordering::SeqCst)
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
    _outgoing_tx: mpsc::Sender<Packet>,
    _tap_events_tx: mpsc::Sender<TapEvent>,
) -> TapHandle {
    let enabled = Arc::new(AtomicBool::new(false));

    #[cfg(target_os = "macos")]
    {
        if !is_permission_granted() {
            log::warn!(
                "keyboard_tap: Accessibility permission not granted — tap will not start"
            );
            return TapHandle {
                enabled,
                inner: None,
            };
        }
        let inner = macos::Inner::start(Arc::clone(&enabled), _outgoing_tx, _tap_events_tx);
        TapHandle {
            enabled,
            inner: Some(inner),
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = (_outgoing_tx, _tap_events_tx);
        TapHandle { enabled }
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use core_foundation::base::TCFType;
    use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
        CallbackResult,
    };
    use wiredesk_protocol::packet::Packet;

    use super::TapEvent;

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
            outgoing_tx: mpsc::Sender<Packet>,
            tap_events_tx: mpsc::Sender<TapEvent>,
        ) -> Self {
            let runloop = Arc::new(Mutex::new(None::<CFRunLoop>));
            let runloop_for_thread = Arc::clone(&runloop);

            // Pointer to the tap's CFMachPort, stored as usize so it crosses
            // the closure boundary as Copy. Written after the tap is created
            // (closure runs on later events, never during construction).
            let tap_port_addr = Arc::new(AtomicUsize::new(0));
            let tap_port_for_cb = Arc::clone(&tap_port_addr);
            let _ = (outgoing_tx, tap_events_tx, &enabled);

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

                            // Real decode in Task 4. For now: log and pass
                            // through so we can verify the tap fires at all.
                            log::trace!("keyboard_tap: event {event_type:?}");
                            let _ = event;
                            CallbackResult::Keep
                        },
                    );

                    let tap = match tap_result {
                        Ok(t) => t,
                        Err(_) => {
                            log::error!("keyboard_tap: CGEventTap::new failed");
                            return;
                        }
                    };

                    // Save port addr for re-enable handler.
                    tap_port_addr
                        .store(tap.mach_port().as_concrete_TypeRef() as usize, Ordering::SeqCst);

                    // Activate the tap (it starts disabled by default).
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

                    // Stash the runloop ref for shutdown.
                    if let Ok(mut g) = runloop_for_thread.lock() {
                        *g = Some(current.clone());
                    }

                    log::debug!("keyboard_tap: runloop started on dedicated thread");
                    CFRunLoop::run_current();
                    log::debug!("keyboard_tap: runloop exited");

                    // Tap dropped at end of scope → CFMachPort released.
                })
                .expect("failed to spawn keyboard tap thread");

            Self {
                runloop,
                join: Some(join),
            }
        }

        pub(super) fn shutdown(self) {
            // Stop the runloop on the tap thread.
            if let Ok(guard) = self.runloop.lock() {
                if let Some(rl) = guard.as_ref() {
                    rl.stop();
                }
            }

            // Best-effort join with timeout. If the thread doesn't exit in
            // 1s we give up and let the OS clean it on process exit.
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
        // Dropped at end of scope. No assertion — just must not panic.
    }

    #[test]
    fn permission_query_returns_bool() {
        // On non-macOS: always true. On macOS: depends on actual TCC state.
        // Either way the function must not panic and must return a bool.
        let _ = is_permission_granted();
    }
}
