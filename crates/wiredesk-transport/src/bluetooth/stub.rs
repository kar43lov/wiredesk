//! Stub `BluetoothLeTransport` for unsupported platforms (anything but macOS / Windows).
//!
//! WireDesk targets Win11 host + macOS client by design (see CLAUDE.md). This
//! module exists only so that the crate keeps compiling on a Linux developer
//! machine running `cargo check --workspace`. `open()` always errors — there
//! is no platform support to fall back to.

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::packet::Packet;

use super::BluetoothFactoryConfig;
use crate::transport::Transport;

/// Placeholder — BLE is not supported on this platform.
#[derive(Debug)]
pub struct BluetoothLeTransport {
    _private: (),
}

impl BluetoothLeTransport {
    pub fn open(_cfg: &BluetoothFactoryConfig) -> Result<Self> {
        Err(WireDeskError::Transport(
            "BLE transport is only supported on macOS and Windows".to_string(),
        ))
    }
}

impl Transport for BluetoothLeTransport {
    fn send(&mut self, _packet: &Packet) -> Result<()> {
        Err(WireDeskError::Transport(
            "BLE not supported on this platform".to_string(),
        ))
    }

    fn recv(&mut self) -> Result<Packet> {
        Err(WireDeskError::Transport(
            "BLE not supported on this platform".to_string(),
        ))
    }

    fn is_connected(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "bluetooth-le-stub"
    }

    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        Err(WireDeskError::Transport(
            "BLE not supported on this platform".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_returns_unsupported_err() {
        let cfg = super::BluetoothFactoryConfig {
            service_uuid: "00000000-0000-0000-0000-000000000000".to_string(),
            peer_name: "Test".to_string(),
            mtu: 247,
            connect_timeout_secs: 2,
            reconnect_max_attempts: 0,
        };
        let result = BluetoothLeTransport::open(&cfg);
        assert!(result.is_err());
    }

    #[test]
    fn name_is_stable() {
        let t = BluetoothLeTransport { _private: () };
        assert_eq!(t.name(), "bluetooth-le-stub");
        assert!(!t.is_connected());
    }
}
