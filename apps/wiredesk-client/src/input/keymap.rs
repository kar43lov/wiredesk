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

/// Maps macOS virtual keycode (CGKeyCode) to Windows scancode (Set 1).
/// Used by the CGEventTap path when capture-mode is on. Same scancode space
/// as egui_key_to_scancode — Host's WindowsInjector treats both identically.
///
/// Source for Mac VK codes: /System/Library/Frameworks/Carbon.framework/...Events.h.
pub fn cgkeycode_to_scancode(keycode: u16) -> Option<u16> {
    Some(match keycode {
        // Letters
        0x00 => 0x1E, // A
        0x0B => 0x30, // B
        0x08 => 0x2E, // C
        0x02 => 0x20, // D
        0x0E => 0x12, // E
        0x03 => 0x21, // F
        0x05 => 0x22, // G
        0x04 => 0x23, // H
        0x22 => 0x17, // I
        0x26 => 0x24, // J
        0x28 => 0x25, // K
        0x25 => 0x26, // L
        0x2E => 0x32, // M
        0x2D => 0x31, // N
        0x1F => 0x18, // O
        0x23 => 0x19, // P
        0x0C => 0x10, // Q
        0x0F => 0x13, // R
        0x01 => 0x1F, // S
        0x11 => 0x14, // T
        0x20 => 0x16, // U
        0x09 => 0x2F, // V
        0x0D => 0x11, // W
        0x07 => 0x2D, // X
        0x10 => 0x15, // Y
        0x06 => 0x2C, // Z

        // Top-row numbers (Mac VK 0x12-0x1D, with quirky ordering)
        0x1D => 0x0B, // 0
        0x12 => 0x02, // 1
        0x13 => 0x03, // 2
        0x14 => 0x04, // 3
        0x15 => 0x05, // 4
        0x17 => 0x06, // 5
        0x16 => 0x07, // 6
        0x1A => 0x08, // 7
        0x1C => 0x09, // 8
        0x19 => 0x0A, // 9

        // Punctuation
        0x18 => 0x0D, // =
        0x1B => 0x0C, // -
        0x21 => 0x1A, // [
        0x1E => 0x1B, // ]
        0x2A => 0x2B, // \
        0x29 => 0x27, // ;
        0x27 => 0x28, // '
        0x32 => 0x29, // `
        0x2B => 0x33, // ,
        0x2F => 0x34, // .
        0x2C => 0x35, // /

        // Special
        0x24 => 0x1C, // Return/Enter
        0x30 => 0x0F, // Tab
        0x31 => 0x39, // Space
        0x33 => 0x0E, // Backspace (Mac calls it Delete)
        0x35 => 0x01, // Escape

        // Arrows (extended scancodes — host applies KEYEVENTF_EXTENDEDKEY)
        0x7B => 0xE04B, // Left
        0x7C => 0xE04D, // Right
        0x7D => 0xE050, // Down
        0x7E => 0xE048, // Up

        // Navigation
        0x73 => 0xE047, // Home
        0x77 => 0xE04F, // End
        0x74 => 0xE049, // PageUp
        0x79 => 0xE051, // PageDown
        0x75 => 0xE053, // Forward Delete

        // Function keys
        0x7A => 0x3B, // F1
        0x78 => 0x3C, // F2
        0x63 => 0x3D, // F3
        0x76 => 0x3E, // F4
        0x60 => 0x3F, // F5
        0x61 => 0x40, // F6
        0x62 => 0x41, // F7
        0x64 => 0x42, // F8
        0x65 => 0x43, // F9
        0x6D => 0x44, // F10
        0x67 => 0x57, // F11
        0x6F => 0x58, // F12

        _ => return None,
    })
}

/// CGEventFlags bit positions for keyboard modifiers.
/// See <https://developer.apple.com/documentation/coregraphics/cgeventflags>
pub const CG_FLAG_SHIFT: u64 = 1 << 17;
pub const CG_FLAG_CONTROL: u64 = 1 << 18;
pub const CG_FLAG_ALT: u64 = 1 << 19; // Mac Option key
pub const CG_FLAG_COMMAND: u64 = 1 << 20;

/// Win Set 1 scancodes for modifier keys (left-side variants).
pub const WIN_SCAN_LCTRL: u16 = 0x1D;
pub const WIN_SCAN_LSHIFT: u16 = 0x2A;
pub const WIN_SCAN_LALT: u16 = 0x38;

/// Pure function: given current and previous CGEventFlags state, produce the
/// list of (Windows scancode, pressed) modifier events to emit.
///
/// Cmd OR Ctrl on Mac collapses to Ctrl on Windows — pressing both at once
/// emits a single Ctrl-down, releasing one while holding the other emits no
/// up event. Caller should hold prev_flags between FlagsChanged events.
pub fn cg_flag_change_to_scancodes(flags: u64, prev: u64) -> Vec<(u16, bool)> {
    let mut out = Vec::new();

    let mac_ctrl_mask = CG_FLAG_COMMAND | CG_FLAG_CONTROL;
    let was_ctrl = (prev & mac_ctrl_mask) != 0;
    let is_ctrl = (flags & mac_ctrl_mask) != 0;
    if !was_ctrl && is_ctrl {
        out.push((WIN_SCAN_LCTRL, true));
    } else if was_ctrl && !is_ctrl {
        out.push((WIN_SCAN_LCTRL, false));
    }

    let was_shift = (prev & CG_FLAG_SHIFT) != 0;
    let is_shift = (flags & CG_FLAG_SHIFT) != 0;
    if !was_shift && is_shift {
        out.push((WIN_SCAN_LSHIFT, true));
    } else if was_shift && !is_shift {
        out.push((WIN_SCAN_LSHIFT, false));
    }

    let was_alt = (prev & CG_FLAG_ALT) != 0;
    let is_alt = (flags & CG_FLAG_ALT) != 0;
    if !was_alt && is_alt {
        out.push((WIN_SCAN_LALT, true));
    } else if was_alt && !is_alt {
        out.push((WIN_SCAN_LALT, false));
    }

    out
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

    // CGKeyCode tests

    #[test]
    fn cg_letters_match_egui() {
        // Spot-check: Mac VK and egui Key produce the same Win scancode.
        assert_eq!(cgkeycode_to_scancode(0x00), Some(0x1E)); // A
        assert_eq!(cgkeycode_to_scancode(0x06), Some(0x2C)); // Z
        assert_eq!(cgkeycode_to_scancode(0x08), Some(0x2E)); // C — important for Cmd+C
        assert_eq!(cgkeycode_to_scancode(0x09), Some(0x2F)); // V — important for Cmd+V
    }

    #[test]
    fn cg_numbers() {
        assert_eq!(cgkeycode_to_scancode(0x1D), Some(0x0B)); // 0
        assert_eq!(cgkeycode_to_scancode(0x12), Some(0x02)); // 1
        assert_eq!(cgkeycode_to_scancode(0x19), Some(0x0A)); // 9
    }

    #[test]
    fn cg_special() {
        assert_eq!(cgkeycode_to_scancode(0x24), Some(0x1C)); // Return
        assert_eq!(cgkeycode_to_scancode(0x31), Some(0x39)); // Space — important for Cmd+Space
        assert_eq!(cgkeycode_to_scancode(0x35), Some(0x01)); // Escape
        assert_eq!(cgkeycode_to_scancode(0x33), Some(0x0E)); // Backspace
        assert_eq!(cgkeycode_to_scancode(0x30), Some(0x0F)); // Tab
    }

    #[test]
    fn cg_arrows_extended() {
        assert_eq!(cgkeycode_to_scancode(0x7E), Some(0xE048)); // Up
        assert_eq!(cgkeycode_to_scancode(0x7D), Some(0xE050)); // Down
        assert_eq!(cgkeycode_to_scancode(0x7B), Some(0xE04B)); // Left
        assert_eq!(cgkeycode_to_scancode(0x7C), Some(0xE04D)); // Right
    }

    #[test]
    fn cg_function_keys() {
        assert_eq!(cgkeycode_to_scancode(0x7A), Some(0x3B)); // F1
        assert_eq!(cgkeycode_to_scancode(0x6F), Some(0x58)); // F12
    }

    #[test]
    fn cg_unknown_keycode() {
        assert_eq!(cgkeycode_to_scancode(0xFFFF), None);
        assert_eq!(cgkeycode_to_scancode(0x80), None);
    }

    // FlagsChanged decoder tests

    #[test]
    fn flag_change_cmd_press() {
        let out = cg_flag_change_to_scancodes(CG_FLAG_COMMAND, 0);
        assert_eq!(out, vec![(WIN_SCAN_LCTRL, true)]);
    }

    #[test]
    fn flag_change_cmd_release() {
        let out = cg_flag_change_to_scancodes(0, CG_FLAG_COMMAND);
        assert_eq!(out, vec![(WIN_SCAN_LCTRL, false)]);
    }

    #[test]
    fn flag_change_ctrl_press() {
        let out = cg_flag_change_to_scancodes(CG_FLAG_CONTROL, 0);
        assert_eq!(out, vec![(WIN_SCAN_LCTRL, true)]);
    }

    #[test]
    fn flag_change_cmd_then_ctrl_no_double_press() {
        // Cmd already down. Adding Ctrl should NOT emit another Ctrl-down.
        let out = cg_flag_change_to_scancodes(CG_FLAG_COMMAND | CG_FLAG_CONTROL, CG_FLAG_COMMAND);
        assert!(out.is_empty(), "Cmd+Ctrl shouldn't double-press: {out:?}");
    }

    #[test]
    fn flag_change_release_one_keep_other() {
        // Both held, release Cmd → still effectively Ctrl held.
        let out = cg_flag_change_to_scancodes(CG_FLAG_CONTROL, CG_FLAG_COMMAND | CG_FLAG_CONTROL);
        assert!(out.is_empty(), "releasing Cmd while Ctrl held shouldn't release Ctrl");
    }

    #[test]
    fn flag_change_full_combo() {
        // No modifiers → Cmd+Shift+Alt all at once.
        let target = CG_FLAG_COMMAND | CG_FLAG_SHIFT | CG_FLAG_ALT;
        let out = cg_flag_change_to_scancodes(target, 0);
        assert_eq!(out.len(), 3);
        assert!(out.contains(&(WIN_SCAN_LCTRL, true)));
        assert!(out.contains(&(WIN_SCAN_LSHIFT, true)));
        assert!(out.contains(&(WIN_SCAN_LALT, true)));
    }

    #[test]
    fn flag_change_full_combo_release() {
        let prev = CG_FLAG_COMMAND | CG_FLAG_SHIFT | CG_FLAG_ALT;
        let out = cg_flag_change_to_scancodes(0, prev);
        assert_eq!(out.len(), 3);
        assert!(out.contains(&(WIN_SCAN_LCTRL, false)));
        assert!(out.contains(&(WIN_SCAN_LSHIFT, false)));
        assert!(out.contains(&(WIN_SCAN_LALT, false)));
    }

    #[test]
    fn flag_change_idempotent() {
        // Same flags as prev → no events.
        let f = CG_FLAG_COMMAND | CG_FLAG_SHIFT;
        let out = cg_flag_change_to_scancodes(f, f);
        assert!(out.is_empty());
    }

    #[test]
    fn flag_change_shift_only() {
        let out = cg_flag_change_to_scancodes(CG_FLAG_SHIFT, 0);
        assert_eq!(out, vec![(WIN_SCAN_LSHIFT, true)]);
    }

    #[test]
    fn flag_change_alt_only() {
        let out = cg_flag_change_to_scancodes(CG_FLAG_ALT, 0);
        assert_eq!(out, vec![(WIN_SCAN_LALT, true)]);
    }
}
