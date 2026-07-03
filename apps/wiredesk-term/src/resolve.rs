//! Resolve which serial port / baud rate `wd` should use.
//!
//! `wiredesk-term` used to hardcode `--port /dev/cu.usbserial-120` and
//! `--baud 115200` as its clap defaults and nothing else — unlike
//! `wiredesk-host`/`wiredesk-client`, it never read `config.toml`, so it
//! silently drifted from whatever the GUI's Settings panel actually had
//! (this Mac has been on `/dev/cu.usbserial-140` @ 3_000_000 since the
//! FT232H upgrade). Worse, macOS reassigns `/dev/cu.usbserial-NNN` by
//! physical USB port location, so even the config.toml value goes stale
//! the moment the cable moves to a different port.
//!
//! Resolution order (highest priority first), mirroring the
//! defaults→config→CLI convention used elsewhere in the project, with one
//! addition ahead of config.toml:
//!
//! 1. `--port` / `--baud` explicitly passed on the CLI — always wins.
//! 2. (port only) A single unambiguous WCH/FTDI adapter detected on the
//!    system right now — self-healing across USB replugs, which is the
//!    whole point of this module existing.
//! 3. `port` / `baud` from `config.toml` (the same file the GUI Settings
//!    panel reads and writes).
//! 4. The hardcoded clap default, unchanged, as a last resort.

use std::path::{Path, PathBuf};

use wiredesk_transport::detect::{enumerate_ports_now, target_indices, DetectedPort};

/// Where the resolved value came from — surfaced in the startup banner so
/// `wd` never silently guesses without saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSource {
    Cli,
    AutoDetected,
    ConfigToml,
    Fallback,
}

impl ValueSource {
    pub fn label(self) -> &'static str {
        match self {
            ValueSource::Cli => "explicit --port/--baud",
            ValueSource::AutoDetected => "auto-detected adapter",
            ValueSource::ConfigToml => "config.toml",
            ValueSource::Fallback => "default guess",
        }
    }
}

/// Only the two fields `wd` cares about. The real config.toml (written by
/// `wiredesk-client`) has many more fields (width, height, bluetooth
/// section, …) — serde ignores unknown fields on a struct by default, so
/// this stays forward-compatible without listing them.
#[derive(serde::Deserialize, Default)]
struct TermTomlConfig {
    port: Option<String>,
    baud: Option<u32>,
}

/// Same path `wiredesk-client`'s `ClientConfig::config_path()` uses — the
/// two binaries share one config file so `wd` picks up whatever the GUI's
/// Settings panel last saved.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WireDesk")
        .join("config.toml")
}

/// Read `port`/`baud` out of config.toml. Missing file, parse error, or
/// missing fields all resolve to `None` for that field — never a hard
/// error, since config.toml is optional context, not a requirement.
pub fn load_toml_port_baud(path: &Path) -> (Option<String>, Option<u32>) {
    match std::fs::read_to_string(path) {
        Ok(s) => match toml::from_str::<TermTomlConfig>(&s) {
            Ok(cfg) => (cfg.port, cfg.baud),
            Err(_) => (None, None),
        },
        Err(_) => (None, None),
    }
}

/// IO wrapper: enumerate the system's serial ports right now and return the
/// port names of every WCH/FTDI ("target") adapter found. Enumeration
/// failure (driver/permissions issue) degrades to "none found" rather than
/// erroring — the caller falls through to config.toml/fallback exactly as
/// if no adapter were plugged in.
pub fn detect_target_ports() -> Vec<String> {
    let ports: Vec<DetectedPort> = match enumerate_ports_now() {
        Ok(ports) => ports,
        Err(e) => {
            log::warn!("serial port enumeration failed: {e}");
            return Vec::new();
        }
    };
    let names: Vec<String> = target_indices(&ports)
        .into_iter()
        .map(|i| ports[i].port_name.clone())
        .collect();
    dedupe_tty_variants(names)
}

/// Drop macOS's `/dev/tty.*` dialin duplicate of each `/dev/cu.*` callout
/// device. `serialport::available_ports()` lists a single physical
/// USB-serial adapter twice on macOS — once as `/dev/cu.usbserial-140`
/// (callout, doesn't block waiting for carrier-detect — what WireDesk
/// always uses) and once as `/dev/tty.usbserial-140` (dialin), both
/// carrying the same VID/PID. Without this, one plugged-in adapter looks
/// like two ("ambiguous") and auto-detect always falls through to
/// config.toml/fallback. No-op on naming schemes without the cu./tty.
/// split (Windows `COMn` never matches the `/dev/tty.` prefix).
fn dedupe_tty_variants(names: Vec<String>) -> Vec<String> {
    names.into_iter().filter(|n| !n.starts_with("/dev/tty.")).collect()
}

/// Pure decision: given what CLI/detection/config.toml produced, which port
/// wins? Ambiguous detection (0 or >1 target adapters) is treated the same
/// as "nothing detected" — silently guessing between two live cables would
/// be exactly the kind of magic this codebase's config-resolution
/// convention avoids; the caller falls through to config.toml/fallback and
/// the user disambiguates with `--port`.
pub fn resolve_port(
    cli_port: Option<String>,
    detected_targets: &[String],
    toml_port: Option<String>,
    fallback: &str,
) -> (String, ValueSource) {
    if let Some(p) = cli_port {
        return (p, ValueSource::Cli);
    }
    if detected_targets.len() == 1 {
        return (detected_targets[0].clone(), ValueSource::AutoDetected);
    }
    if let Some(p) = toml_port {
        return (p, ValueSource::ConfigToml);
    }
    (fallback.to_string(), ValueSource::Fallback)
}

/// Pure decision for baud: no hardware signal to auto-detect a line rate
/// from, so this tier is just CLI > config.toml > fallback.
pub fn resolve_baud(cli_baud: Option<u32>, toml_baud: Option<u32>, fallback: u32) -> (u32, ValueSource) {
    if let Some(b) = cli_baud {
        return (b, ValueSource::Cli);
    }
    if let Some(b) = toml_baud {
        return (b, ValueSource::ConfigToml);
    }
    (fallback, ValueSource::Fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- resolve_port -------------------------------------------------

    #[test]
    fn cli_port_wins_over_everything() {
        let (port, src) = resolve_port(
            Some("/dev/cu.explicit".to_string()),
            &["/dev/cu.usbserial-140".to_string()],
            Some("/dev/cu.usbserial-999".to_string()),
            "/dev/cu.usbserial-120",
        );
        assert_eq!(port, "/dev/cu.explicit");
        assert_eq!(src, ValueSource::Cli);
    }

    #[test]
    fn single_detected_adapter_wins_over_toml_and_fallback() {
        let (port, src) = resolve_port(
            None,
            &["/dev/cu.usbserial-140".to_string()],
            Some("/dev/cu.usbserial-999".to_string()),
            "/dev/cu.usbserial-120",
        );
        assert_eq!(port, "/dev/cu.usbserial-140");
        assert_eq!(src, ValueSource::AutoDetected);
    }

    #[test]
    fn no_detection_falls_back_to_toml() {
        let (port, src) = resolve_port(
            None,
            &[],
            Some("/dev/cu.usbserial-999".to_string()),
            "/dev/cu.usbserial-120",
        );
        assert_eq!(port, "/dev/cu.usbserial-999");
        assert_eq!(src, ValueSource::ConfigToml);
    }

    #[test]
    fn ambiguous_detection_falls_back_to_toml_not_first_match() {
        // Two adapters plugged in at once (e.g. CH340 + FT232H) — don't
        // silently guess, fall through same as zero detected.
        let (port, src) = resolve_port(
            None,
            &[
                "/dev/cu.usbserial-140".to_string(),
                "/dev/cu.usbserial-200".to_string(),
            ],
            Some("/dev/cu.usbserial-999".to_string()),
            "/dev/cu.usbserial-120",
        );
        assert_eq!(port, "/dev/cu.usbserial-999");
        assert_eq!(src, ValueSource::ConfigToml);
    }

    #[test]
    fn nothing_resolved_uses_hardcoded_fallback() {
        let (port, src) = resolve_port(None, &[], None, "/dev/cu.usbserial-120");
        assert_eq!(port, "/dev/cu.usbserial-120");
        assert_eq!(src, ValueSource::Fallback);
    }

    // ---- dedupe_tty_variants ---------------------------------------------

    #[test]
    fn dedupe_drops_tty_keeps_cu() {
        let names = dedupe_tty_variants(vec![
            "/dev/cu.usbserial-140".to_string(),
            "/dev/tty.usbserial-140".to_string(),
        ]);
        assert_eq!(names, vec!["/dev/cu.usbserial-140".to_string()]);
    }

    #[test]
    fn dedupe_passthrough_for_com_style_names() {
        let names = dedupe_tty_variants(vec!["COM5".to_string(), "COM7".to_string()]);
        assert_eq!(names, vec!["COM5".to_string(), "COM7".to_string()]);
    }

    #[test]
    fn dedupe_empty_stays_empty() {
        assert!(dedupe_tty_variants(vec![]).is_empty());
    }

    // ---- resolve_baud ---------------------------------------------------

    #[test]
    fn cli_baud_wins() {
        let (baud, src) = resolve_baud(Some(57_600), Some(3_000_000), 115_200);
        assert_eq!(baud, 57_600);
        assert_eq!(src, ValueSource::Cli);
    }

    #[test]
    fn toml_baud_used_when_no_cli() {
        let (baud, src) = resolve_baud(None, Some(3_000_000), 115_200);
        assert_eq!(baud, 3_000_000);
        assert_eq!(src, ValueSource::ConfigToml);
    }

    #[test]
    fn fallback_baud_when_nothing_else() {
        let (baud, src) = resolve_baud(None, None, 115_200);
        assert_eq!(baud, 115_200);
        assert_eq!(src, ValueSource::Fallback);
    }

    // ---- load_toml_port_baud --------------------------------------------

    #[test]
    fn load_toml_missing_file_returns_none_none() {
        let (port, baud) = load_toml_port_baud(Path::new("/nonexistent/path/config.toml"));
        assert_eq!(port, None);
        assert_eq!(baud, None);
    }

    #[test]
    fn load_toml_parses_port_and_baud_ignoring_extra_fields() {
        let dir = std::env::temp_dir().join(format!(
            "wiredesk-term-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
port = "/dev/cu.usbserial-140"
baud = 3000000
width = 2560
height = 1440

[bluetooth]
service_uuid = "cc7d466c-21f3-41ba-a711-991adf9f218e"
"#,
        )
        .unwrap();

        let (port, baud) = load_toml_port_baud(&path);
        assert_eq!(port, Some("/dev/cu.usbserial-140".to_string()));
        assert_eq!(baud, Some(3_000_000));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_toml_garbage_returns_none_none() {
        let dir = std::env::temp_dir().join(format!(
            "wiredesk-term-test-garbage-{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "not valid toml {{{").unwrap();

        let (port, baud) = load_toml_port_baud(&path);
        assert_eq!(port, None);
        assert_eq!(baud, None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
