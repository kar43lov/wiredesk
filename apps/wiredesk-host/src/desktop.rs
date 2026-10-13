//! Primary Windows desktop, requested over the existing bounded stream hub.
//! No listening port, shell, subprocess, clipboard or stored screenshots.
use serde::Deserialize;
use std::io;
use std::sync::{Arc, Mutex};

#[derive(Deserialize, Debug)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Click { x: u16, y: u16, button: u8 },
    Drag { points: Vec<[u16; 2]> },
    Wheel { x: u16, y: u16, delta: i16 },
    Key { vk: u16, modifiers: u8 },
    Text { text: String },
}
impl Action {
    fn validate(&self) -> io::Result<()> {
        let valid = match self {
            Self::Click { button, .. } => *button <= 2,
            Self::Drag { points } => (2..=64).contains(&points.len()),
            Self::Wheel { delta, .. } => delta.unsigned_abs() <= 1200,
            Self::Key { vk, modifiers } => (1..=254).contains(vk) && modifiers & !7 == 0,
            Self::Text { text } => !text.is_empty() && text.len() <= 4096,
        };
        if valid {
            Ok(())
        } else {
            Err(io::Error::other("invalid desktop input"))
        }
    }
}

pub fn handler() -> wiredesk_browser::RequestHandler {
    let serial = Mutex::new(());
    Arc::new(move |request| {
        // Bound concurrent captures/input batches; never queue old frames.
        let _guard = serial
            .lock()
            .map_err(|_| io::Error::other("desktop worker unavailable"))?;
        match handle(request) {
            Ok(r) => Ok(r),
            Err(e) => {
                log::debug!("desktop request failed: {e}");
                Ok(reply(
                    "400 Bad Request",
                    "text/plain",
                    b"Desktop request failed",
                ))
            }
        }
    })
}
fn handle(request: &[u8]) -> io::Result<Vec<u8>> {
    let (line, body) = wiredesk_browser::request_body(request)?
        .ok_or_else(|| io::Error::other("incomplete desktop request"))?;
    match line {
        "GET /desktop/frame HTTP/1.1" if body.is_empty() => {
            let jpeg = capture()?;
            Ok(reply("200 OK", "image/jpeg", &jpeg))
        }
        "POST /desktop/input HTTP/1.1" => {
            let action: Action = serde_json::from_slice(body).map_err(io::Error::other)?;
            action.validate()?;
            inject(action)?;
            Ok(reply("200 OK", "application/json", b"{}"))
        }
        _ => Err(io::Error::other("unknown desktop endpoint")),
    }
}
fn reply(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut result = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",body.len()).into_bytes();
    result.extend_from_slice(body);
    result
}

#[cfg(not(target_os = "windows"))]
fn capture() -> io::Result<Vec<u8>> {
    Err(io::Error::other("desktop capture requires Windows"))
}
#[cfg(not(target_os = "windows"))]
fn inject(_action: Action) -> io::Result<()> {
    Err(io::Error::other("desktop input requires Windows"))
}

#[cfg(target_os = "windows")]
fn capture() -> io::Result<Vec<u8>> {
    use windows::Win32::{Foundation::HWND, Graphics::Gdi::*, UI::WindowsAndMessaging::*};
    // All handles are created/used/destroyed on this stream worker.
    unsafe {
        let sw = GetSystemMetrics(SM_CXSCREEN);
        let sh = GetSystemMetrics(SM_CYSCREEN);
        if !(1..=16384).contains(&sw) || !(1..=16384).contains(&sh) {
            return Err(io::Error::other("invalid screen size"));
        }
        let w = sw.min(1280);
        let h = ((i64::from(sh) * i64::from(w)) / i64::from(sw)).max(1) as i32;
        if h > 8192 {
            return Err(io::Error::other("screen aspect unsupported"));
        }
        let desktop = GetDC(HWND::default());
        if desktop.is_invalid() {
            return Err(io::Error::other("GetDC failed"));
        }
        let memory = CreateCompatibleDC(desktop);
        if memory.is_invalid() {
            ReleaseDC(HWND::default(), desktop);
            return Err(io::Error::other("CreateCompatibleDC failed"));
        }
        let bitmap = CreateCompatibleBitmap(desktop, w, h);
        if bitmap.is_invalid() {
            let _ = DeleteDC(memory);
            ReleaseDC(HWND::default(), desktop);
            return Err(io::Error::other("CreateCompatibleBitmap failed"));
        }
        let old = SelectObject(memory, bitmap);
        let result = (|| {
            if old.is_invalid() {
                return Err(io::Error::other("SelectObject failed"));
            }
            SetStretchBltMode(memory, HALFTONE);
            let blit = StretchBlt(
                memory,
                0,
                0,
                w,
                h,
                desktop,
                0,
                0,
                sw,
                sh,
                SRCCOPY | CAPTUREBLT,
            );
            SelectObject(memory, old);
            if !blit.as_bool() {
                return Err(io::Error::other("StretchBlt failed"));
            }
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut pixels = vec![0u8; w as usize * h as usize * 4];
            if GetDIBits(
                desktop,
                bitmap,
                0,
                h as u32,
                Some(pixels.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            ) != h
            {
                return Err(io::Error::other("GetDIBits failed"));
            }
            let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
            for p in pixels.as_chunks::<4>().0 {
                rgb.extend_from_slice(&[p[2], p[1], p[0]]);
            }
            let mut jpeg = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 75)
                .encode(&rgb, w as u32, h as u32, image::ExtendedColorType::Rgb8)
                .map_err(io::Error::other)?;
            if jpeg.len() > 2 * 1024 * 1024 - 1024 {
                return Err(io::Error::other("desktop frame too large"));
            }
            Ok(jpeg)
        })();
        // Restore before DeleteObject even on error; no leaked GDI handles.
        if !old.is_invalid() {
            SelectObject(memory, old);
        }
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory);
        ReleaseDC(HWND::default(), desktop);
        result
    }
}

#[cfg(target_os = "windows")]
fn inject(action: Action) -> io::Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    fn key(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        }
    }
    fn mouse(x: u16, y: u16, flags: MOUSE_EVENT_FLAGS, data: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x as i32,
                    dy: y as i32,
                    mouseData: data,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        }
    }
    let mut events = Vec::new();
    match action {
        Action::Click { x, y, button } => {
            events.push(mouse(x, y, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, 0));
            let (down, up) = match button {
                0 => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
                1 => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
                _ => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
            };
            events.push(mouse(0, 0, down, 0));
            events.push(mouse(0, 0, up, 0));
        }
        Action::Drag { points } => {
            events.push(mouse(
                points[0][0],
                points[0][1],
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                0,
            ));
            events.push(mouse(0, 0, MOUSEEVENTF_LEFTDOWN, 0));
            for [x, y] in points.into_iter().skip(1) {
                events.push(mouse(x, y, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, 0));
            }
            events.push(mouse(0, 0, MOUSEEVENTF_LEFTUP, 0));
        }
        Action::Wheel { x, y, delta } => {
            events.push(mouse(x, y, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE, 0));
            events.push(mouse(0, 0, MOUSEEVENTF_WHEEL, delta as u32));
        }
        Action::Key { vk, modifiers } => {
            let mods = [(1, 0x12), (2, 0x11), (4, 0x10)];
            for (bit, code) in mods {
                if modifiers & bit != 0 {
                    events.push(key(code, 0, KEYBD_EVENT_FLAGS(0)));
                }
            }
            events.push(key(vk, 0, KEYBD_EVENT_FLAGS(0)));
            events.push(key(vk, 0, KEYEVENTF_KEYUP));
            for (bit, code) in mods.into_iter().rev() {
                if modifiers & bit != 0 {
                    events.push(key(code, 0, KEYEVENTF_KEYUP));
                }
            }
        }
        Action::Text { text } => {
            for unit in text.encode_utf16() {
                events.push(key(0, unit, KEYEVENTF_UNICODE));
                events.push(key(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
        }
    }
    let sent = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) } as usize;
    if sent != events.len() {
        // Release only the keys/buttons this batch pressed, never all user input.
        let ups: Vec<_> = events
            .iter()
            .copied()
            .filter(|e| unsafe {
                match e.r#type {
                    INPUT_KEYBOARD => e.Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP),
                    INPUT_MOUSE => {
                        e.Anonymous.mi.dwFlags.0
                            & (MOUSEEVENTF_LEFTUP | MOUSEEVENTF_RIGHTUP | MOUSEEVENTF_MIDDLEUP).0
                            != 0
                    }
                    _ => false,
                }
            })
            .collect();
        unsafe { SendInput(&ups, std::mem::size_of::<INPUT>() as i32) };
        return Err(io::Error::other("desktop input was blocked"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unknown_actions_unbounded_input_and_arbitrary_paths() {
        for json in [
            r#"{"kind":"click","x":0,"y":0,"button":3}"#,
            r#"{"kind":"key","vk":255,"modifiers":0}"#,
            r#"{"kind":"key","vk":65,"modifiers":8}"#,
            r#"{"kind":"drag","points":[]}"#,
            r#"{"kind":"text","text":""}"#,
        ] {
            let a: Action = serde_json::from_str(json).unwrap();
            assert!(a.validate().is_err());
        }
        assert!(serde_json::from_str::<Action>(r#"{"kind":"exec","command":"x"}"#).is_err());
        assert!(handle(b"GET /arbitrary HTTP/1.1\r\n\r\n").is_err());
        assert!(Action::Text {
            text: "x".repeat(4097)
        }
        .validate()
        .is_err());
        Action::Key {
            vk: 65,
            modifiers: 2,
        }
        .validate()
        .unwrap();
    }
}
