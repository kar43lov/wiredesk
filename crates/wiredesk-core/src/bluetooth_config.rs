//! Shared configuration for the Bluetooth LE transport (Plan C).
//!
//! Lives in `wiredesk-core` rather than per-app config crates so the same
//! defaults — most importantly `service_uuid` — are guaranteed to match on
//! both ends of the link. A drift between host and client UUIDs would mean
//! the BLE Central can't discover the BLE Peripheral; this struct prevents
//! that class of bug at the type level.

use serde::{Deserialize, Serialize};

/// Default custom GATT service UUID. Must match the constant baked into
/// `wiredesk-transport::bluetooth::uuids::SERVICE_UUID`. Tested in the
/// transport crate (`uuids::tests`) — the BLE peer can override via
/// `config.toml` for advanced setups but the default is the source of truth.
pub const DEFAULT_SERVICE_UUID: &str = "cc7d466c-21f3-41ba-a711-991adf9f218e";

/// Default ATT MTU we negotiate after connect. 247 = 244 ATT payload +
/// 3-byte ATT header; minus our 4-byte ChunkHeader leaves 240 bytes per
/// chunk. See `wiredesk-transport::bluetooth::fragment` for the math.
pub const DEFAULT_MTU: u16 = 247;

/// Default connect-attempt timeout (seconds). Long enough for a sleep-wake
/// recovery cycle, short enough that an empty channel surfaces an error
/// before the user gives up.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u32 = 30;

/// 0 = unlimited. Loop runs forever with backoff `next_backoff()` from
/// the reconnect helper (Task 10 of the BLE transport plan).
pub const DEFAULT_RECONNECT_MAX_ATTEMPTS: u32 = 0;

/// Default advertised peer name on the Win side / scan filter on the Mac
/// side. Plain ASCII to avoid any encoding round-trip surprises in the
/// scan response payload.
pub const DEFAULT_PEER_NAME: &str = "WireDeskHost";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct BluetoothConfig {
    /// Custom GATT service UUID — must be identical on both peers.
    pub service_uuid: String,

    /// Local advertised name on the Peripheral side and scan filter on the
    /// Central side. ASCII keeps the BLE advertisement payload predictable.
    pub peer_name: String,

    /// ATT MTU to attempt to negotiate after connect. If the peer doesn't
    /// agree we fall back to whatever the GATT stack returned.
    pub mtu: u16,

    /// How long to wait for the initial scan + connect before erroring out.
    pub connect_timeout_secs: u32,

    /// 0 means "retry forever with exponential backoff" — the typical case.
    /// A positive value caps total reconnect attempts (useful in tests and
    /// for headless agents where you want a hard fail rather than a silent
    /// background loop).
    pub reconnect_max_attempts: u32,
}

impl Default for BluetoothConfig {
    fn default() -> Self {
        Self {
            service_uuid: DEFAULT_SERVICE_UUID.to_string(),
            peer_name: DEFAULT_PEER_NAME.to_string(),
            mtu: DEFAULT_MTU,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            reconnect_max_attempts: DEFAULT_RECONNECT_MAX_ATTEMPTS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_constants() {
        let cfg = BluetoothConfig::default();
        assert_eq!(cfg.service_uuid, DEFAULT_SERVICE_UUID);
        assert_eq!(cfg.peer_name, DEFAULT_PEER_NAME);
        assert_eq!(cfg.mtu, DEFAULT_MTU);
        assert_eq!(cfg.connect_timeout_secs, DEFAULT_CONNECT_TIMEOUT_SECS);
        assert_eq!(cfg.reconnect_max_attempts, DEFAULT_RECONNECT_MAX_ATTEMPTS);
    }

    #[test]
    fn default_service_uuid_parses_as_uuid() {
        // Sanity check: the string we hand to `uuid!()` in the transport
        // crate is a valid UUID. Without this guard a typo here would only
        // explode much later in btleplug.
        let parsed = uuid::Uuid::parse_str(DEFAULT_SERVICE_UUID);
        assert!(parsed.is_ok(), "DEFAULT_SERVICE_UUID must parse as v4 UUID");
        assert_eq!(parsed.unwrap().get_version_num(), 4);
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = BluetoothConfig {
            service_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
            peer_name: "TestHost".to_string(),
            mtu: 244,
            connect_timeout_secs: 5,
            reconnect_max_attempts: 3,
        };
        let s = toml::to_string(&cfg).unwrap();
        let back: BluetoothConfig = toml::from_str(&s).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn empty_toml_yields_defaults() {
        // A `[bluetooth]` section omitted from config.toml should still
        // deserialize via `#[serde(default)]` on the parent struct that
        // embeds this type. Verify the inner fields take their defaults
        // when the section itself is empty.
        let back: BluetoothConfig = toml::from_str("").unwrap();
        assert_eq!(back, BluetoothConfig::default());
    }
}
