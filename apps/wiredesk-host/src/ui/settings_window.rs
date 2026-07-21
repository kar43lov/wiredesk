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
use crate::ui::icons::{self, ICON_YELLOW_BYTES};
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
    pub transport_label: nwg::Label,
    pub transport_combo: nwg::ComboBox<String>,
    pub port_label: nwg::Label,
    pub port_combo: nwg::ComboBox<String>,
    pub detect_btn: nwg::Button,
    pub port_manual_label: nwg::Label,
    pub port_input: nwg::TextInput,
    /// Bare COM names index-aligned with `port_combo`'s labels. The labels
    /// carry a chip hint ("COM7 — FT232H"), so a dropdown selection maps back
    /// to the value written into config through this side table.
    pub port_choice_coms: RefCell<Vec<String>>,
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

    // --- Clipboard group (Task 8) ---
    // Single checkbox today — "Receive files" — but kept in its own group so
    // future toggles (send_files, send_images, receive_images) can join
    // without a layout shift. Mirrors the Mac client Clipboard group.
    pub clipboard_title: nwg::Label,
    pub clipboard_frame: nwg::Frame,
    pub clipboard_layout: nwg::GridLayout,
    pub receive_files_check: nwg::CheckBox,

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
    pub quit_btn: nwg::Button,
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
                // MAIN_WINDOW (not WINDOW) so the frame carries WS_THICKFRAME +
                // maximize/minimize boxes — the window is resizable. The plain
                // WINDOW flag gave only WS_CAPTION|WS_SYSMENU (fixed border), so
                // on a monitor with a different DPI/scale the right column of
                // the layout got clipped with no way to widen the window. nwg's
                // GridLayout listens on WM_SIZE and reflows on resize/maximize,
                // and the outer grid's min_size keeps it from shrinking below
                // legibility. `center(true)` places it on the active monitor
                // (overriding the fixed position) so it can't spawn off-screen.
                nwg::Window::builder()
                    .size((520, 600))
                    .position((300, 300))
                    .center(true)
                    .title("WireDesk Host Settings")
                    .icon(icon_ref)
                    .flags(nwg::WindowFlags::MAIN_WINDOW)
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

            // Transport selector — picks between USB-Serial null-modem
            // and Bluetooth LE GATT. Save+Restart applies the change.
            // Bluetooth-specific tuning (peer name, MTU, service UUID,
            // reconnect attempts) lives in config.toml under
            // [bluetooth] — they rarely need editing day-to-day.
            nwg::Label::builder()
                .text("Transport:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.connection_frame)
                .build(&mut s.transport_label)?;
            let transport_options =
                vec!["USB-Serial".to_string(), "Bluetooth LE".to_string()];
            let transport_idx = if config.transport == "bluetooth" {
                Some(1usize)
            } else {
                Some(0usize)
            };
            nwg::ComboBox::builder()
                .collection(transport_options)
                .selected_index(transport_idx)
                .parent(&s.connection_frame)
                .build(&mut s.transport_combo)?;

            nwg::Label::builder()
                .text("Serial port:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.connection_frame)
                .build(&mut s.port_label)?;
            // Labeled dropdown of detected serial ports (e.g. "COM7 — FT232H").
            // Populated on every window open and on Detect via
            // `set_port_choices`; selecting an entry copies its bare COM name
            // into `port_input` (the canonical value read at Save). Starts
            // empty — `show()` fills it before the window becomes visible.
            nwg::ComboBox::builder()
                .collection(Vec::<String>::new())
                .parent(&s.connection_frame)
                .build(&mut s.port_combo)?;
            // Auto-detect button — enumerates serial ports, repopulates the
            // dropdown, and auto-selects a WireDesk adapter (CH340 VID 0x1A86
            // or FTDI VID 0x0403). Handler in main.rs OnButtonClick. Alt+D.
            nwg::Button::builder()
                .text("&Detect")
                .parent(&s.connection_frame)
                .build(&mut s.detect_btn)?;
            // Manual override — free-text COM port for ports the dropdown
            // didn't enumerate (mirrors the Mac client's combo + free-text).
            nwg::Label::builder()
                .text("or type:")
                .h_align(nwg::HTextAlign::Right)
                .parent(&s.connection_frame)
                .build(&mut s.port_manual_label)?;
            nwg::TextInput::builder()
                .text(&config.port)
                .parent(&s.connection_frame)
                .build(&mut s.port_input)?;

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

            // ---- Clipboard group (Task 8) ----
            // "Receive files" mirror of the Mac client's checkbox. When off,
            // incoming FORMAT_FILE offers are declined (ClipboardSync's
            // `on_offer` returns ClipDecline; reassembly stays idle). Save +
            // Restart respawns the host process — the new boot reads the
            // updated TOML and stores `false` into the session-thread Arc.
            nwg::Label::builder()
                .text("Clipboard")
                .parent(&s.window)
                .build(&mut s.clipboard_title)?;
            nwg::Frame::builder()
                .parent(&s.window)
                .flags(nwg::FrameFlags::VISIBLE | nwg::FrameFlags::BORDER)
                .build(&mut s.clipboard_frame)?;

            let receive_files_initial = if config.receive_files {
                nwg::CheckBoxState::Checked
            } else {
                nwg::CheckBoxState::Unchecked
            };
            nwg::CheckBox::builder()
                .text("Receive files (Mac → Host)")
                .check_state(receive_files_initial)
                .parent(&s.clipboard_frame)
                .build(&mut s.receive_files_check)?;

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
                .text("&Quit")
                .parent(&s.bar_frame)
                .build(&mut s.quit_btn)?;
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
                // Row 0: [Transport label] [combo spans cols 1..2]
                .child(0, 0, &s.transport_label)
                .child_item(nwg::GridLayoutItem::new(&s.transport_combo, 1, 0, 2, 1))
                // Row 1: [Serial port label] [port_combo] [Detect]
                .child(0, 1, &s.port_label)
                .child(1, 1, &s.port_combo)
                .child(2, 1, &s.detect_btn)
                // Row 2: [or type label] [port_input spans cols 1..2]
                .child(0, 2, &s.port_manual_label)
                .child_item(nwg::GridLayoutItem::new(&s.port_input, 1, 2, 2, 1))
                // Row 3: [Baud label] [baud_input spans cols 1..2]
                .child(0, 3, &s.baud_label)
                .child_item(nwg::GridLayoutItem::new(&s.baud_input, 1, 3, 2, 1))
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
                .parent(&s.clipboard_frame)
                .max_column(Some(3))
                .spacing(4)
                .margin([6, 6, 6, 6])
                .child_item(nwg::GridLayoutItem::new(&s.receive_files_check, 0, 0, 3, 1))
                .build(&s.clipboard_layout)?;

            nwg::GridLayout::builder()
                .parent(&s.system_frame)
                .max_column(Some(3))
                .spacing(4)
                .margin([6, 6, 6, 6])
                .child_item(nwg::GridLayoutItem::new(&s.autostart_check, 0, 0, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.copy_mac_btn, 0, 1, 3, 1))
                .build(&s.system_layout)?;

            // Button-bar internal grid: 4 cols × 1 row. Col 0 holds Quit
            // on the left (destructive, separated from Save group), col 1
            // is a spacer so Restart/Save are right-aligned.
            nwg::GridLayout::builder()
                .parent(&s.bar_frame)
                .max_column(Some(4))
                .spacing(4)
                .margin([0, 0, 0, 0])
                .child(0, 0, &s.quit_btn)
                .child(2, 0, &s.restart_btn)
                .child(3, 0, &s.save_btn)
                .build(&s.bar_layout)?;

            // ---- Outer grid: status row + 3 groups (title + frame) +
            // button-bar + message. Each group is two rows: 1-row title,
            // then a multi-row frame for nested controls. The Connection
            // frame is now 4 rows tall (Transport / Port+Detect / manual /
            // Baud), so all subsequent rows shift +1 vs the pre-dropdown
            // layout.
            nwg::GridLayout::builder()
                .parent(&s.window)
                .min_size([440, 560])
                .max_column(Some(3))
                .spacing(4)
                .margin([6, 6, 6, 6])
                // Row 0: status icon + label
                .child(0, 0, &s.status_icon)
                .child_item(nwg::GridLayoutItem::new(&s.status_label, 1, 0, 2, 1))
                // Row 1: Connection title; rows 2-5: Connection frame
                // (Transport / Port+Detect / manual / Baud — 4 rows)
                .child_item(nwg::GridLayoutItem::new(&s.connection_title, 0, 1, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.connection_frame, 0, 2, 3, 4))
                // Row 6: Display title; rows 7-8: Display frame
                .child_item(nwg::GridLayoutItem::new(&s.display_title, 0, 6, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.display_frame, 0, 7, 3, 2))
                // Row 9: Clipboard title; rows 10-11: Clipboard frame
                // (single checkbox today — Task 8). Frame height matches
                // Display so the visual grid stays even.
                .child_item(nwg::GridLayoutItem::new(&s.clipboard_title, 0, 9, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.clipboard_frame, 0, 10, 3, 2))
                // Row 12: System title; rows 13-14: System frame
                .child_item(nwg::GridLayoutItem::new(&s.system_title, 0, 12, 3, 1))
                .child_item(nwg::GridLayoutItem::new(&s.system_frame, 0, 13, 3, 2))
                // Row 15: button-bar (right-aligned via internal grid)
                .child_item(nwg::GridLayoutItem::new(&s.bar_frame, 0, 15, 3, 1))
                // Row 16: message label
                .child_item(nwg::GridLayoutItem::new(&s.message_label, 0, 16, 3, 1))
                .build(&s.layout)?;

            // Hidden by default — caller decides when to reveal.
            s.window.set_visible(false);
        }
        Ok(me)
    }

    pub fn show(&self) {
        // Refresh the port dropdown before revealing the window so it always
        // reflects what's plugged in right now (adapters come and go between
        // opens). Detect re-runs this on demand with target auto-select.
        self.refresh_port_choices();
        self.window.set_visible(true);
        self.window.set_focus();
    }

    /// Re-enumerate serial ports and repopulate the dropdown, pre-selecting
    /// the entry whose COM matches the current `port_input` value (the
    /// configured port). Enumeration failure is logged, not surfaced — an
    /// empty dropdown with the manual field still works.
    pub fn refresh_port_choices(&self) {
        match format::enumerate_ports_now() {
            Ok(ports) => {
                let current = self.port_input.text();
                let select = ports.iter().position(|p| p.port_name == current);
                self.set_port_choices(&ports, select);
            }
            Err(e) => log::warn!("serial port enumeration for dropdown failed: {e}"),
        }
    }

    /// Fill the port dropdown with `ports`' labels and select `select`,
    /// stashing the bare COM names index-aligned so a later selection maps
    /// back to a config value.
    pub fn set_port_choices(&self, ports: &[format::DetectedPort], select: Option<usize>) {
        let labels: Vec<String> = ports.iter().map(|p| p.label.clone()).collect();
        self.port_combo.set_collection(labels);
        self.port_combo.set_selection(select);
        *self.port_choice_coms.borrow_mut() =
            ports.iter().map(|p| p.port_name.clone()).collect();
    }

    /// Bare COM name of the currently selected dropdown entry, if any. Used by
    /// the OnComboBoxChanged handler to copy the choice into `port_input`.
    pub fn selected_port_com(&self) -> Option<String> {
        self.port_combo
            .selection()
            .and_then(|i| self.port_choice_coms.borrow().get(i).cloned())
    }

    pub fn hide(&self) {
        self.window.set_visible(false);
    }

    /// Read values out of the form into a typed `HostConfig`. Returns
    /// `Err(message)` on the first validation failure so the caller can
    /// surface it via `set_message`. The input strings are trimmed.
    /// Build a `HostConfig` from the form fields, taking unexposed fields
    /// (host_name, bluetooth advanced tuning, transport_fallback) from
    /// `base`. The transport ComboBox drives `cfg.transport`; serial
    /// port + baud + screen size + autostart come from their own inputs.
    /// Bluetooth peer_name / mtu / service_uuid stay in config.toml — the
    /// UI exposes only the transport selector (the day-to-day toggle).
    pub fn read_form(&self, base: &HostConfig) -> Result<HostConfig, String> {
        let port = format::validate_port(&self.port_input.text())?.to_string();
        let baud = format::validate_baud(&self.baud_input.text())?;
        let width = format::validate_dimension(&self.width_input.text())?;
        let height = format::validate_dimension(&self.height_input.text())?;
        let run_on_startup = self.autostart_check.check_state() == nwg::CheckBoxState::Checked;
        // Task 8: file clipboard receive toggle. Checkbox lives in the
        // Clipboard group; mirrors the Mac client's `receive_files` toggle.
        // Save-and-Restart respawns the host process so the value takes
        // effect on the next session (no live re-arm — matches the existing
        // baud/port/transport pattern).
        let receive_files =
            self.receive_files_check.check_state() == nwg::CheckBoxState::Checked;
        let transport = match self.transport_combo.selection() {
            Some(1) => "bluetooth".to_string(),
            _ => "serial".to_string(),
        };
        Ok(HostConfig {
            port,
            baud,
            width,
            height,
            run_on_startup,
            receive_files,
            transport,
            // Preserve fields not edited in this form.
            host_name: base.host_name.clone(),
            transport_fallback: base.transport_fallback.clone(),
            bluetooth: base.bluetooth.clone(),
        })
    }

    /// Update the status indicator (icon + label) to reflect a new
    /// `SessionStatus`. Rebuilds the bitmap in-place — the `ImageFrame`
    /// stays bound to the same field, so the layout doesn't shift. Takes
    /// `&mut self` because `nwg::Bitmap::builder` writes through a mut
    /// reference into our owned field.
    pub fn set_status(&mut self, status: &SessionStatus) {
        if let Err(e) = icons::build_status_bitmap(status, &mut self.status_icon_bitmap) {
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
