use wiredesk_core::error::Result;
use wiredesk_protocol::packet::Packet;

pub trait Transport: Send {
    fn send(&mut self, packet: &Packet) -> Result<()>;
    fn recv(&mut self) -> Result<Packet>;
    fn is_connected(&self) -> bool;
    fn name(&self) -> &'static str;
}

/// Blanket impl for Box<dyn Transport> so it can be used behind Arc<Mutex<>>.
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
}
