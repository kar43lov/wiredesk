//! macOS BLE Central implementation. Substantive btleplug wiring lands in
//! Task 5 of `docs/plans/20260506-bluetooth-le-transport.md`. For now this
//! module exposes the same struct + config shape as the stub so the crate
//! compiles on macOS while the real implementation is in flight.

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::packet::Packet;

use super::BluetoothFactoryConfig;
use crate::transport::Transport;

#[derive(Debug)]
pub struct BluetoothLeTransport {
    _private: (),
}

impl BluetoothLeTransport {
    pub fn open(_cfg: &BluetoothFactoryConfig) -> Result<Self> {
        Err(WireDeskError::Transport(
            "BLE Central impl pending (Task 5)".to_string(),
        ))
    }
}

impl Transport for BluetoothLeTransport {
    fn send(&mut self, _packet: &Packet) -> Result<()> {
        unimplemented!("BLE send pending Task 5")
    }

    fn recv(&mut self) -> Result<Packet> {
        unimplemented!("BLE recv pending Task 5")
    }

    fn is_connected(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "bluetooth-le-central"
    }

    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        Err(WireDeskError::Transport(
            "BLE try_clone pending Task 5".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_currently_errors_with_pending_message() {
        let cfg = super::BluetoothFactoryConfig {
            service_uuid: "00000000-0000-0000-0000-000000000000".to_string(),
            peer_name: "Test".to_string(),
            mtu: 247,
            connect_timeout_secs: 2,
            reconnect_max_attempts: 0,
        };
        let err = BluetoothLeTransport::open(&cfg).unwrap_err();
        assert!(err.to_string().contains("Task 5"));
    }

    #[test]
    fn name_is_stable() {
        let t = BluetoothLeTransport { _private: () };
        assert_eq!(t.name(), "bluetooth-le-central");
    }
}
