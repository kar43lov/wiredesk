use crate::config::HostConfig;
use crate::session_thread::SessionStatus;

/// Tray icon color — three discrete states the user can read at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusColor {
    Green, // active client connection
    Yellow, // serial open, waiting for handshake
    Gray, // disconnected / serial down
}

pub fn status_color(status: &SessionStatus) -> StatusColor {
    match status {
        SessionStatus::Connected { .. } => StatusColor::Green,
        SessionStatus::Waiting => StatusColor::Yellow,
        SessionStatus::Disconnected(_) => StatusColor::Gray,
        // Notifications are transient (balloon-only) and never reach
        // the icon-color path. Default to Gray as a sensible no-op so a
        // future caller that forgets to special-case Notification doesn't
        // panic on a missing match arm.
        SessionStatus::Notification(_) => StatusColor::Gray,
    }
}

/// Render the Mac-side launch command the user should paste into a terminal
/// after copying from the settings window. Mapping of Windows COM port →
/// Mac /dev/cu.* device is impossible to do reliably (different hardware,
/// different drivers) — we just keep the Mac default and let the user edit.
pub fn format_mac_launch_command(config: &HostConfig) -> String {
    let mac_port = "/dev/cu.usbserial-120"; // sane default; user edits if needed
    format!(
        "./target/release/wiredesk-client --port {} --baud {}",
        mac_port, config.baud
    )
}

/// Baud must parse as u32 and meet a minimum useful threshold. We accept
/// anything ≥ 9600 because slower rates are not realistic for our 256-byte
/// clipboard chunks + heartbeat budget.
pub fn validate_baud(s: &str) -> Result<u32, String> {
    let v: u32 = s
        .trim()
        .parse()
        .map_err(|_| format!("not a valid baud rate: {s:?}"))?;
    if v < 9_600 {
        return Err(format!("baud too low: {v} (min 9600)"));
    }
    Ok(v)
}

/// Port must be non-empty after trimming.
pub fn validate_port(s: &str) -> Result<&str, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("port cannot be empty".to_string());
    }
    Ok(trimmed)
}

/// USB Vendor IDs of the two USB-to-serial chip families WireDesk's
/// null-modem link is built around. WCH (江苏沁恒微电子) makes the
/// CH340/CH341/CH343/CH9102 chips shipped in the original cable kit; FTDI
/// makes the FT232H/FT232R/FT2232 chips used for the high-baud upgrade.
/// A VID match (any PID) flags a port as a "target" adapter — the one the
/// user most likely wants Detect to auto-select.
pub const WCH_VID: u16 = 0x1A86;
pub const FTDI_VID: u16 = 0x0403;

/// What kind of serial port a discovered entry is. Drives both the dropdown
/// label and whether Detect auto-selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdapterKind {
    /// WCH CH340/CH341/CH343/CH9102 (VID 0x1A86).
    Ch34x,
    /// FTDI FT232x/FT2232/FT4232 (VID 0x0403).
    Ftdi,
    /// Some other USB-serial bridge (CP210x, PL2303, …) — listed, not auto-picked.
    OtherUsb,
    /// Non-USB serial port (PCI, Bluetooth SPP, on-board COM, …).
    NonUsb,
}

impl AdapterKind {
    /// A USB-serial adapter the WireDesk link is built around (CH34x or
    /// FTDI). Detect auto-selects these; everything else is shown in the
    /// dropdown but never auto-picked.
    pub fn is_target(self) -> bool {
        matches!(self, AdapterKind::Ch34x | AdapterKind::Ftdi)
    }
}

/// A serial port discovered on the system, classified and given a
/// human-readable label for the Settings dropdown (e.g. `"COM7 — FT232H"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedPort {
    /// Bare OS port name written into config (`COM7`, `/dev/cu.usbserial-1120`).
    pub port_name: String,
    /// Dropdown label: port name + chip / kind hint.
    pub label: String,
    pub kind: AdapterKind,
}

impl DetectedPort {
    pub fn is_target(&self) -> bool {
        self.kind.is_target()
    }
}

/// Friendly chip name from USB VID/PID. Returns `None` for vendors we don't
/// recognize — the caller falls back to a raw `VID:PID` label.
fn chip_label(vid: u16, pid: u16) -> Option<&'static str> {
    match (vid, pid) {
        (WCH_VID, 0x7523) => Some("CH340"),
        (WCH_VID, 0x5523) => Some("CH341"),
        (WCH_VID, 0x55D3) => Some("CH343"),
        (WCH_VID, 0x55D4) => Some("CH9102"),
        (WCH_VID, _) => Some("CH34x"),
        (FTDI_VID, 0x6001) => Some("FT232R"),
        (FTDI_VID, 0x6010) => Some("FT2232"),
        (FTDI_VID, 0x6011) => Some("FT4232"),
        (FTDI_VID, 0x6014) => Some("FT232H"),
        (FTDI_VID, 0x6015) => Some("FT-X"),
        (FTDI_VID, _) => Some("FTDI"),
        (0x10C4, _) => Some("CP210x"),
        (0x067B, _) => Some("PL2303"),
        _ => None,
    }
}

/// Classify every serial port the OS reported into a labeled `DetectedPort`.
/// Pure helper — caller supplies `serialport::available_ports()`. Order is
/// preserved: Windows COMx ordering already mirrors Device Manager, which the
/// user matches against by eye, so we don't sort. USB adapters get a chip
/// name; unknown USB devices get a raw `VID:PID`; non-USB ports are labeled
/// plainly.
pub fn classify_ports(ports: &[serialport::SerialPortInfo]) -> Vec<DetectedPort> {
    ports
        .iter()
        .map(|p| match &p.port_type {
            serialport::SerialPortType::UsbPort(info) => {
                let kind = match info.vid {
                    WCH_VID => AdapterKind::Ch34x,
                    FTDI_VID => AdapterKind::Ftdi,
                    _ => AdapterKind::OtherUsb,
                };
                let label = match chip_label(info.vid, info.pid) {
                    Some(chip) => format!("{} — {chip}", p.port_name),
                    None => format!(
                        "{} — USB serial {:04X}:{:04X}",
                        p.port_name, info.vid, info.pid
                    ),
                };
                DetectedPort {
                    port_name: p.port_name.clone(),
                    label,
                    kind,
                }
            }
            _ => DetectedPort {
                port_name: p.port_name.clone(),
                label: format!("{} — serial", p.port_name),
                kind: AdapterKind::NonUsb,
            },
        })
        .collect()
}

/// Indices (into `ports`) of the target adapters (CH34x / FTDI). Caller maps
/// the count to UX: 0 → ask the user to plug in or pick manually; 1 →
/// autofill; >1 → autofill the first and ask the user to confirm from the
/// list (the "CH340 and FT232H both plugged in" case).
pub fn target_indices(ports: &[DetectedPort]) -> Vec<usize> {
    ports
        .iter()
        .enumerate()
        .filter(|(_, p)| p.is_target())
        .map(|(i, _)| i)
        .collect()
}

/// Enumerate + classify the system's serial ports right now. Thin IO wrapper
/// over `serialport::available_ports()` so the UI handler stays a pure
/// dispatch; the classification logic in `classify_ports` is unit-tested
/// cross-platform. `Err` carries the OS enumeration error so the user sees
/// "enumeration failed: …" instead of a misleading "no adapter found" when
/// the OS API itself failed (driver missing, permissions denied).
pub fn enumerate_ports_now() -> Result<Vec<DetectedPort>, String> {
    match serialport::available_ports() {
        Ok(ports) => Ok(classify_ports(&ports)),
        Err(e) => {
            log::warn!("serialport::available_ports failed: {e}");
            Err(e.to_string())
        }
    }
}

/// Width / height: must parse as u16 and meet a sane minimum (we cap at the
/// u16 max from the protocol). VGA-class 320 is a generous floor — any real
/// monitor will far exceed this.
pub fn validate_dimension(s: &str) -> Result<u16, String> {
    let v: u16 = s
        .trim()
        .parse()
        .map_err(|_| format!("not a valid dimension: {s:?}"))?;
    if v < 320 {
        return Err(format!("dimension too small: {v} (min 320)"));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HostConfig {
        HostConfig::default()
    }

    #[test]
    fn status_color_connected_is_green() {
        assert_eq!(
            status_color(&SessionStatus::Connected {
                client_name: "x".to_string()
            }),
            StatusColor::Green
        );
    }

    #[test]
    fn status_color_waiting_is_yellow() {
        assert_eq!(status_color(&SessionStatus::Waiting), StatusColor::Yellow);
    }

    #[test]
    fn status_color_disconnected_is_gray() {
        assert_eq!(
            status_color(&SessionStatus::Disconnected("link down".to_string())),
            StatusColor::Gray
        );
    }

    #[test]
    fn format_mac_command_default_baud() {
        let s = format_mac_launch_command(&cfg());
        assert!(s.contains("/dev/cu.usbserial-120"));
        assert!(s.contains("--baud 115200"));
        assert!(s.starts_with("./target/release/wiredesk-client"));
    }

    #[test]
    fn format_mac_command_custom_baud() {
        let mut c = cfg();
        c.baud = 57_600;
        let s = format_mac_launch_command(&c);
        assert!(s.contains("--baud 57600"));
    }

    #[test]
    fn validate_baud_accepts_standard_rates() {
        for r in [9_600, 19_200, 115_200, 921_600] {
            assert!(validate_baud(&r.to_string()).is_ok(), "rate {r}");
        }
    }

    #[test]
    fn validate_baud_rejects_too_low() {
        assert!(validate_baud("100").is_err());
        assert!(validate_baud("9599").is_err());
    }

    #[test]
    fn validate_baud_rejects_garbage() {
        assert!(validate_baud("abc").is_err());
        assert!(validate_baud("").is_err());
        assert!(validate_baud("12.5").is_err());
    }

    #[test]
    fn validate_port_accepts_nonempty() {
        assert_eq!(validate_port("COM3").unwrap(), "COM3");
        assert_eq!(validate_port("  COM3  ").unwrap(), "COM3");
    }

    #[test]
    fn validate_port_rejects_empty() {
        assert!(validate_port("").is_err());
        assert!(validate_port("   ").is_err());
    }

    #[test]
    fn validate_dimension_accepts_realistic_sizes() {
        assert_eq!(validate_dimension("1920").unwrap(), 1920);
        assert_eq!(validate_dimension("2560").unwrap(), 2560);
        assert_eq!(validate_dimension("3840").unwrap(), 3840);
        assert_eq!(validate_dimension("65535").unwrap(), 65_535);
    }

    #[test]
    fn validate_dimension_rejects_too_small() {
        assert!(validate_dimension("0").is_err());
        assert!(validate_dimension("100").is_err());
        assert!(validate_dimension("319").is_err());
    }

    #[test]
    fn validate_dimension_rejects_overflow_and_garbage() {
        assert!(validate_dimension("65536").is_err()); // > u16::MAX
        assert!(validate_dimension("abc").is_err());
        assert!(validate_dimension("").is_err());
    }

    // ---- port classification + target detection ---------------------------

    use serialport::{SerialPortInfo, SerialPortType, UsbPortInfo};

    fn usb(name: &str, vid: u16, pid: u16) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid,
                serial_number: None,
                manufacturer: None,
                product: None,
            }),
        }
    }

    fn non_usb(name: &str, ty: SerialPortType) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_string(),
            port_type: ty,
        }
    }

    #[test]
    fn classify_empty_list_is_empty() {
        assert!(classify_ports(&[]).is_empty());
    }

    #[test]
    fn classify_labels_ch340_and_marks_target() {
        let ports = classify_ports(&[usb("COM7", WCH_VID, 0x7523)]);
        assert_eq!(ports[0].port_name, "COM7");
        assert_eq!(ports[0].label, "COM7 — CH340");
        assert_eq!(ports[0].kind, AdapterKind::Ch34x);
        assert!(ports[0].is_target());
    }

    #[test]
    fn classify_labels_ft232h_and_marks_target() {
        let ports = classify_ports(&[usb("COM5", FTDI_VID, 0x6014)]);
        assert_eq!(ports[0].label, "COM5 — FT232H");
        assert_eq!(ports[0].kind, AdapterKind::Ftdi);
        assert!(ports[0].is_target());
    }

    #[test]
    fn classify_unknown_usb_uses_raw_vid_pid_and_is_not_target() {
        let ports = classify_ports(&[usb("COM9", 0x1234, 0xABCD)]);
        assert_eq!(ports[0].label, "COM9 — USB serial 1234:ABCD");
        assert_eq!(ports[0].kind, AdapterKind::OtherUsb);
        assert!(!ports[0].is_target());
    }

    #[test]
    fn classify_known_other_usb_bridge_gets_chip_name_but_is_not_target() {
        // CP210x is labeled (helps the user recognize it) but isn't a
        // WireDesk adapter, so Detect won't auto-select it.
        let ports = classify_ports(&[usb("COM4", 0x10C4, 0xEA60)]);
        assert_eq!(ports[0].label, "COM4 — CP210x");
        assert_eq!(ports[0].kind, AdapterKind::OtherUsb);
        assert!(!ports[0].is_target());
    }

    #[test]
    fn classify_non_usb_is_plain_and_not_target() {
        let ports = classify_ports(&[
            non_usb("COM1", SerialPortType::PciPort),
            non_usb("COM2", SerialPortType::BluetoothPort),
        ]);
        assert_eq!(ports[0].label, "COM1 — serial");
        assert_eq!(ports[1].label, "COM2 — serial");
        assert!(ports.iter().all(|p| p.kind == AdapterKind::NonUsb));
        assert!(ports.iter().all(|p| !p.is_target()));
    }

    #[test]
    fn classify_preserves_order_with_mixed_kinds() {
        // The "CH340 and FT232H both plugged in" case the user called out.
        let ports = classify_ports(&[
            usb("COM3", WCH_VID, 0x7523),  // CH340
            usb("COM7", FTDI_VID, 0x6014), // FT232H
            non_usb("COM1", SerialPortType::Unknown),
        ]);
        assert_eq!(
            ports.iter().map(|p| p.port_name.as_str()).collect::<Vec<_>>(),
            vec!["COM3", "COM7", "COM1"]
        );
    }

    #[test]
    fn target_indices_picks_ch34x_and_ftdi_only() {
        let ports = classify_ports(&[
            usb("COM3", WCH_VID, 0x7523),  // CH340  → target
            usb("COM4", 0x10C4, 0xEA60),   // CP2102 → not
            usb("COM7", FTDI_VID, 0x6014), // FT232H → target
            non_usb("COM1", SerialPortType::Unknown),
        ]);
        assert_eq!(target_indices(&ports), vec![0, 2]);
    }

    #[test]
    fn target_indices_empty_when_no_wiredesk_adapter() {
        let ports = classify_ports(&[
            usb("COM4", 0x10C4, 0xEA60),
            non_usb("COM1", SerialPortType::PciPort),
        ]);
        assert!(target_indices(&ports).is_empty());
    }

    #[test]
    fn chip_label_covers_wch_and_ftdi_variants() {
        assert_eq!(chip_label(WCH_VID, 0x7523), Some("CH340"));
        assert_eq!(chip_label(WCH_VID, 0x55D4), Some("CH9102"));
        assert_eq!(chip_label(WCH_VID, 0x9999), Some("CH34x")); // unknown WCH PID
        assert_eq!(chip_label(FTDI_VID, 0x6014), Some("FT232H"));
        assert_eq!(chip_label(FTDI_VID, 0x6001), Some("FT232R"));
        assert_eq!(chip_label(FTDI_VID, 0x9999), Some("FTDI")); // unknown FTDI PID
        assert_eq!(chip_label(0xDEAD, 0xBEEF), None);
    }
}
