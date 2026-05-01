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
}
