//! Windows autostart for the host.
//!
//! Two mechanisms, deliberately: a **logon task** in Task Scheduler with
//! "run with highest privileges", and the plain `HKCU\…\Run` value as a
//! fallback.
//!
//! The Run key alone is not enough, and that is why autostart went unused
//! for so long. Windows starts a `Run` entry with the *filtered* token, so
//! the host comes up unelevated — and an unelevated process cannot inject
//! input into a window that belongs to an elevated one (UIPI blocks
//! `SendInput` upward). The symptom is exactly what it looks like from the
//! Mac: most of the desktop reacts, and some windows quietly ignore every
//! click. A logon task registered with `HighestAvailable` starts the host
//! with the full token and no UAC prompt, which is the only way to get an
//! always-on host that can click everything.
//!
//! Creating that task needs elevation itself, so `enable` falls back to the
//! Run key when it cannot register one — better a host that starts
//! unelevated than one that does not start at all.
//!
//! On non-Windows targets everything here is a no-op so the rest of the
//! host code can call it unconditionally.

const APP_REG_NAME: &str = "WireDesk Host";
/// Task Scheduler name. Lives at the root of the task library, next to the
/// other third-party logon tasks.
#[cfg_attr(not(windows), allow(dead_code))]
const TASK_NAME: &str = "WireDesk Host";

/// Path of the running executable, unquoted — the form the scheduled task
/// stores and the form every comparison here uses.
pub fn expected_command() -> std::io::Result<String> {
    Ok(std::env::current_exe()?.display().to_string())
}

/// Strip one layer of surrounding double quotes, so a Run value
/// (`"C:\…\wiredesk-host.exe"`) compares equal to the same path taken out
/// of a scheduled task (`C:\…\wiredesk-host.exe`).
#[cfg_attr(not(windows), allow(dead_code))]
fn unquote(s: &str) -> String {
    let t = s.trim();
    t.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .unwrap_or(t)
        .to_string()
}

/// Whether startup should (re)write the autostart entry. True only when the
/// config asks for autostart *and* the registered command isn't already
/// exactly what this executable would register — so an entry pointing at an
/// old install path gets corrected instead of silently launching a stale
/// binary. Startup never *removes* an entry: the settings window treats an
/// externally-set one as authoritative (`run_on_startup || is_enabled()`),
/// and undoing that here would fight the user's own toggle.
pub fn needs_refresh(want: bool, stored: Option<&str>, expected: &str) -> bool {
    want && stored != Some(expected)
}

// --- Windows ---------------------------------------------------------------

#[cfg(windows)]
pub fn enable() -> std::io::Result<()> {
    match create_logon_task() {
        Ok(()) => {
            // Only one of the two may be armed, or the host starts twice at
            // logon. The single-instance mutex turns the second start into
            // "open Settings", which is not what anyone wants at boot.
            if let Err(e) = delete_run_value() {
                log::warn!("autostart: logon task created but Run value lingers: {e}");
            }
            Ok(())
        }
        // A task exists that this process could not rewrite - almost always
        // because it is running unelevated. Adding a Run value on top would
        // arm *both*: at the next logon the task would start whatever path
        // it still holds (an old install, possibly) and the Run value would
        // start this one, with the single-instance mutex picking a winner
        // at random. Report the failure instead, so the Settings window
        // says so and the user knows to relaunch as administrator.
        Err(e) if stored_task_present() => Err(std::io::Error::other(format!(
            "a 'WireDesk Host' logon task is already registered and could not be              updated ({e}). Run WireDesk as administrator and save again, or              delete the task in Task Scheduler."
        ))),
        Err(e) => {
            log::warn!(
                "autostart: no elevated logon task ({e}); falling back to the Run key. \
                 The host will start unelevated and will not be able to click \
                 windows of elevated applications."
            );
            set_run_value()
        }
    }
}

/// Whether a logon task is registered at all, whatever it points at.
#[cfg(windows)]
fn stored_task_present() -> bool {
    task_command().is_some()
}

#[cfg(windows)]
pub fn disable() -> std::io::Result<()> {
    let task = delete_logon_task();
    let run = delete_run_value();
    // Report the first real failure, but always attempt both.
    task.and(run)
}

#[cfg(windows)]
pub fn is_enabled() -> bool {
    stored_command().is_some()
}

/// The command currently registered to start the host at logon — the
/// scheduled task first, then the Run value. `None` when neither exists.
/// Lets the caller spot a *stale* entry, which a bare "does it exist"
/// check cannot.
#[cfg(windows)]
pub fn stored_command() -> Option<String> {
    task_command().or_else(run_value)
}

/// Run `schtasks.exe` with no console window flashing on the user's screen.
#[cfg(windows)]
fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    use std::os::windows::process::CommandExt;
    /// `CREATE_NO_WINDOW`
    const NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(NO_WINDOW)
        .output()
}

#[cfg(windows)]
fn create_logon_task() -> std::io::Result<()> {
    // The path is quoted *inside* the `/TR` value, not just passed as one
    // argument. `schtasks` splits that value into a command and its
    // arguments at the first space, so an install under
    // `C:\Program Files\WireDesk\` would register `C:\Program` with
    // `Files\WireDesk\wiredesk-host.exe` as its argument - and that is
    // precisely where this belongs, since a build directory the user can
    // write to must not be the source of an elevated logon task.
    let exe = format!("\"{}\"", expected_command()?);
    // `/RL HIGHEST` is the whole point: the task runs with the full
    // administrator token, so input injection reaches elevated windows.
    // `/SC ONLOGON` + the default interactive-token principal means it
    // starts in the user's own desktop session, with no UAC prompt.
    // `/F` overwrites a task left behind by an older install path.
    let out = schtasks(&[
        "/Create", "/TN", TASK_NAME, "/TR", &exe, "/SC", "ONLOGON", "/RL", "HIGHEST", "/F",
    ])?;
    if out.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "schtasks /Create: {}",
        console_text(&out.stderr, &out.stdout)
    )))
}

#[cfg(windows)]
fn delete_logon_task() -> std::io::Result<()> {
    if task_command().is_none() {
        return Ok(());
    }
    let out = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])?;
    if out.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "schtasks /Delete: {}",
        console_text(&out.stderr, &out.stdout)
    )))
}

#[cfg(windows)]
fn task_command() -> Option<String> {
    let out = schtasks(&["/Query", "/TN", TASK_NAME, "/XML"]).ok()?;
    if !out.status.success() {
        return None;
    }
    parse_task_command(&out.stdout)
}

/// Pick the message a failed `schtasks` run left behind — it writes to
/// stderr normally and to stdout in some locales.
#[cfg(windows)]
fn console_text(stderr: &[u8], stdout: &[u8]) -> String {
    let e = decode_console(stderr);
    if e.trim().is_empty() {
        decode_console(stdout).trim().to_string()
    } else {
        e.trim().to_string()
    }
}

/// Decode `schtasks` output: `/XML` answers in UTF-16LE with a BOM, plain
/// queries in the console code page. Treat the BOM as the signal and fall
/// back to a lossy UTF-8 read, which is right for ASCII either way.
#[cfg_attr(not(windows), allow(dead_code))]
fn decode_console(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let units: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Pull the `<Command>` out of a task definition. Hand-parsed rather than
/// pulled through an XML crate: one element, one shape, and the host
/// already avoids dependencies it can spell out in ten lines.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_task_command(xml: &[u8]) -> Option<String> {
    let text = decode_console(xml);
    let start = text.find("<Command>")? + "<Command>".len();
    let end = text[start..].find("</Command>")? + start;
    let raw = text[start..end].trim();
    if raw.is_empty() {
        return None;
    }
    // Task XML escapes only these in element text.
    let unescaped = raw
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&");
    Some(unquote(&unescaped))
}

#[cfg(windows)]
fn set_run_value() -> std::io::Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ,
    };

    // Quote the path so spaces in the path don't break parsing on launch.
    let value = format!("\"{}\"", expected_command()?);

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
fn delete_run_value() -> std::io::Result<()> {
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
            r if r == ERROR_FILE_NOT_FOUND => Ok(()), // already absent
            r => Err(std::io::Error::other(format!(
                "RegDeleteValueW: {:?}",
                r.to_hresult()
            ))),
        }
    }
}

#[cfg(windows)]
fn run_value() -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
        REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE,
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
            return None;
        }

        // First call sizes the buffer, second fills it.
        let mut kind = REG_VALUE_TYPE::default();
        let mut len: u32 = 0;
        let sized = RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut len),
        );
        if sized.is_err() || (kind != REG_SZ && kind != REG_EXPAND_SZ) || len == 0 {
            let _ = RegCloseKey(hkey);
            return None;
        }

        let mut buf = vec![0u8; len as usize];
        let read = RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        );
        let _ = RegCloseKey(hkey);
        if read.is_err() {
            return None;
        }
        buf.truncate(len as usize);
        Some(unquote(&decode_reg_sz(&buf)))
    }
}

/// Decode a `REG_SZ` payload: little-endian UTF-16 with an optional
/// trailing NUL, and possibly an odd trailing byte if the value was
/// written by something careless.
#[cfg_attr(not(windows), allow(dead_code))]
fn decode_reg_sz(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(windows)]
fn encode_utf16(s: &str) -> Vec<u16> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
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

#[cfg(not(windows))]
pub fn stored_command() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real registry / Task Scheduler probe — only meaningful on Windows.
    /// Marked `#[ignore]` so it doesn't run in CI / on macOS dev. Run
    /// manually via `cargo test -p wiredesk-host -- --ignored`.
    #[test]
    #[ignore]
    fn windows_enable_then_disable_round_trip() {
        let pre = is_enabled();
        enable().expect("enable");
        assert!(is_enabled(), "expected enabled after enable()");
        assert_eq!(
            stored_command().as_deref(),
            expected_command().ok().as_deref(),
            "registered command should point at this executable"
        );
        disable().expect("disable");
        assert!(!is_enabled(), "expected disabled after disable()");
        // Restore prior state if it was set.
        if pre {
            let _ = enable();
        }
    }

    #[test]
    fn refresh_only_when_wanted_and_out_of_date() {
        let want = r"C:\app\wiredesk-host.exe";
        // Off in config — startup keeps its hands off the entry either way.
        assert!(!needs_refresh(false, None, want));
        assert!(!needs_refresh(
            false,
            Some(r"C:\old\wiredesk-host.exe"),
            want
        ));
        // On, but nothing registered yet — the case the settings window
        // used to be the only way to reach.
        assert!(needs_refresh(true, None, want));
        // On and already exact — no write, no log noise on every boot.
        assert!(!needs_refresh(true, Some(want), want));
        // On but pointing at where the exe used to live.
        assert!(needs_refresh(true, Some(r"C:\old\wiredesk-host.exe"), want));
    }

    #[test]
    fn quotes_are_stripped_so_both_mechanisms_compare_equal() {
        assert_eq!(unquote(r#""C:\a\b.exe""#), r"C:\a\b.exe");
        assert_eq!(unquote(r"C:\a\b.exe"), r"C:\a\b.exe");
        assert_eq!(unquote(r#"  "C:\a b\c.exe"  "#), r"C:\a b\c.exe");
        // A lone quote is not a pair — leave it be rather than mangle it.
        assert_eq!(unquote(r#""C:\a\b.exe"#), r#""C:\a\b.exe"#);
    }

    #[test]
    fn reg_sz_decodes_utf16_and_stops_at_nul() {
        let mut bytes: Vec<u8> = r"C:\a\b.exe"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        bytes.extend_from_slice(&[0, 0]); // trailing NUL
        bytes.extend_from_slice(&[0x41, 0x00]); // garbage past the NUL
        assert_eq!(decode_reg_sz(&bytes), r"C:\a\b.exe");
        assert_eq!(decode_reg_sz(&[]), "");
        // Odd trailing byte must not panic.
        assert_eq!(decode_reg_sz(&[0x41, 0x00, 0x42]), "A");
    }

    #[test]
    fn task_xml_yields_the_registered_command() {
        let xml = br#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2"><Actions Context="Author">
<Exec><Command>C:\Download\wiredesk-host.exe</Command></Exec>
</Actions></Task>"#;
        assert_eq!(
            parse_task_command(xml).as_deref(),
            Some(r"C:\Download\wiredesk-host.exe")
        );
        // UTF-16LE with a BOM is what `schtasks /XML` actually writes.
        let mut utf16: Vec<u8> = vec![0xFF, 0xFE];
        utf16.extend(
            "<Exec><Command>C:\\a b\\host.exe</Command></Exec>"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes()),
        );
        assert_eq!(
            parse_task_command(&utf16).as_deref(),
            Some(r"C:\a b\host.exe")
        );
        // Quoting and XML escaping both normalise away.
        let esc = br#"<Command>&quot;C:\a &amp; b\host.exe&quot;</Command>"#;
        assert_eq!(
            parse_task_command(esc).as_deref(),
            Some(r"C:\a & b\host.exe")
        );
        // No task, no command.
        assert_eq!(parse_task_command(b"ERROR: cannot find the file"), None);
        assert_eq!(parse_task_command(b"<Command>  </Command>"), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_stubs_dont_panic() {
        // No-op semantics: enable/disable always succeed, is_enabled always false.
        assert!(enable().is_ok());
        assert!(disable().is_ok());
        assert!(!is_enabled());
        assert_eq!(stored_command(), None);
    }
}
