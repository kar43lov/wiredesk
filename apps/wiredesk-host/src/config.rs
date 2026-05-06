use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::parser::ValueSource;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};
use wiredesk_core::BluetoothConfig;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct HostConfig {
    pub port: String,
    pub baud: u32,
    pub width: u16,
    pub height: u16,
    pub host_name: String,
    pub run_on_startup: bool,

    /// Which transport to open on startup. `"serial"` (default) keeps the
    /// existing behaviour. `"bluetooth"` opens the BLE Peripheral GATT
    /// server and ignores the serial fields.
    #[serde(default = "default_transport")]
    pub transport: String,

    /// If the primary transport fails to open, fall back to this one.
    /// Currently only `"serial"` makes sense as a fallback. `None` (or
    /// missing field) = no fallback, error out and exit.
    #[serde(default)]
    pub transport_fallback: Option<String>,

    /// Bluetooth-specific settings. Used only when `transport == "bluetooth"`.
    #[serde(default)]
    pub bluetooth: BluetoothConfig,
}

fn default_transport() -> String {
    "serial".to_string()
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            port: "COM3".to_string(),
            baud: 115_200,
            width: 2560,
            height: 1440,
            host_name: "wiredesk-host".to_string(),
            run_on_startup: false,
            transport: default_transport(),
            transport_fallback: None,
            bluetooth: BluetoothConfig::default(),
        }
    }
}

#[allow(dead_code)] // wired up in later tasks of the launcher-ui plan
impl HostConfig {
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("WireDesk")
            .join("config.toml")
    }

    pub fn load() -> Self {
        Self::load_from(&Self::config_path())
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_to(&Self::config_path())
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(s) => match toml::from_str::<HostConfig>(&s) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::warn!("config parse error at {}: {e}; using defaults", path.display());
                    Self::default()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                log::warn!("config read error at {}: {e}; using defaults", path.display());
                Self::default()
            }
        }
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        fs::write(path, s)
    }
}

/// Merge `HostConfig` (from TOML / defaults) with parsed CLI args.
/// CLI values explicitly provided by the user (CommandLine / EnvVariable
/// sources) override TOML; default-only sources fall back to TOML.
pub fn merge_args(matches: &ArgMatches, mut cfg: HostConfig) -> HostConfig {
    if from_user(matches.value_source("port")) {
        if let Some(v) = matches.get_one::<String>("port") {
            cfg.port = v.clone();
        }
    }
    if from_user(matches.value_source("baud")) {
        if let Some(v) = matches.get_one::<u32>("baud") {
            cfg.baud = *v;
        }
    }
    if from_user(matches.value_source("name")) {
        if let Some(v) = matches.get_one::<String>("name") {
            cfg.host_name = v.clone();
        }
    }
    if from_user(matches.value_source("width")) {
        if let Some(v) = matches.get_one::<u16>("width") {
            cfg.width = *v;
        }
    }
    if from_user(matches.value_source("height")) {
        if let Some(v) = matches.get_one::<u16>("height") {
            cfg.height = *v;
        }
    }
    if from_user(matches.value_source("transport")) {
        if let Some(v) = matches.get_one::<String>("transport") {
            cfg.transport = v.clone();
        }
    }
    cfg
}

fn from_user(src: Option<ValueSource>) -> bool {
    matches!(
        src,
        Some(ValueSource::CommandLine) | Some(ValueSource::EnvVariable)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Args;
    use clap::CommandFactory;
    use tempfile::tempdir;

    #[test]
    fn defaults_match_hardcodes() {
        let cfg = HostConfig::default();
        assert_eq!(cfg.port, "COM3");
        assert_eq!(cfg.baud, 115_200);
        assert_eq!(cfg.width, 2560);
        assert_eq!(cfg.height, 1440);
        assert_eq!(cfg.host_name, "wiredesk-host");
        assert!(!cfg.run_on_startup);
        assert_eq!(cfg.transport, "serial");
        assert!(cfg.transport_fallback.is_none());
        assert_eq!(cfg.bluetooth, BluetoothConfig::default());
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = HostConfig {
            port: "COM7".to_string(),
            baud: 57_600,
            width: 1920,
            height: 1080,
            host_name: "test-host".to_string(),
            run_on_startup: true,
            transport: "serial".to_string(),
            transport_fallback: None,
            bluetooth: BluetoothConfig::default(),
        };
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        cfg.save_to(&path).unwrap();
        let loaded = HostConfig::load_from(&path);
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn toml_transport_bluetooth_section_roundtrips() {
        let cfg = HostConfig {
            transport: "bluetooth".to_string(),
            transport_fallback: Some("serial".to_string()),
            bluetooth: BluetoothConfig {
                service_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                peer_name: "TestHost".to_string(),
                mtu: 244,
                connect_timeout_secs: 5,
                reconnect_max_attempts: 3,
            },
            ..HostConfig::default()
        };
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        cfg.save_to(&path).unwrap();
        let loaded = HostConfig::load_from(&path);
        assert_eq!(loaded.transport, "bluetooth");
        assert_eq!(loaded.transport_fallback.as_deref(), Some("serial"));
        assert_eq!(loaded.bluetooth.peer_name, "TestHost");
        assert_eq!(loaded.bluetooth.mtu, 244);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn partial_toml_without_bluetooth_section_uses_defaults() {
        // A TOML file written before the BLE plumbing existed must still
        // load cleanly; the absent [bluetooth] section and `transport`
        // field deserialize to their defaults via #[serde(default)].
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.toml");
        fs::write(
            &path,
            "port = \"COM5\"\n\
             baud = 115200\n\
             width = 2560\n\
             height = 1440\n\
             host_name = \"legacy-host\"\n",
        )
        .unwrap();
        let cfg = HostConfig::load_from(&path);
        assert_eq!(cfg.port, "COM5");
        assert_eq!(cfg.transport, "serial");
        assert!(cfg.transport_fallback.is_none());
        assert_eq!(cfg.bluetooth, BluetoothConfig::default());
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");
        let cfg = HostConfig::load_from(&path);
        assert_eq!(cfg, HostConfig::default());
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("config.toml");
        assert!(!path.parent().unwrap().exists());
        let cfg = HostConfig::default();
        cfg.save_to(&path).unwrap();
        assert!(path.exists());
        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn load_garbage_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is not valid toml [[[[").unwrap();
        let cfg = HostConfig::load_from(&path);
        assert_eq!(cfg, HostConfig::default());
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "port = \"COM9\"\n").unwrap();
        let cfg = HostConfig::load_from(&path);
        assert_eq!(cfg.port, "COM9");
        assert_eq!(cfg.baud, 115_200);
        assert_eq!(cfg.host_name, "wiredesk-host");
    }

    fn toml_cfg() -> HostConfig {
        HostConfig {
            port: "COM_TOML".to_string(),
            baud: 9_600,
            width: 1280,
            height: 720,
            host_name: "from-toml".to_string(),
            run_on_startup: true,
            transport: "serial".to_string(),
            transport_fallback: None,
            bluetooth: BluetoothConfig::default(),
        }
    }

    #[test]
    fn merge_no_cli_args_keeps_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-host"])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "COM_TOML");
        assert_eq!(merged.baud, 9_600);
        assert_eq!(merged.width, 1280);
        assert_eq!(merged.host_name, "from-toml");
        assert!(merged.run_on_startup); // not exposed via CLI — survives merge
    }

    #[test]
    fn merge_cli_port_overrides_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-host", "--port", "COM_CLI"])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "COM_CLI");
        assert_eq!(merged.baud, 9_600); // not overridden — keeps TOML
    }

    #[test]
    fn merge_cli_full_override() {
        let matches = Args::command()
            .try_get_matches_from([
                "wiredesk-host",
                "--port", "COM7",
                "--baud", "57600",
                "--name", "cli-host",
                "--width", "1920",
                "--height", "1080",
            ])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "COM7");
        assert_eq!(merged.baud, 57_600);
        assert_eq!(merged.host_name, "cli-host");
        assert_eq!(merged.width, 1920);
        assert_eq!(merged.height, 1080);
        assert!(merged.run_on_startup); // still TOML — no CLI flag
    }

    #[test]
    fn merge_cli_transport_overrides_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-host", "--transport", "bluetooth"])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.transport, "bluetooth");
        // Other fields unchanged.
        assert_eq!(merged.port, "COM_TOML");
        assert_eq!(merged.baud, 9_600);
    }

    #[test]
    fn merge_no_transport_arg_keeps_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-host"])
            .unwrap();
        let mut cfg = toml_cfg();
        cfg.transport = "bluetooth".to_string();
        let merged = merge_args(&matches, cfg);
        // No --transport CLI arg → TOML's "bluetooth" survives.
        assert_eq!(merged.transport, "bluetooth");
    }
}
