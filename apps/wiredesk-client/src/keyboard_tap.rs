//! macOS CGEventTap-based keyboard hijack for capture-mode.
//!
//! On macOS: spawns a thread with a CFRunLoop and a CGEventTap. The tap
//! intercepts all keyboard events at the session level. When the enable flag
//! is true, events are decoded into `Packet`s and forwarded to `outgoing_tx`;
//! when false, the callback returns the event untouched and macOS handles it
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

/// Handle to the tap thread. Owns the enable flag so the UI can switch the
/// tap on/off in O(1). On macOS additionally owns a reference to the
/// CFRunLoop and the thread join handle for graceful shutdown via Drop.
pub struct TapHandle {
    enabled: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    inner: Option<MacOsInner>,
}

#[cfg(target_os = "macos")]
struct MacOsInner {
    // Real fields populated in Task 3 (CFRunLoop ref, thread join handle).
    // For Task 2 this is a placeholder so the type compiles.
    _placeholder: (),
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
        // Real shutdown (CFRunLoopStop + thread join) lands in Task 3.
        // For now there's nothing to tear down.
        let _ = self.inner.take();
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
        // Real CGEventTap creation lands in Task 3.
        TapHandle {
            enabled,
            inner: None,
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

        // Build { kAXTrustedCheckOptionPrompt: kCFBooleanFalse }.
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
        // Either way, the function must not panic and must return a bool.
        let _ = is_permission_granted();
    }
}
