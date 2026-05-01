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
use crate::ui::{autostart, format};

// Window icon — title-bar / Alt-Tab. Loaded at runtime from the multi-size
// .ico asset. This path runs without a windres / RC.exe toolchain (we'd
// normally embed it as a PE resource for taskbar+Alt+Tab on first paint
// and a clean Win-explorer .exe icon, but that requires a Windows-side
// build environment — see plan task 2 fallback note).
const APP_ICON_BYTES: &[u8] = include_bytes!("../../../../assets/app-icon.ico");

/// All controls owned by the settings window. Stored together so the
/// caller can wire up event handlers via `Rc<RefCell<SettingsWindow>>`.
#[derive(Default)]
pub struct SettingsWindow {
    pub window: nwg::Window,
    pub window_icon: nwg::Icon,
    pub layout: nwg::GridLayout,

    pub status_label: nwg::Label,

    pub port_label: nwg::Label,
    pub port_input: nwg::TextInput,

    pub baud_label: nwg::Label,
    pub baud_input: nwg::TextInput,

    pub width_label: nwg::Label,
    pub width_input: nwg::TextInput,

    pub height_label: nwg::Label,
    pub height_input: nwg::TextInput,

    pub autostart_check: nwg::CheckBox,

    pub copy_mac_btn: nwg::Button,
    pub save_btn: nwg::Button,
    pub hide_btn: nwg::Button,

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
                let icon_ok = nwg::Icon::builder()
                    .source_bin(Some(APP_ICON_BYTES))
                    .strict(false)
                    .build(window_icon)
                    .is_ok();
                if !icon_ok {
                    log::warn!("failed to load app icon (resource missing or malformed)");
                }
                let icon_ref = if icon_ok { Some(&*window_icon) } else { None };
                nwg::Window::builder()
                    .size((420, 340))
                    .position((300, 300))
                    .title("WireDesk Host Settings")
                    .icon(icon_ref)
                    .flags(nwg::WindowFlags::WINDOW)
                    .build(window)?;
            }

            nwg::Label::builder()
                .text(&SessionStatus::Waiting.label())
                .parent(&s.window)
                .build(&mut s.status_label)?;

            nwg::Label::builder()
                .text("Serial port:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.window)
                .build(&mut s.port_label)?;
            nwg::TextInput::builder()
                .text(&config.port)
                .parent(&s.window)
                .build(&mut s.port_input)?;

            nwg::Label::builder()
                .text("Baud:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.window)
                .build(&mut s.baud_label)?;
            nwg::TextInput::builder()
                .text(&config.baud.to_string())
                .parent(&s.window)
                .build(&mut s.baud_input)?;

            nwg::Label::builder()
                .text("Screen W:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.window)
                .build(&mut s.width_label)?;
            nwg::TextInput::builder()
                .text(&config.width.to_string())
                .parent(&s.window)
                .build(&mut s.width_input)?;

            nwg::Label::builder()
                .text("Screen H:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.window)
                .build(&mut s.height_label)?;
            nwg::TextInput::builder()
                .text(&config.height.to_string())
                .parent(&s.window)
                .build(&mut s.height_input)?;

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
                .parent(&s.window)
                .build(&mut s.autostart_check)?;

            nwg::Button::builder()
                .text("Copy Mac launch command")
                .parent(&s.window)
                .build(&mut s.copy_mac_btn)?;
            nwg::Button::builder()
                .text("Save")
                .parent(&s.window)
                .build(&mut s.save_btn)?;
            nwg::Button::builder()
                .text("Hide")
                .parent(&s.window)
                .build(&mut s.hide_btn)?;

            nwg::Label::builder()
                .text("")
                .parent(&s.window)
                .build(&mut s.message_label)?;

            // Two-column grid: labels on the left, inputs on the right.
            nwg::GridLayout::builder()
                .parent(&s.window)
                .min_size([400, 320])
                .max_column(Some(3))
                .child_item(nwg::GridLayoutItem::new(&s.status_label, 0, 0, 3, 1))
                .child(0, 1, &s.port_label)
                .child_item(nwg::GridLayoutItem::new(&s.port_input, 1, 1, 2, 1))
                .child(0, 2, &s.baud_label)
                .child_item(nwg::GridLayoutItem::new(&s.baud_input, 1, 2, 2, 1))
                .child(0, 3, &s.width_label)
                .child_item(nwg::GridLayoutItem::new(&s.width_input, 1, 3, 2, 1))
                .child(0, 4, &s.height_label)
                .child_item(nwg::GridLayoutItem::new(&s.height_input, 1, 4, 2, 1))
                .child_item(nwg::GridLayoutItem::new(&s.autostart_check, 0, 5, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.copy_mac_btn, 0, 6, 3, 1))
                .child(0, 7, &s.save_btn)
                .child(1, 7, &s.hide_btn)
                .child_item(nwg::GridLayoutItem::new(&s.message_label, 0, 8, 3, 1))
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

    pub fn set_status(&self, status: &SessionStatus) {
        self.status_label.set_text(&status.label());
    }

    pub fn set_message(&self, msg: &str) {
        self.message_label.set_text(msg);
    }
}
