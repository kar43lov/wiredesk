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

// Port discovery / classification (WCH_VID, FTDI_VID, AdapterKind,
// DetectedPort, classify_ports, target_indices, enumerate_ports_now) lives in
// `wiredesk-transport::detect` — shared with `wiredesk-term`'s startup
// auto-detect. Re-exported here so existing `format::` call sites keep
// working. `wiredesk-host` is a bin-only crate (no external consumers), so
// rustc's unused_imports lint can't see these as "used" via the `ui::format::`
// qualified paths in main.rs / settings_window.rs — allow is the standard
// fix for a re-export in that situation.
#[allow(unused_imports)]
pub use wiredesk_transport::detect::{
    classify_ports, enumerate_ports_now, target_indices, AdapterKind, DetectedPort, FTDI_VID,
    WCH_VID,
};

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

    // Port classification + target detection (classify_ports, target_indices,
    // chip_label) is unit-tested in wiredesk-transport::detect — this module
    // only re-exports it.
}
