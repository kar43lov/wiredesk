//! Shared configuration for the Bluetooth Classic (RFCOMM / SPP) transport.
//!
//! Lives in `wiredesk-core` for the same reason `BluetoothConfig` does: the
//! `service_uuid` must be identical on both peers — the host registers an
//! SDP record under it, the client asks the host's SDP server for that
//! record to learn the RFCOMM channel number. A drift between the two ends
//! means the client never finds the service.

use serde::{Deserialize, Serialize};

/// Default 128-bit SDP service-class UUID of the WireDesk RFCOMM service.
/// Distinct from the BLE GATT service UUID on purpose — the two transports
/// are independent and may both be published by the same host.
pub const DEFAULT_SERVICE_UUID: &str = "3d2df5cf-4f32-40c5-ab30-f1ccd6925b60";

/// SDP service instance name attached to the host's record. Advisory only:
/// the client matches by UUID, the name is what shows up in Bluetooth
/// browsers (macOS Bluetooth Explorer, `sdptool`).
pub const DEFAULT_SERVICE_NAME: &str = "WireDesk";

/// Fixed RFCOMM channel used by both peers, skipping SDP entirely.
///
/// 0 would mean "host lets the OS pick a free channel and publishes it via
/// SDP, client looks it up" — the textbook arrangement, and the one that
/// does not work here. Live 2026-09-11 (Mac M4 / macOS 26 ↔ Win11): the
/// record the host registers with `WSASetServiceW` is invisible from the
/// Mac. `IOBluetoothDevice.performSDPQuery(_:uuids:)` never calls back at
/// all, and the plain `performSDPQuery(_:)` returns instantly from a cache
/// filled at pairing time, listing the machine's stock records (CDP, A2DP,
/// AVRCP…) and nothing of ours. A fixed channel sidesteps both ends of
/// that: the host binds it, the client dials it.
///
/// 20 is inside the RFCOMM range (1..=30) and clear of the low numbers
/// Windows hands out to its own services (1–6 were taken on the live host).
/// Setting 0 restores the SDP path for a peer whose SDP server does work.
pub const DEFAULT_CHANNEL: u8 = 20;

/// How long the client waits for SDP lookup + RFCOMM connect before
/// erroring out. Live-measured connect is ~0.5 s; the budget covers a host
/// that is still coming out of sleep.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u32 = 15;

/// Idle keepalive period in milliseconds. Bluetooth Classic drops an idle
/// ACL link into sniff mode after ~1–2 s, and the first packet after that
/// pays ~80 ms to wake it (measured Mac M4 ↔ Intel 8265, 2026-09-11: 21 ms
/// after 1 s idle, 82 ms after 3 s). A single 0x00 byte — a COBS frame
/// delimiter the receiver skips as empty — every 500 ms keeps the link
/// active so mouse and keyboard stay at the ~10 ms round-trip. 0 = off.
pub const DEFAULT_KEEPALIVE_MS: u32 = 500;

/// Require an authenticated (paired) and encrypted link before accepting a
/// client on the host. Same rationale as the BLE `require_encryption`: the
/// wire protocol authenticates nobody, so pairing is the only gate.
pub const DEFAULT_REQUIRE_ENCRYPTION: bool = true;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct RfcommConfig {
    /// SDP service-class UUID — must be identical on both peers.
    pub service_uuid: String,

    /// Client only: Bluetooth address of the host, `"A0:B1:C2:D3:E4:F5"`
    /// style. Empty = try every paired device (computers first) and use
    /// the first one whose SDP server knows `service_uuid`.
    pub peer_address: String,

    /// RFCOMM channel, 1..=30. 0 = SDP-assigned (host) / SDP-looked-up
    /// (client), which macOS cannot resolve — see [`DEFAULT_CHANNEL`].
    pub channel: u8,

    /// Client only: SDP lookup + connect budget in seconds.
    pub connect_timeout_secs: u32,

    /// Idle keepalive period in milliseconds, 0 = off. See
    /// [`DEFAULT_KEEPALIVE_MS`].
    pub keepalive_ms: u32,

    /// Host only: demand an authenticated + encrypted link (pairing).
    pub require_encryption: bool,
}

impl Default for RfcommConfig {
    fn default() -> Self {
        Self {
            service_uuid: DEFAULT_SERVICE_UUID.to_string(),
            peer_address: String::new(),
            channel: DEFAULT_CHANNEL,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            keepalive_ms: DEFAULT_KEEPALIVE_MS,
            require_encryption: DEFAULT_REQUIRE_ENCRYPTION,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_constants() {
        let c = RfcommConfig::default();
        assert_eq!(c.service_uuid, DEFAULT_SERVICE_UUID);
        assert!(c.peer_address.is_empty());
        assert_eq!(c.channel, DEFAULT_CHANNEL);
        assert_eq!(c.connect_timeout_secs, DEFAULT_CONNECT_TIMEOUT_SECS);
        assert_eq!(c.keepalive_ms, DEFAULT_KEEPALIVE_MS);
        assert_eq!(c.require_encryption, DEFAULT_REQUIRE_ENCRYPTION);
    }

    #[test]
    fn default_service_uuid_parses_and_differs_from_ble() {
        let u = uuid::Uuid::parse_str(DEFAULT_SERVICE_UUID).expect("valid uuid");
        assert_eq!(u.get_version_num(), 4);
        assert_ne!(
            DEFAULT_SERVICE_UUID,
            crate::bluetooth_config::DEFAULT_SERVICE_UUID
        );
    }

    #[test]
    fn toml_roundtrip() {
        let c = RfcommConfig {
            peer_address: "A0:B1:C2:D3:E4:F5".to_string(),
            channel: 20,
            keepalive_ms: 0,
            ..Default::default()
        };
        let s = toml::to_string(&c).expect("serialize");
        let back: RfcommConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(back, c);
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let c: RfcommConfig = toml::from_str("").expect("empty table");
        assert_eq!(c, RfcommConfig::default());
    }
}
