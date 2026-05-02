use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use clap::parser::ValueSource;
use clap::ArgMatches;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct ClientConfig {
    pub port: String,
    pub baud: u32,
    pub width: u16,
    pub height: u16,
    pub client_name: String,
    /// Human-readable name (`NSScreen.localizedName`) of the preferred
    /// fullscreen target — e.g. "Studio Display", "Built-in Retina Display".
    /// `None` → use the display the window currently sits on.
    /// `#[serde(default)]` on the struct makes a missing field deserialize as
    /// `None`, so existing configs round-trip safely without migration.
    ///
    /// Stored as a name (not an `NSScreen::screens()` index) because ordinals
    /// aren't stable across reboot / dock event / hot-plug — a saved index
    /// stays in-range but silently points at a different physical display.
    /// Name-based resolution survives reboots and re-orderings; if the user
    /// renames the display in System Settings the saved preference falls
    /// back to "active monitor" until they re-pick.
    pub preferred_monitor: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: "/dev/cu.usbserial-120".to_string(),
            baud: 115_200,
            width: 2560,
            height: 1440,
            client_name: "wiredesk-client".to_string(),
            preferred_monitor: None,
        }
    }
}

#[allow(dead_code)] // wired up in later tasks of the launcher-ui plan
impl ClientConfig {
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
            Ok(s) => match toml::from_str::<ClientConfig>(&s) {
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

/// Merge `ClientConfig` (from TOML / defaults) with parsed CLI args.
/// CLI values explicitly provided by the user (CommandLine / EnvVariable
/// sources) override TOML; default-only sources fall back to TOML.
pub fn merge_args(matches: &ArgMatches, mut cfg: ClientConfig) -> ClientConfig {
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
            cfg.client_name = v.clone();
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
        let cfg = ClientConfig::default();
        assert_eq!(cfg.port, "/dev/cu.usbserial-120");
        assert_eq!(cfg.baud, 115_200);
        assert_eq!(cfg.width, 2560);
        assert_eq!(cfg.height, 1440);
        assert_eq!(cfg.client_name, "wiredesk-client");
        assert!(cfg.preferred_monitor.is_none());
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = ClientConfig {
            port: "/dev/cu.wch-1".to_string(),
            baud: 57_600,
            width: 1920,
            height: 1080,
            client_name: "test-client".to_string(),
            preferred_monitor: None,
        };
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        cfg.save_to(&path).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn load_missing_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg, ClientConfig::default());
    }

    #[test]
    fn save_creates_parent_dir() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("config.toml");
        assert!(!path.parent().unwrap().exists());
        let cfg = ClientConfig::default();
        cfg.save_to(&path).unwrap();
        assert!(path.exists());
        assert!(path.parent().unwrap().is_dir());
    }

    #[test]
    fn load_garbage_returns_default() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this is not valid toml [[[[").unwrap();
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg, ClientConfig::default());
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_fields() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "port = \"/dev/cu.usbserial-999\"\n").unwrap();
        let cfg = ClientConfig::load_from(&path);
        assert_eq!(cfg.port, "/dev/cu.usbserial-999");
        assert_eq!(cfg.baud, 115_200);
        assert_eq!(cfg.client_name, "wiredesk-client");
        // New `preferred_monitor` field: a TOML written before the field
        // existed must round-trip as `None` rather than fail to deserialize.
        assert!(cfg.preferred_monitor.is_none());
    }

    #[test]
    fn toml_roundtrip_preferred_monitor() {
        let dir = tempdir().unwrap();

        // Case 1: None — the implicit default. Should survive roundtrip.
        let cfg_none = ClientConfig {
            preferred_monitor: None,
            ..ClientConfig::default()
        };
        let path = dir.path().join("none.toml");
        cfg_none.save_to(&path).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert_eq!(loaded, cfg_none);
        assert!(loaded.preferred_monitor.is_none());

        // Case 2: Some(name) — a real display name. Should survive roundtrip.
        let cfg_some = ClientConfig {
            preferred_monitor: Some("Studio Display".to_string()),
            ..ClientConfig::default()
        };
        let path = dir.path().join("some.toml");
        cfg_some.save_to(&path).unwrap();
        let loaded = ClientConfig::load_from(&path);
        assert_eq!(loaded, cfg_some);
        assert_eq!(
            loaded.preferred_monitor.as_deref(),
            Some("Studio Display")
        );
    }

    fn toml_cfg() -> ClientConfig {
        ClientConfig {
            port: "/dev/cu.from-toml".to_string(),
            baud: 9_600,
            width: 1280,
            height: 720,
            client_name: "from-toml".to_string(),
            preferred_monitor: None,
        }
    }

    #[test]
    fn merge_no_cli_args_keeps_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-client"])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "/dev/cu.from-toml");
        assert_eq!(merged.baud, 9_600);
        assert_eq!(merged.client_name, "from-toml");
    }

    #[test]
    fn merge_cli_port_overrides_toml() {
        let matches = Args::command()
            .try_get_matches_from(["wiredesk-client", "--port", "/dev/cu.from-cli"])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "/dev/cu.from-cli");
        assert_eq!(merged.baud, 9_600); // not overridden — keeps TOML
        assert_eq!(merged.client_name, "from-toml");
    }

    #[test]
    fn merge_cli_all_fields_override_toml() {
        let matches = Args::command()
            .try_get_matches_from([
                "wiredesk-client",
                "--port", "/dev/cu.cli",
                "--baud", "57600",
                "--name", "cli-name",
            ])
            .unwrap();
        let merged = merge_args(&matches, toml_cfg());
        assert_eq!(merged.port, "/dev/cu.cli");
        assert_eq!(merged.baud, 57_600);
        assert_eq!(merged.client_name, "cli-name");
    }
}
