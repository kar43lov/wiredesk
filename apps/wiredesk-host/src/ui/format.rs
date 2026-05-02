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

/// USB Vendor ID for WCH (江苏沁恒微电子) — the maker of the CH340 / CH341
/// USB-to-UART chips we ship in the cable kit. All PIDs (0x7523, 0x55D3,
/// 0x55D4, …) sit under the same VID, so a VID-only filter picks up every
/// CH340/CH341/CH343/CH9102 variant the user might plug in.
pub const WCH_VID: u16 = 0x1A86;

/// Outcome of an auto-detect scan over the system's USB serial ports.
/// Caller decides UX: `Found` → autofill the port input; `Multiple` →
/// show the list and ask the user to pick; `NotFound` → tell the user to
/// plug the cable in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectResult {
    Found(String),
    Multiple(Vec<String>),
    NotFound,
}

/// Filter the given port list for USB devices whose VID matches WCH
/// (0x1A86). Pure helper — caller supplies `serialport::available_ports()`.
/// Order in `Multiple` follows the order in which the OS reported the
/// ports; we don't sort because COMx ordering on Windows already reflects
/// enumeration order (which the user's brain matches against Device Manager).
pub fn detect_ch340_port(ports: &[serialport::SerialPortInfo]) -> DetectResult {
    let matches: Vec<String> = ports
        .iter()
        .filter(|p| {
            matches!(
                &p.port_type,
                serialport::SerialPortType::UsbPort(info) if info.vid == WCH_VID
            )
        })
        .map(|p| p.port_name.clone())
        .collect();
    match matches.len() {
        0 => DetectResult::NotFound,
        1 => DetectResult::Found(matches.into_iter().next().unwrap()),
        _ => DetectResult::Multiple(matches),
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
        HostConfig {
            port: "COM3".to_string(),
            baud: 115_200,
            width: 2560,
            height: 1440,
            host_name: "wiredesk-host".to_string(),
            run_on_startup: false,
        }
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

    // ---- detect_ch340_port -------------------------------------------------

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
    fn detect_returns_notfound_on_empty_list() {
        let ports: Vec<SerialPortInfo> = vec![];
        assert_eq!(detect_ch340_port(&ports), DetectResult::NotFound);
    }

    #[test]
    fn detect_returns_notfound_when_only_non_usb_ports() {
        let ports = vec![
            non_usb("COM1", SerialPortType::PciPort),
            non_usb("COM2", SerialPortType::BluetoothPort),
            non_usb("COM5", SerialPortType::Unknown),
        ];
        assert_eq!(detect_ch340_port(&ports), DetectResult::NotFound);
    }

    #[test]
    fn detect_returns_notfound_when_only_non_wch_usb_devices() {
        // FTDI (0x0403) and Silicon Labs (0x10C4) — common non-WCH USB UARTs.
        let ports = vec![
            usb("COM3", 0x0403, 0x6001), // FTDI FT232
            usb("COM4", 0x10C4, 0xEA60), // CP2102
        ];
        assert_eq!(detect_ch340_port(&ports), DetectResult::NotFound);
    }

    #[test]
    fn detect_returns_found_for_single_ch340() {
        let ports = vec![
            usb("COM3", 0x0403, 0x6001), // FTDI — should be skipped
            usb("COM7", WCH_VID, 0x7523), // CH340
            non_usb("COM8", SerialPortType::Unknown),
        ];
        assert_eq!(
            detect_ch340_port(&ports),
            DetectResult::Found("COM7".to_string())
        );
    }

    #[test]
    fn detect_returns_multiple_when_two_or_more_ch340() {
        let ports = vec![
            usb("COM3", WCH_VID, 0x7523),
            usb("COM4", 0x0403, 0x6001), // FTDI — filtered out
            usb("COM7", WCH_VID, 0x55D4),
            usb("COM9", WCH_VID, 0x55D3),
        ];
        assert_eq!(
            detect_ch340_port(&ports),
            DetectResult::Multiple(vec![
                "COM3".to_string(),
                "COM7".to_string(),
                "COM9".to_string(),
            ])
        );
    }

    #[test]
    fn detect_matches_all_known_pid_variants_via_vid() {
        // CH340 (0x7523), CH343 (0x55D3), CH9102 (0x55D4) all share VID
        // 0x1A86 — VID-only filter picks them all individually.
        for pid in [0x7523_u16, 0x55D3, 0x55D4] {
            let ports = vec![usb("COM10", WCH_VID, pid)];
            assert_eq!(
                detect_ch340_port(&ports),
                DetectResult::Found("COM10".to_string()),
                "PID 0x{pid:04X} should be detected via WCH VID filter"
            );
        }
    }
}
