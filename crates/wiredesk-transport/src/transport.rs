use wiredesk_core::error::Result;
use wiredesk_protocol::packet::Packet;

pub trait Transport: Send {
    fn send(&mut self, packet: &Packet) -> Result<()>;
    fn recv(&mut self) -> Result<Packet>;
    fn is_connected(&self) -> bool;
    fn name(&self) -> &'static str;

    /// Whether `send` returns once the OS has *queued* the bytes rather than
    /// once the link has carried them.
    ///
    /// Serial writes block on the UART, so a sender that paces its writes
    /// paces the wire. RFCOMM and BLE hand data to the Bluetooth stack, which
    /// takes tens of KB before a write blocks — a sender pacing its writes
    /// there only paces how fast that hidden queue fills, and anything urgent
    /// written after it waits for the whole queue. Such a link needs feedback
    /// from the receiving side to be paced at all (see `RxProgress`).
    fn buffers_sends(&self) -> bool {
        false
    }

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
    fn buffers_sends(&self) -> bool {
        (**self).buffers_sends()
    }
    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        (**self).try_clone()
    }
}
