use eframe::egui;

/// Maps egui Key to Windows scan code (Set 1).
/// Using scancodes instead of VK codes ensures correct behavior
/// regardless of keyboard layout on Host (including Cyrillic).
pub fn egui_key_to_scancode(key: &egui::Key) -> Option<u16> {
    use egui::Key;
    Some(match key {
        // Letters (US QWERTY scancodes, work for any layout on Host)
        Key::A => 0x1E,
        Key::B => 0x30,
        Key::C => 0x2E,
        Key::D => 0x20,
        Key::E => 0x12,
        Key::F => 0x21,
        Key::G => 0x22,
        Key::H => 0x23,
        Key::I => 0x17,
        Key::J => 0x24,
        Key::K => 0x25,
        Key::L => 0x26,
        Key::M => 0x32,
        Key::N => 0x31,
        Key::O => 0x18,
        Key::P => 0x19,
        Key::Q => 0x10,
        Key::R => 0x13,
        Key::S => 0x1F,
        Key::T => 0x14,
        Key::U => 0x16,
        Key::V => 0x2F,
        Key::W => 0x11,
        Key::X => 0x2D,
        Key::Y => 0x15,
        Key::Z => 0x2C,

        // Numbers
        Key::Num0 => 0x0B,
        Key::Num1 => 0x02,
        Key::Num2 => 0x03,
        Key::Num3 => 0x04,
        Key::Num4 => 0x05,
        Key::Num5 => 0x06,
        Key::Num6 => 0x07,
        Key::Num7 => 0x08,
        Key::Num8 => 0x09,
        Key::Num9 => 0x0A,

        // Function keys
        Key::F1 => 0x3B,
        Key::F2 => 0x3C,
        Key::F3 => 0x3D,
        Key::F4 => 0x3E,
        Key::F5 => 0x3F,
        Key::F6 => 0x40,
        Key::F7 => 0x41,
        Key::F8 => 0x42,
        Key::F9 => 0x43,
        Key::F10 => 0x44,
        Key::F11 => 0x57,
        Key::F12 => 0x58,

        // Special keys
        Key::Escape => 0x01,
        Key::Tab => 0x0F,
        Key::Backspace => 0x0E,
        Key::Enter => 0x1C,
        Key::Space => 0x39,
        Key::Insert => 0xE052,
        Key::Delete => 0xE053,
        Key::Home => 0xE047,
        Key::End => 0xE04F,
        Key::PageUp => 0xE049,
        Key::PageDown => 0xE051,

        // Arrow keys (extended scancodes)
        Key::ArrowUp => 0xE048,
        Key::ArrowDown => 0xE050,
        Key::ArrowLeft => 0xE04B,
        Key::ArrowRight => 0xE04D,

        // Punctuation
        Key::Minus => 0x0C,
        Key::Equals => 0x0D,
        Key::OpenBracket => 0x1A,
        Key::CloseBracket => 0x1B,
        Key::Backslash => 0x2B,
        Key::Semicolon => 0x27,
        Key::Quote => 0x28,
        Key::Backtick => 0x29,
        Key::Comma => 0x33,
        Key::Period => 0x34,
        Key::Slash => 0x35,

        _ => return None,
    })
}

/// Convert egui Modifiers to our modifier bitmap.
/// On macOS, Cmd is remapped to Ctrl for Windows.
pub fn egui_modifiers_to_u8(modifiers: &egui::Modifiers) -> u8 {
    let mut m = 0u8;
    // Cmd on Mac → Ctrl on Windows
    if modifiers.ctrl || modifiers.command {
        m |= 0x01; // CTRL
    }
    if modifiers.shift {
        m |= 0x02; // SHIFT
    }
    if modifiers.alt {
        m |= 0x04; // ALT
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_scancodes() {
        assert_eq!(egui_key_to_scancode(&egui::Key::A), Some(0x1E));
        assert_eq!(egui_key_to_scancode(&egui::Key::Z), Some(0x2C));
    }

    #[test]
    fn number_scancodes() {
        assert_eq!(egui_key_to_scancode(&egui::Key::Num0), Some(0x0B));
        assert_eq!(egui_key_to_scancode(&egui::Key::Num9), Some(0x0A));
    }

    #[test]
    fn function_keys() {
        assert_eq!(egui_key_to_scancode(&egui::Key::F1), Some(0x3B));
        assert_eq!(egui_key_to_scancode(&egui::Key::F12), Some(0x58));
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(egui_key_to_scancode(&egui::Key::ArrowUp), Some(0xE048));
    }

    #[test]
    fn special_keys() {
        assert_eq!(egui_key_to_scancode(&egui::Key::Escape), Some(0x01));
        assert_eq!(egui_key_to_scancode(&egui::Key::Enter), Some(0x1C));
        assert_eq!(egui_key_to_scancode(&egui::Key::Space), Some(0x39));
    }

    #[test]
    fn modifiers_cmd_to_ctrl() {
        let mods = egui::Modifiers { command: true, ..Default::default() };
        assert_eq!(egui_modifiers_to_u8(&mods), 0x01);
    }

    #[test]
    fn modifiers_combined() {
        let mods = egui::Modifiers { ctrl: true, shift: true, alt: true, ..Default::default() };
        assert_eq!(egui_modifiers_to_u8(&mods), 0x07);
    }
}
