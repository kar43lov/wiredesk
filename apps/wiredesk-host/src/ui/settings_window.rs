//! Windows-only settings window built on `native-windows-gui`. Compiles
//! only under `cfg(windows)` (the `mod settings_window` line in `ui/mod.rs`
//! is gated). Pure validation / formatting logic lives in `ui::format` so
//! it can be unit-tested cross-platform.
//!
//! The struct is constructed via the `nwg::*::builder()` API (instead of
//! the `NwgUi` derive macro) to keep the wiring explicit and the cross-
//! compile guard small. Event wiring is handled by the caller (`main.rs`
//! Windows path, Task 7) so this module only owns the controls.

use std::cell::RefCell;
use std::rc::Rc;

use native_windows_gui as nwg;

use crate::config::HostConfig;
use crate::session_thread::SessionStatus;
use crate::ui::format::StatusColor;
use crate::ui::icons::{ICON_GRAY_BYTES, ICON_GREEN_BYTES, ICON_YELLOW_BYTES};
use crate::ui::{autostart, format};

// Window icon — title-bar / Alt-Tab. Loaded at runtime from the multi-size
// .ico asset. This path runs without a windres / RC.exe toolchain (we'd
// normally embed it as a PE resource for taskbar+Alt+Tab on first paint
// and a clean Win-explorer .exe icon, but that requires a Windows-side
// build environment — see plan task 2 fallback note).
const APP_ICON_BYTES: &[u8] = include_bytes!("../../../../assets/app-icon.ico");

/// All controls owned by the settings window. Stored together so the
/// caller can wire up event handlers via `Rc<RefCell<SettingsWindow>>`.
///
/// Layout: status-row at the top, then three group-boxes (Connection /
/// Display / System), then a button-bar, then a message label. nwg's
/// `Frame` is only a container without a header label, so each group is
/// rendered as `Label` (title, strong-styled) + `Frame` (bordered box
/// holding nested controls via its own `GridLayout`). See plan task 4
/// — fallback chosen because nwg::Frame::builder has no `text()`.
#[derive(Default)]
pub struct SettingsWindow {
    pub window: nwg::Window,
    pub window_icon: nwg::Icon,
    pub layout: nwg::GridLayout,

    pub status_icon: nwg::ImageFrame,
    pub status_icon_bitmap: nwg::Bitmap,
    pub status_label: nwg::Label,

    // --- Connection group ---
    pub connection_title: nwg::Label,
    pub connection_frame: nwg::Frame,
    pub connection_layout: nwg::GridLayout,
    pub port_label: nwg::Label,
    pub port_input: nwg::TextInput,
    pub detect_btn: nwg::Button,
    pub baud_label: nwg::Label,
    pub baud_input: nwg::TextInput,

    // --- Display group ---
    pub display_title: nwg::Label,
    pub display_frame: nwg::Frame,
    pub display_layout: nwg::GridLayout,
    pub width_label: nwg::Label,
    pub width_input: nwg::TextInput,
    pub height_label: nwg::Label,
    pub height_input: nwg::TextInput,

    // --- System group ---
    pub system_title: nwg::Label,
    pub system_frame: nwg::Frame,
    pub system_layout: nwg::GridLayout,
    pub autostart_check: nwg::CheckBox,
    pub copy_mac_btn: nwg::Button,

    // --- Bottom button-bar (outside groups) ---
    // The bar holds two right-aligned buttons. `save_btn` is the primary
    // action; `restart_btn` saves AND respawns the host process so settings
    // changes take effect without the user manually quitting from tray.
    // No `set_default_button` — Enter inside a TextEdit shouldn't trigger
    // Save (would surprise users typing baud / dimensions). Hide button
    // removed — close-X provides the same affordance (UX-audit N3).
    pub bar_frame: nwg::Frame,
    pub bar_layout: nwg::GridLayout,
    pub restart_btn: nwg::Button,
    pub save_btn: nwg::Button,

    pub message_label: nwg::Label,
}

impl SettingsWindow {
    /// Build the settings window with the given config as initial values.
    /// Window starts hidden — `show()` reveals it. `nwg::init()` and
    /// `nwg::Font::set_global_default()` must be called by the caller
    /// once at startup before `build()`.
    pub fn build(config: &HostConfig) -> Result<Rc<RefCell<Self>>, nwg::NwgError> {
        let me = Rc::new(RefCell::new(Self::default()));
        {
            let mut s = me.borrow_mut();

            // Build window-icon and window in a single expression to give
            // the borrow checker a clear field split: through a single
            // `RefMut<SettingsWindow>` it can't see that `s.window_icon`
            // and `s.window` are disjoint fields, so we destructure once.
            {
                let SettingsWindow {
                    ref mut window,
                    ref mut window_icon,
                    ..
                } = *s;
                // `include_bytes!` already guarantees the asset is present
                // and non-empty at compile time. A failure here means the
                // bundled `.ico` is malformed — that's a build-time bug we
                // want to catch loudly the first time it runs, not silently
                // ship a windowless title-bar to users.
                nwg::Icon::builder()
                    .source_bin(Some(APP_ICON_BYTES))
                    .strict(true)
                    .build(window_icon)
                    .expect("malformed bundled app-icon.ico — rebuild assets");
                let icon_ref = Some(&*window_icon);
                nwg::Window::builder()
                    .size((460, 460))
                    .position((300, 300))
                    .title("WireDesk Host Settings")
                    .icon(icon_ref)
                    .flags(nwg::WindowFlags::WINDOW)
                    .build(window)?;
            }

            // Initial status indicator: yellow (Waiting). The bitmap is
            // rebuilt in-place every set_status() so we keep ownership of
            // the field and the ImageFrame stays bound to the same struct.
            // Destructure the borrow so the borrow checker sees that
            // `status_icon`, `status_icon_bitmap` and `window` are disjoint.
            {
                let SettingsWindow {
                    ref window,
                    ref mut status_icon,
                    ref mut status_icon_bitmap,
                    ..
                } = *s;
                nwg::Bitmap::builder()
                    .source_bin(Some(ICON_YELLOW_BYTES))
                    .strict(true)
                    .build(status_icon_bitmap)?;
                nwg::ImageFrame::builder()
                    .bitmap(Some(&*status_icon_bitmap))
                    .parent(window)
                    .build(status_icon)?;
            }

            nwg::Label::builder()
                .text(&SessionStatus::Waiting.label())
                .parent(&s.window)
                .build(&mut s.status_label)?;

            // ---- Connection group ----
            nwg::Label::builder()
                .text("Connection")
                .parent(&s.window)
                .build(&mut s.connection_title)?;
            nwg::Frame::builder()
                .parent(&s.window)
                .flags(nwg::FrameFlags::VISIBLE | nwg::FrameFlags::BORDER)
                .build(&mut s.connection_frame)?;

            nwg::Label::builder()
                .text("Serial port:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.connection_frame)
                .build(&mut s.port_label)?;
            nwg::TextInput::builder()
                .text(&config.port)
                .parent(&s.connection_frame)
                .build(&mut s.port_input)?;
            // Auto-detect button — fills port_input with the discovered COM
            // port if exactly one CH340 (VID 0x1A86) is plugged in. Handler
            // wired in main.rs OnButtonClick. Alt+D accelerator.
            nwg::Button::builder()
                .text("&Detect")
                .parent(&s.connection_frame)
                .build(&mut s.detect_btn)?;

            nwg::Label::builder()
                .text("Baud:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.connection_frame)
                .build(&mut s.baud_label)?;
            nwg::TextInput::builder()
                .text(&config.baud.to_string())
                .parent(&s.connection_frame)
                .build(&mut s.baud_input)?;

            // ---- Display group ----
            nwg::Label::builder()
                .text("Display")
                .parent(&s.window)
                .build(&mut s.display_title)?;
            nwg::Frame::builder()
                .parent(&s.window)
                .flags(nwg::FrameFlags::VISIBLE | nwg::FrameFlags::BORDER)
                .build(&mut s.display_frame)?;

            nwg::Label::builder()
                .text("Screen W:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.display_frame)
                .build(&mut s.width_label)?;
            nwg::TextInput::builder()
                .text(&config.width.to_string())
                .parent(&s.display_frame)
                .build(&mut s.width_input)?;

            nwg::Label::builder()
                .text("Screen H:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.display_frame)
                .build(&mut s.height_label)?;
            nwg::TextInput::builder()
                .text(&config.height.to_string())
                .parent(&s.display_frame)
                .build(&mut s.height_input)?;

            // ---- System group ----
            nwg::Label::builder()
                .text("System")
                .parent(&s.window)
                .build(&mut s.system_title)?;
            nwg::Frame::builder()
                .parent(&s.window)
                .flags(nwg::FrameFlags::VISIBLE | nwg::FrameFlags::BORDER)
                .build(&mut s.system_frame)?;

            // Reflect actual registry state, not just config.run_on_startup —
            // user might have toggled the run-key elsewhere between sessions.
            let want_startup = config.run_on_startup || autostart::is_enabled();
            let initial_check = if want_startup {
                nwg::CheckBoxState::Checked
            } else {
                nwg::CheckBoxState::Unchecked
            };
            nwg::CheckBox::builder()
                .text("Run on startup")
                .check_state(initial_check)
                .parent(&s.system_frame)
                .build(&mut s.autostart_check)?;

            nwg::Button::builder()
                .text("Copy Mac launch command")
                .parent(&s.system_frame)
                .build(&mut s.copy_mac_btn)?;

            // ---- Bottom button-bar (outside groups) ----
            // Horizontal frame with right-aligned primary action. Captions
            // use `&` accelerators (Alt+R / Alt+S — Win11 standard).
            // "Re&start" was chosen over "Save && &Restart" — the double-`&`
            // pattern produces a literal ampersand followed by an accelerator,
            // but visually reads like AND ("Save AND Restart") and confused
            // testers. The shorter "Re&start" makes the action self-evident
            // (it always saves before restart), and Alt+R still hits.
            nwg::Frame::builder()
                .parent(&s.window)
                .flags(nwg::FrameFlags::VISIBLE)
                .build(&mut s.bar_frame)?;
            nwg::Button::builder()
                .text("Re&start")
                .parent(&s.bar_frame)
                .build(&mut s.restart_btn)?;
            nwg::Button::builder()
                .text("&Save")
                .parent(&s.bar_frame)
                .build(&mut s.save_btn)?;

            nwg::Label::builder()
                .text("")
                .parent(&s.window)
                .build(&mut s.message_label)?;

            // ---- Nested grids inside each frame ----
            nwg::GridLayout::builder()
                .parent(&s.connection_frame)
                .max_column(Some(3))
                .spacing(4)
                .margin([6, 6, 6, 6])
                // Row 0: [label] [port_input] [Detect]
                .child(0, 0, &s.port_label)
                .child(1, 0, &s.port_input)
                .child(2, 0, &s.detect_btn)
                // Row 1: [label] [baud_input spans cols 1..2]
                .child(0, 1, &s.baud_label)
                .child_item(nwg::GridLayoutItem::new(&s.baud_input, 1, 1, 2, 1))
                .build(&s.connection_layout)?;

            nwg::GridLayout::builder()
                .parent(&s.display_frame)
                .max_column(Some(3))
                .spacing(4)
                .margin([6, 6, 6, 6])
                .child(0, 0, &s.width_label)
                .child_item(nwg::GridLayoutItem::new(&s.width_input, 1, 0, 2, 1))
                .child(0, 1, &s.height_label)
                .child_item(nwg::GridLayoutItem::new(&s.height_input, 1, 1, 2, 1))
                .build(&s.display_layout)?;

            nwg::GridLayout::builder()
                .parent(&s.system_frame)
                .max_column(Some(3))
                .spacing(4)
                .margin([6, 6, 6, 6])
                .child_item(nwg::GridLayoutItem::new(&s.autostart_check, 0, 0, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.copy_mac_btn, 0, 1, 3, 1))
                .build(&s.system_layout)?;

            // Button-bar internal grid: 3 cols × 1 row. Col 0 is a spacer
            // (no child) so the two buttons get pushed to the right; cols 1
            // and 2 hold the action buttons in [Save&Restart][Save] order.
            nwg::GridLayout::builder()
                .parent(&s.bar_frame)
                .max_column(Some(3))
                .spacing(4)
                .margin([0, 0, 0, 0])
                .child(1, 0, &s.restart_btn)
                .child(2, 0, &s.save_btn)
                .build(&s.bar_layout)?;

            // ---- Outer grid: status row + 3 groups (title + frame) +
            // button-bar + message. Each group is two rows: 1-row title,
            // then a multi-row frame for nested controls. The frame row
            // height is widened (rowspan) so the bordered box has air.
            // Window-level grid uses 9 rows × 3 cols.
            nwg::GridLayout::builder()
                .parent(&s.window)
                .min_size([440, 440])
                .max_column(Some(3))
                .spacing(4)
                .margin([6, 6, 6, 6])
                // Row 0: status icon + label
                .child(0, 0, &s.status_icon)
                .child_item(nwg::GridLayoutItem::new(&s.status_label, 1, 0, 2, 1))
                // Row 1: Connection title; rows 2-3: Connection frame
                .child_item(nwg::GridLayoutItem::new(&s.connection_title, 0, 1, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.connection_frame, 0, 2, 3, 2))
                // Row 4: Display title; rows 5-6: Display frame
                .child_item(nwg::GridLayoutItem::new(&s.display_title, 0, 4, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.display_frame, 0, 5, 3, 2))
                // Row 7: System title; rows 8-9: System frame
                .child_item(nwg::GridLayoutItem::new(&s.system_title, 0, 7, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.system_frame, 0, 8, 3, 2))
                // Row 10: button-bar (right-aligned via internal grid)
                .child_item(nwg::GridLayoutItem::new(&s.bar_frame, 0, 10, 3, 1))
                // Row 11: message label
                .child_item(nwg::GridLayoutItem::new(&s.message_label, 0, 11, 3, 1))
                .build(&s.layout)?;

            // Hidden by default — caller decides when to reveal.
            s.window.set_visible(false);
        }
        Ok(me)
    }

    pub fn show(&self) {
        self.window.set_visible(true);
        self.window.set_focus();
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Read values out of the form into a typed `HostConfig`. Returns
    /// `Err(message)` on the first validation failure so the caller can
    /// surface it via `set_message`. The input strings are trimmed.
    pub fn read_form(&self) -> Result<HostConfig, String> {
        let port = format::validate_port(&self.port_input.text())?.to_string();
        let baud = format::validate_baud(&self.baud_input.text())?;
        let width = format::validate_dimension(&self.width_input.text())?;
        let height = format::validate_dimension(&self.height_input.text())?;
        let host_name = "wiredesk-host".to_string(); // not exposed in this view yet
        let run_on_startup = self.autostart_check.check_state() == nwg::CheckBoxState::Checked;
        Ok(HostConfig {
            port,
            baud,
            width,
            height,
            host_name,
            run_on_startup,
        })
    }

    /// Update the status indicator (icon + label) to reflect a new
    /// `SessionStatus`. Rebuilds the bitmap in-place — the `ImageFrame`
    /// stays bound to the same field, so the layout doesn't shift. Takes
    /// `&mut self` because `nwg::Bitmap::builder` writes through a mut
    /// reference into our owned field.
    pub fn set_status(&mut self, status: &SessionStatus) {
        let bytes = match format::status_color(status) {
            StatusColor::Green => ICON_GREEN_BYTES,
            StatusColor::Yellow => ICON_YELLOW_BYTES,
            StatusColor::Gray => ICON_GRAY_BYTES,
        };
        if let Err(e) = nwg::Bitmap::builder()
            .source_bin(Some(bytes))
            .strict(true)
            .build(&mut self.status_icon_bitmap)
        {
            log::warn!("status icon bitmap rebuild failed: {e}");
        } else {
            self.status_icon.set_bitmap(Some(&self.status_icon_bitmap));
        }
        self.status_label.set_text(&status.label());
    }

    pub fn set_message(&self, msg: &str) {
        self.message_label.set_text(msg);
    }
}
