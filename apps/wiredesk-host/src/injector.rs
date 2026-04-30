use wiredesk_core::error::Result;

/// Trait for injecting input events on the host machine.
pub trait InputInjector: Send {
    fn mouse_move_absolute(&mut self, x: u16, y: u16) -> Result<()>;
    fn mouse_button(&mut self, button: u8, pressed: bool) -> Result<()>;
    fn mouse_scroll(&mut self, delta_x: i16, delta_y: i16) -> Result<()>;
    fn key_down(&mut self, scancode: u16, modifiers: u8) -> Result<()>;
    fn key_up(&mut self, scancode: u16, modifiers: u8) -> Result<()>;
    fn release_all(&mut self) -> Result<()>;
}

/// Windows implementation using SendInput API.
#[cfg(target_os = "windows")]
pub struct WindowsInjector;

#[cfg(target_os = "windows")]
impl WindowsInjector {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

#[cfg(target_os = "windows")]
impl InputInjector for WindowsInjector {
    fn mouse_move_absolute(&mut self, x: u16, y: u16) -> Result<()> {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x as i32,
                    dy: y as i32,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        Ok(())
    }

    fn mouse_button(&mut self, button: u8, pressed: bool) -> Result<()> {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;

        let flags = match (button, pressed) {
            (0, true) => MOUSEEVENTF_LEFTDOWN,
            (0, false) => MOUSEEVENTF_LEFTUP,
            (1, true) => MOUSEEVENTF_RIGHTDOWN,
            (1, false) => MOUSEEVENTF_RIGHTUP,
            (2, true) => MOUSEEVENTF_MIDDLEDOWN,
            (2, false) => MOUSEEVENTF_MIDDLEUP,
            _ => return Ok(()),
        };

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        Ok(())
    }

    fn mouse_scroll(&mut self, _delta_x: i16, delta_y: i16) -> Result<()> {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta_y as u32,
                    dwFlags: MOUSEEVENTF_WHEEL,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        Ok(())
    }

    fn key_down(&mut self, scancode: u16, _modifiers: u8) -> Result<()> {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;

        // Extended scancodes (0xE0xx) need KEYEVENTF_EXTENDEDKEY flag
        let (scan, flags) = if scancode & 0xFF00 == 0xE000 {
            ((scancode & 0xFF) as u16, KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY)
        } else {
            (scancode, KEYEVENTF_SCANCODE)
        };

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        Ok(())
    }

    fn key_up(&mut self, scancode: u16, _modifiers: u8) -> Result<()> {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;

        let (scan, base_flags) = if scancode & 0xFF00 == 0xE000 {
            ((scancode & 0xFF) as u16, KEYEVENTF_SCANCODE | KEYEVENTF_EXTENDEDKEY)
        } else {
            (scancode, KEYEVENTF_SCANCODE)
        };

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: scan,
                    dwFlags: base_flags | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        // Release common modifier keys
        let _ = self.key_up(0x1D, 0); // Left Ctrl
        let _ = self.key_up(0x2A, 0); // Left Shift
        let _ = self.key_up(0x38, 0); // Left Alt
        let _ = self.mouse_button(0, false);
        let _ = self.mouse_button(1, false);
        let _ = self.mouse_button(2, false);
        Ok(())
    }
}

/// Mock injector for testing — records all calls.
#[derive(Default)]
#[allow(dead_code)]
pub struct MockInjector {
    pub events: Vec<InjectorEvent>,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub enum InjectorEvent {
    MouseMove { x: u16, y: u16 },
    MouseButton { button: u8, pressed: bool },
    MouseScroll { delta_x: i16, delta_y: i16 },
    KeyDown { scancode: u16, modifiers: u8 },
    KeyUp { scancode: u16, modifiers: u8 },
    ReleaseAll,
}

impl InputInjector for MockInjector {
    fn mouse_move_absolute(&mut self, x: u16, y: u16) -> Result<()> {
        self.events.push(InjectorEvent::MouseMove { x, y });
        Ok(())
    }

    fn mouse_button(&mut self, button: u8, pressed: bool) -> Result<()> {
        self.events.push(InjectorEvent::MouseButton { button, pressed });
        Ok(())
    }

    fn mouse_scroll(&mut self, delta_x: i16, delta_y: i16) -> Result<()> {
        self.events.push(InjectorEvent::MouseScroll { delta_x, delta_y });
        Ok(())
    }

    fn key_down(&mut self, scancode: u16, modifiers: u8) -> Result<()> {
        self.events.push(InjectorEvent::KeyDown { scancode, modifiers });
        Ok(())
    }

    fn key_up(&mut self, scancode: u16, modifiers: u8) -> Result<()> {
        self.events.push(InjectorEvent::KeyUp { scancode, modifiers });
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        self.events.push(InjectorEvent::ReleaseAll);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_records_events() {
        let mut inj = MockInjector::default();
        inj.mouse_move_absolute(100, 200).unwrap();
        inj.key_down(0x1E, 0x01).unwrap();
        inj.key_up(0x1E, 0x00).unwrap();
        inj.mouse_button(0, true).unwrap();
        inj.release_all().unwrap();

        assert_eq!(inj.events.len(), 5);
        assert_eq!(inj.events[0], InjectorEvent::MouseMove { x: 100, y: 200 });
        assert_eq!(inj.events[1], InjectorEvent::KeyDown { scancode: 0x1E, modifiers: 0x01 });
        assert_eq!(inj.events[4], InjectorEvent::ReleaseAll);
    }
}
