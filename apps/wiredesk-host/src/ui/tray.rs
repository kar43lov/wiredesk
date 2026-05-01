//! Tray icon + popup menu for the Windows host.
//!
//! Three embedded PNG variants (`assets/tray-{green,yellow,gray}.png`) map
//! to the three `SessionStatus` colors. Right-click pops up Show Settings
//! / Open Logs / Quit.

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;

use native_windows_gui as nwg;

use crate::session_thread::SessionStatus;
use crate::ui::format::{self, StatusColor};

const ICON_GREEN_BYTES: &[u8] = include_bytes!("../../../../assets/tray-green.png");
const ICON_YELLOW_BYTES: &[u8] = include_bytes!("../../../../assets/tray-yellow.png");
const ICON_GRAY_BYTES: &[u8] = include_bytes!("../../../../assets/tray-gray.png");

#[derive(Default)]
pub struct TrayUi {
    pub window: nwg::MessageWindow,
    pub tray: nwg::TrayNotification,
    pub menu: nwg::Menu,
    pub menu_show_settings: nwg::MenuItem,
    pub menu_open_logs: nwg::MenuItem,
    pub menu_separator: nwg::MenuSeparator,
    pub menu_quit: nwg::MenuItem,

    pub icon_green: nwg::Bitmap,
    pub icon_yellow: nwg::Bitmap,
    pub icon_gray: nwg::Bitmap,

    log_dir: PathBuf,
}

impl TrayUi {
    pub fn build(log_dir: PathBuf) -> Result<Rc<RefCell<Self>>, nwg::NwgError> {
        let me = Rc::new(RefCell::new(Self {
            log_dir,
            ..Default::default()
        }));
        {
            let mut s = me.borrow_mut();

            nwg::MessageWindow::builder().build(&mut s.window)?;

            // Build all three icons up front so update_status() is a quick
            // pointer swap. The PNGs are tiny (≤ 500 B each).
            nwg::Bitmap::builder()
                .source_bin(Some(ICON_GREEN_BYTES))
                .strict(true)
                .build(&mut s.icon_green)?;
            nwg::Bitmap::builder()
                .source_bin(Some(ICON_YELLOW_BYTES))
                .strict(true)
                .build(&mut s.icon_yellow)?;
            nwg::Bitmap::builder()
                .source_bin(Some(ICON_GRAY_BYTES))
                .strict(true)
                .build(&mut s.icon_gray)?;

            // Initial state: gray (disconnected).
            // TrayNotification needs an Icon, but Bitmap suffices for the
            // tip image; we read the Icon from the bitmap via reinterpret.
            // Easier: use nwg::Icon directly built from the PNG bytes.
            let mut icon_gray = nwg::Icon::default();
            nwg::Icon::builder()
                .source_bin(Some(ICON_GRAY_BYTES))
                .strict(true)
                .build(&mut icon_gray)?;

            nwg::TrayNotification::builder()
                .parent(&s.window)
                .icon(Some(&icon_gray))
                .tip(Some("WireDesk Host — disconnected"))
                .build(&mut s.tray)?;

            nwg::Menu::builder()
                .popup(true)
                .parent(&s.window)
                .build(&mut s.menu)?;

            nwg::MenuItem::builder()
                .text("Show Settings…")
                .parent(&s.menu)
                .build(&mut s.menu_show_settings)?;
            nwg::MenuItem::builder()
                .text("Open Logs")
                .parent(&s.menu)
                .build(&mut s.menu_open_logs)?;
            nwg::MenuSeparator::builder()
                .parent(&s.menu)
                .build(&mut s.menu_separator)?;
            nwg::MenuItem::builder()
                .text("Quit")
                .parent(&s.menu)
                .build(&mut s.menu_quit)?;
        }
        Ok(me)
    }

    pub fn update_status(&mut self, status: &SessionStatus) -> Result<(), nwg::NwgError> {
        let bytes = match format::status_color(status) {
            StatusColor::Green => ICON_GREEN_BYTES,
            StatusColor::Yellow => ICON_YELLOW_BYTES,
            StatusColor::Gray => ICON_GRAY_BYTES,
        };
        let mut icon = nwg::Icon::default();
        nwg::Icon::builder()
            .source_bin(Some(bytes))
            .strict(true)
            .build(&mut icon)?;
        self.tray.set_icon(&icon);
        self.tray.set_tip(&format!("WireDesk Host — {}", status.label()));
        Ok(())
    }

    /// Show the popup menu at the current cursor position. Hook this up to
    /// the `OnContextMenu` event for the tray.
    pub fn show_popup(&self) {
        let (x, y) = nwg::GlobalCursor::position();
        self.menu.popup(x, y);
    }

    /// Open the host log folder in Explorer. No-op (logs warning) if the
    /// directory cannot be opened.
    pub fn open_logs(&self) {
        if let Err(e) = Command::new("explorer").arg(&self.log_dir).spawn() {
            log::warn!("failed to spawn explorer for logs: {e}");
        }
    }
}
