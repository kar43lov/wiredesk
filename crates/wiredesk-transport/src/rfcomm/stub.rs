//! Placeholder for platforms without an RFCOMM implementation (Linux CI).

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::packet::Packet;

use super::RfcommFactoryConfig;
use crate::transport::Transport;

#[derive(Debug)]
pub struct RfcommTransport;

impl RfcommTransport {
    pub fn open(_cfg: &RfcommFactoryConfig) -> Result<Self> {
        Err(WireDeskError::Transport(
            "RFCOMM transport is only available on macOS and Windows".into(),
        ))
    }
}

impl Transport for RfcommTransport {
    fn send(&mut self, _packet: &Packet) -> Result<()> {
        Err(WireDeskError::Transport("RFCOMM stub".into()))
    }

    fn recv(&mut self) -> Result<Packet> {
        Err(WireDeskError::Transport("RFCOMM stub".into()))
    }

    fn is_connected(&self) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "rfcomm-stub"
    }

    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        Err(WireDeskError::Transport("RFCOMM stub".into()))
    }
}
