//! Named-mutex based single-instance lock for the Windows host.
//!
//! Pattern: `CreateMutexW("Global\\WireDeskHostSingleton")`. If
//! `GetLastError() == ERROR_ALREADY_EXISTS`, another instance owns the
//! lock; the new process should show a "already running" message and
//! exit cleanly. The first process keeps the `SingleInstanceGuard`
//! alive for its lifetime — drop closes the handle, releasing the
//! mutex.
//!
//! Non-Windows targets get a no-op stub.

#[cfg(windows)]
pub struct SingleInstanceGuard {
    handle: windows::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl SingleInstanceGuard {
    pub fn acquire(name: &str) -> SingleInstanceResult {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows::Win32::System::Threading::CreateMutexW;

        let wide: Vec<u16> = OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        unsafe {
            match CreateMutexW(None, false, PCWSTR(wide.as_ptr())) {
                Ok(handle) => {
                    let last = GetLastError();
                    if last == ERROR_ALREADY_EXISTS {
                        // Close our handle to the existing mutex; the
                        // original owner's handle stays open.
                        let _ = windows::Win32::Foundation::CloseHandle(handle);
                        SingleInstanceResult::AlreadyRunning
                    } else {
                        SingleInstanceResult::Acquired(Self { handle })
                    }
                }
                Err(e) => SingleInstanceResult::Error(e.to_string()),
            }
        }
    }
}

#[cfg(windows)]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(not(windows))]
pub struct SingleInstanceGuard;

#[cfg(not(windows))]
impl SingleInstanceGuard {
    pub fn acquire(_name: &str) -> SingleInstanceResult {
        // On non-Windows targets the host is a dev-only foreground process —
        // no need to enforce a single instance.
        SingleInstanceResult::Acquired(Self)
    }
}

pub enum SingleInstanceResult {
    Acquired(SingleInstanceGuard),
    AlreadyRunning,
    Error(String),
}

/// Try to acquire the named-mutex single-instance lock with bounded retries.
///
/// Used by the Save & Restart flow: the freshly spawned host process may
/// race the previous one's shutdown. `acquire` against a still-held mutex
/// returns `AlreadyRunning` immediately, so we sleep `delay_ms` between
/// attempts to give the old process time to drop its handle. With the
/// defaults wired in `main.rs` (5 attempts × 100ms) the new process has a
/// 500ms budget — comfortably more than a graceful shutdown of the
/// previous instance, but short enough that a genuinely already-running
/// session still surfaces as a duplicate-launch quickly.
///
/// `Error` and `Acquired` short-circuit out — only `AlreadyRunning` is
/// retried.
pub fn try_acquire_with_retry(
    name: &str,
    attempts: u8,
    delay_ms: u64,
) -> SingleInstanceResult {
    if attempts == 0 {
        return SingleInstanceResult::AlreadyRunning;
    }
    for i in 0..attempts {
        match SingleInstanceGuard::acquire(name) {
            SingleInstanceResult::AlreadyRunning => {
                if i + 1 < attempts {
                    std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                }
                continue;
            }
            other => return other,
        }
    }
    SingleInstanceResult::AlreadyRunning
}

/// Cross-process "show settings" signal between the running host and a
/// freshly-launched second instance. Implementation: a Win32 named
/// auto-reset event. The owning process keeps a handle and a wait
/// thread; a second instance opens the same event by name, calls
/// SetEvent, and exits.
///
/// Auto-reset (manual_reset = FALSE) so a single SetEvent wakes one
/// waiter and resets — perfect for "click the icon again to bring
/// up Settings" semantics.
#[cfg(windows)]
pub const SHOW_SETTINGS_EVENT_NAME: &str = "WireDeskHostShowSettings";

/// Open and signal the show-settings event. Used by a second-instance
/// launch when the named-mutex check returns AlreadyRunning. Returns
/// true if the existing process was successfully signalled.
#[cfg(windows)]
pub fn signal_show_settings(name: &str) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};

    let wide: Vec<u16> = OsStr::new(name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(wide.as_ptr())) {
            Ok(h) => {
                let ok = SetEvent(h).is_ok();
                let _ = CloseHandle(h);
                ok
            }
            Err(_) => false,
        }
    }
}

/// Owning-side: create the named auto-reset event. Returns the raw
/// handle as a `usize` so it can cross thread boundaries (the
/// `windows::HANDLE` is `!Send`/`!Sync`). Caller is responsible for
/// closing it on shutdown — typically by holding the returned guard.
#[cfg(windows)]
pub fn create_show_settings_event(name: &str) -> Option<ShowSettingsEventHandle> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::System::Threading::CreateEventW;

    let wide: Vec<u16> = OsStr::new(name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    unsafe {
        match CreateEventW(None, false, false, PCWSTR(wide.as_ptr())) {
            Ok(h) => Some(ShowSettingsEventHandle { raw: h.0 as usize }),
            Err(_) => None,
        }
    }
}

/// Wait on the event handle until SetEvent fires, or `stop()` is invoked
/// from another thread. The returned closure stops the wait by
/// signalling the event itself — auto-reset means the one waiter wakes
/// and the loop checks `should_stop` before continuing.
#[cfg(windows)]
pub struct ShowSettingsEventHandle {
    raw: usize,
}

#[cfg(windows)]
impl ShowSettingsEventHandle {
    /// Block until SetEvent fires. Returns true on a real signal,
    /// false on error.
    pub fn wait(&self) -> bool {
        use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{WaitForSingleObject, INFINITE};
        unsafe {
            let h = HANDLE(self.raw as *mut _);
            WaitForSingleObject(h, INFINITE) == WAIT_OBJECT_0
        }
    }
}

#[cfg(windows)]
impl Drop for ShowSettingsEventHandle {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe {
            let _ = CloseHandle(HANDLE(self.raw as *mut _));
        }
    }
}

// Send/Sync: HANDLE is just a kernel object; sharing the raw usize
// across threads is fine — the OS handles synchronisation.
#[cfg(windows)]
unsafe impl Send for ShowSettingsEventHandle {}
#[cfg(windows)]
unsafe impl Sync for ShowSettingsEventHandle {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_returns_a_variant() {
        // Sanity: the call doesn't panic and returns one of the variants.
        // On non-Windows it's always Acquired; on Windows the result depends
        // on whether another test or process already holds the mutex.
        let r = SingleInstanceGuard::acquire("WireDeskHostSingleton-test-xyz");
        assert!(matches!(
            r,
            SingleInstanceResult::Acquired(_)
                | SingleInstanceResult::AlreadyRunning
                | SingleInstanceResult::Error(_)
        ));
    }

    #[test]
    fn try_acquire_with_retry_succeeds_when_free() {
        // Unique name keeps this test isolated from other test runs.
        let name = "WireDeskHostSingleton-retry-free-test";
        let r = try_acquire_with_retry(name, 3, 10);
        assert!(matches!(r, SingleInstanceResult::Acquired(_)));
    }

    #[cfg(windows)]
    #[test]
    fn try_acquire_with_retry_returns_already_running_after_retries() {
        // Hold the mutex first, then attempt to acquire again with retries —
        // every attempt must see ERROR_ALREADY_EXISTS until we give up.
        // Guarded with #[cfg(windows)] because the non-Windows stub always
        // returns Acquired and would never exercise the retry path.
        let name = "WireDeskHostSingleton-retry-busy-test";
        let _held = match SingleInstanceGuard::acquire(name) {
            SingleInstanceResult::Acquired(g) => g,
            other => panic!("expected initial acquire to succeed, got {:?}", match other {
                SingleInstanceResult::AlreadyRunning => "AlreadyRunning",
                SingleInstanceResult::Error(_) => "Error",
                SingleInstanceResult::Acquired(_) => unreachable!(),
            }),
        };
        let r = try_acquire_with_retry(name, 2, 10);
        assert!(matches!(r, SingleInstanceResult::AlreadyRunning));
    }
}
