//! Opt-in Chrome startup. Never use the elevated host's process token.

#[cfg(windows)]
pub fn start(enabled: bool) {
    if !enabled {
        return;
    }
    // Browser/Explorer startup may be slow at logon. Keep it off the GUI
    // and session threads; shell routing and channel budgets are untouched.
    if let Err(e) = std::thread::Builder::new()
        .name("browser-startup".into())
        .spawn(|| {
            if listening() {
                log::info!("browser: CDP port already listening; leaving existing browser alone");
                return;
            }
            let mut launched = launch();
            // At logon the user's Explorer COM server may not be ready
            // yet. Retry errors only; never repeat a successful launch.
            for _ in 0..9 {
                if launched.is_ok() || listening() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
                launched = launch();
            }
            match launched {
                Ok(()) => {
                    for _ in 0..30 {
                        if listening() {
                            log::info!("browser: CDP port ready on 127.0.0.1:19222");
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                    log::warn!("browser: Chrome launched but CDP port did not become ready");
                }
                Err(e) => log::warn!("browser: automatic startup failed: {e}"),
            }
        })
    {
        log::warn!("browser: cannot start worker: {e}");
    }
}

#[cfg(not(windows))]
pub fn start(_enabled: bool) {}

#[cfg(windows)]
fn listening() -> bool {
    std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], 19222).into(),
        std::time::Duration::from_millis(150),
    )
    .is_ok()
}

#[cfg(any(windows, test))]
fn arguments(profile: &std::path::Path) -> Result<String, &'static str> {
    let path = profile.to_str().ok_or("profile path is not UTF-8")?;
    if path.contains(['"', '\r', '\n']) || !profile.is_absolute() {
        return Err("invalid browser profile path");
    }
    Ok(format!(
        "--remote-debugging-address=127.0.0.1 --remote-debugging-port=19222 --user-data-dir=\"{path}\" about:blank"
    ))
}

#[cfg(windows)]
fn launch() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use windows::core::{Interface, BSTR, VARIANT};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, IServiceProvider, CLSCTX_LOCAL_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        IShellBrowser, IShellDispatch2, IShellFolderViewDual, IShellWindows, SID_STopLevelBrowser,
        ShellWindows, SVGIO_BACKGROUND, SWC_DESKTOP, SWFO_NEEDDISPATCH,
    };

    let local = std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA missing")?;
    let profile = PathBuf::from(&local).join("WireDeskBrowser");
    let args = arguments(&profile)?;
    let chrome = ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|root| PathBuf::from(root).join("Google/Chrome/Application/chrome.exe"))
        .find(|p| p.is_absolute() && p.is_file())
        .ok_or("Chrome not found")?;
    let chrome = chrome.to_str().ok_or("Chrome path is not UTF-8")?;

    // Microsoft ExecuteInExplorer pattern. COM pointers drop BEFORE the
    // apartment guard; never fall back to Command::spawn/ShellExecuteW.
    // https://learn.microsoft.com/windows/win32/shell/samples-execinexplorer
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _apartment = Apartment;
        let shell: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_LOCAL_SERVER)?;
        let mut hwnd = 0;
        let desktop = shell.FindWindowSW(
            &VARIANT::default(),
            &VARIANT::default(),
            SWC_DESKTOP,
            &mut hwnd,
            SWFO_NEEDDISPATCH,
        )?;
        ensure_unelevated(hwnd)?;
        let provider: IServiceProvider = desktop.cast()?;
        let browser: IShellBrowser = provider.QueryService(&SID_STopLevelBrowser)?;
        let view = browser.QueryActiveShellView()?;
        let background: windows::Win32::System::Com::IDispatch =
            view.GetItemObject(SVGIO_BACKGROUND)?;
        let folder: IShellFolderViewDual = background.cast()?;
        let dispatch: IShellDispatch2 = folder.Application()?.cast()?;
        dispatch.ShellExecute(
            &BSTR::from(chrome),
            &VARIANT::from(args.as_str()),
            &VARIANT::default(),
            &VARIANT::from("open"),
            &VARIANT::from(1i32),
        )?;
    }
    Ok(())
}

#[cfg(windows)]
unsafe fn ensure_unelevated(hwnd: i32) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
    use windows::Win32::Security::{
        EqualSid, GetTokenInformation, TokenElevation, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
    let mut pid = 0;
    if hwnd == 0 || GetWindowThreadProcessId(HWND(hwnd as isize as *mut _), Some(&mut pid)) == 0 {
        return Err("desktop Explorer unavailable".into());
    }
    let process = Handle(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?);
    let mut token = HANDLE::default();
    OpenProcessToken(process.0, TOKEN_QUERY, &mut token)?;
    let token = Handle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut size = 0;
    GetTokenInformation(
        token.0,
        TokenElevation,
        Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
        std::mem::size_of::<TOKEN_ELEVATION>() as u32,
        &mut size,
    )?;
    if elevation.TokenIsElevated != 0 {
        return Err("desktop Explorer is elevated; refusing elevated Chrome".into());
    }
    // Profile discovery uses this account's LOCALAPPDATA. Refuse a host
    // started as a different user rather than mixing account profiles.
    let mut own = HANDLE::default();
    OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut own)?;
    let own = Handle(own);
    // Aligned buffers large enough for TOKEN_USER + maximum Windows SID;
    // keep both alive until EqualSid has consumed their embedded pointers.
    let mut desktop_user = [0usize; 32];
    let mut own_user = [0usize; 32];
    for (handle, buffer) in [(token.0, &mut desktop_user), (own.0, &mut own_user)] {
        GetTokenInformation(
            handle,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            std::mem::size_of_val(buffer) as u32,
            &mut size,
        )?;
    }
    EqualSid(
        (*desktop_user.as_ptr().cast::<TOKEN_USER>()).User.Sid,
        (*own_user.as_ptr().cast::<TOKEN_USER>()).User.Sid,
    )
    .map_err(|_| "host and desktop must use the same Windows account")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_with_spaces_is_one_argument_and_debugging_is_loopback_only() {
        let root = std::env::temp_dir().join("WireDesk Browser");
        let args = arguments(&root).unwrap();
        assert!(args.contains(&format!("--user-data-dir=\"{}\"", root.display())));
        assert!(args.contains("--remote-debugging-address=127.0.0.1"));
        assert!(!args.contains("--no-sandbox"));
    }

    #[test]
    fn reject_relative_and_argument_breakout() {
        assert!(arguments(std::path::Path::new("relative")).is_err());
        for suffix in ["bad\" --no-sandbox", "bad\r", "bad\n"] {
            assert!(arguments(&std::env::temp_dir().join(suffix)).is_err());
        }
    }
}
