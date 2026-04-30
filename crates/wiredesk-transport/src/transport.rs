use wiredesk_core::error::Result;
use wiredesk_protocol::packet::Packet;

pub trait Transport: Send {
    fn send(&mut self, packet: &Packet) -> Result<()>;
    fn recv(&mut self) -> Result<Packet>;
    fn is_connected(&self) -> bool;
    fn name(&self) -> &'static str;

    /// Create a separate handle to the same underlying channel for use in
    /// another thread (e.g., reader and writer halves). The new handle has
    /// its own decoder state — reads on one don't affect reads on the other.
    fn try_clone(&self) -> Result<Box<dyn Transport>>;
}

impl Transport for Box<dyn Transport> {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        (**self).send(packet)
    }
    fn recv(&mut self) -> Result<Packet> {
        (**self).recv()
    }
    fn is_connected(&self) -> bool {
        (**self).is_connected()
    }
    fn name(&self) -> &'static str {
        (**self).name()
    }
    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        (**self).try_clone()
    }
}
