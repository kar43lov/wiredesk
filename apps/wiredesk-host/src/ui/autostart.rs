//! Windows autostart toggle via HKCU\Software\Microsoft\Windows\CurrentVersion\Run.
//!
//! No external crate — we use the `windows` crate already pulled in for
//! SendInput. On non-Windows targets we expose `is_enabled() -> false` and
//! `enable() / disable()` as no-ops so the rest of the host code can call
//! them unconditionally.

const APP_REG_NAME: &str = "WireDesk Host";

#[cfg(windows)]
pub fn enable() -> std::io::Result<()> {
    use std::path::PathBuf;
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_SZ,
    };

    let exe: PathBuf = std::env::current_exe()?;
    // Quote the path so spaces in the path don't break parsing on launch.
    let value = format!("\"{}\"", exe.display());

    let subkey = encode_utf16(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let name = encode_utf16(APP_REG_NAME);
    let data = encode_utf16(&value);

    unsafe {
        let mut hkey = HKEY::default();
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
        .ok()
        .map_err(|e| std::io::Error::other(format!("RegOpenKeyExW: {e}")))?;

        let bytes = std::slice::from_raw_parts(
            data.as_ptr() as *const u8,
            data.len() * std::mem::size_of::<u16>(),
        );

        let result = RegSetValueExW(hkey, PCWSTR(name.as_ptr()), 0, REG_SZ, Some(bytes));
        let _ = RegCloseKey(hkey);
        result
            .ok()
            .map_err(|e| std::io::Error::other(format!("RegSetValueExW: {e}")))?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn disable() -> std::io::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    };

    let subkey = encode_utf16(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let name = encode_utf16(APP_REG_NAME);

    unsafe {
        let mut hkey = HKEY::default();
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut hkey,
        )
        .ok()
        .map_err(|e| std::io::Error::other(format!("RegOpenKeyExW: {e}")))?;

        let result = RegDeleteValueW(hkey, PCWSTR(name.as_ptr()));
        let _ = RegCloseKey(hkey);

        match result {
            r if r.is_ok() => Ok(()),
            r if r == ERROR_FILE_NOT_FOUND => Ok(()), // already disabled
            r => Err(std::io::Error::other(format!(
                "RegDeleteValueW: {:?}",
                r.to_hresult()
            ))),
        }
    }
}

#[cfg(windows)]
pub fn is_enabled() -> bool {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    };

    let subkey = encode_utf16(r"Software\Microsoft\Windows\CurrentVersion\Run");
    let name = encode_utf16(APP_REG_NAME);

    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            KEY_QUERY_VALUE,
            &mut hkey,
        )
        .is_err()
        {
            return false;
        }
        let r = RegQueryValueExW(hkey, PCWSTR(name.as_ptr()), None, None, None, None);
        let _ = RegCloseKey(hkey);
        r.is_ok()
    }
}

#[cfg(windows)]
fn encode_utf16(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

// --- Non-Windows stubs ------------------------------------------------------
//
// These let the rest of the host code call `autostart::*` unconditionally
// without spraying `cfg(windows)` everywhere. On macOS / Linux we silently
// claim "not enabled" and "no-op" for toggles.

#[cfg(not(windows))]
pub fn enable() -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn disable() -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real registry probe — only meaningful on Windows. Marked `#[ignore]`
    /// so it doesn't run in CI / on macOS dev. Run manually on a Windows
    /// machine via `cargo test -p wiredesk-host -- --ignored`.
    #[test]
    #[ignore]
    fn windows_enable_then_disable_round_trip() {
        let pre = is_enabled();
        enable().expect("enable");
        assert!(is_enabled(), "expected enabled after enable()");
        disable().expect("disable");
        assert!(!is_enabled(), "expected disabled after disable()");
        // Restore prior state if it was set.
        if pre {
            let _ = enable();
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_stubs_dont_panic() {
        // No-op semantics: enable/disable always succeed, is_enabled always false.
        assert!(enable().is_ok());
        assert!(disable().is_ok());
        assert!(!is_enabled());
    }
}
