//! Bluetooth Classic transport — RFCOMM (the Serial Port Profile, SPP).
//!
//! Second Bluetooth option next to the BLE GATT transport in [`crate::bluetooth`].
//! Live-measured 2026-09-11 on Mac M4 ↔ Win11 (Intel 8265, BT 4.2):
//! **~120 KB/s each direction, ~10 ms round-trip** — against ~4–5 KB/s for
//! BLE on the same pair, because Classic EDR moves data in 2–3 Mbit/s
//! ACL packets while BLE on a 4.2 adapter is stuck on the 1M PHY with
//! per-connection-event pacing.
//!
//! Roles are fixed by the OS APIs, mirroring BLE:
//! - **Win host = server.** A Winsock `AF_BTH` socket listens on an RFCOMM
//!   channel and publishes an SDP record under `service_uuid` so the client
//!   can find the channel number ([`win`]).
//! - **Mac client = client.** IOBluetooth queries the host's SDP server for
//!   `service_uuid`, opens the channel and streams bytes ([`mac`]).
//! - **Win client = client** too, over the same Winsock API — unlike BLE,
//!   nothing stops a Windows client from reaching a Windows host.
//!
//! Framing on the wire is the COBS stream of `SerialTransport`
//! ([`crate::framing`]), so a lone `0x00` is a legal idle keepalive: it
//! keeps the ACL link out of sniff mode ([`common::IdleKeepalive`]).

pub mod common;

/// Runtime configuration handed to `RfcommTransport::open`. Built by the
/// apps from `wiredesk_core::RfcommConfig` plus the role the app plays.
#[derive(Clone, Debug)]
pub struct RfcommFactoryConfig {
    /// SDP service-class UUID (string form) shared by both peers.
    pub service_uuid: String,
    /// Client: Bluetooth address of the host, empty = any paired device.
    pub peer_address: String,
    /// 1..=30 = fixed channel, no SDP (the default; macOS cannot read the
    /// SDP record the Windows host publishes). 0 = SDP-assigned / looked-up.
    pub channel: u8,
    /// Client: SDP lookup + connect budget.
    pub connect_timeout_secs: u32,
    /// Idle keepalive period, 0 = off.
    pub keepalive_ms: u32,
    /// Host: require an authenticated + encrypted link.
    pub require_encryption: bool,
    /// Which end of the link this process is.
    pub role: RfcommRole,
}

/// Which side of the RFCOMM link this process plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RfcommRole {
    /// Publish the service and wait for a client (the host).
    Listen,
    /// Find the host's service and connect to it (the client).
    Connect,
}

#[cfg(target_os = "macos")]
mod mac;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod stub;
#[cfg(target_os = "windows")]
mod win;

#[cfg(target_os = "macos")]
pub use mac::{request_bluetooth_permission, RfcommTransport};
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use stub::RfcommTransport;
#[cfg(target_os = "windows")]
pub use win::RfcommTransport;
