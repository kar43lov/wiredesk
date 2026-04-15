use std::sync::mpsc;

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::cobs;
use wiredesk_protocol::packet::Packet;

use crate::transport::Transport;

/// In-memory transport for testing. Uses mpsc channels.
pub struct MockTransport {
    tx: mpsc::Sender<Vec<u8>>,
    rx: mpsc::Receiver<Vec<u8>>,
    connected: bool,
}

impl MockTransport {
    /// Create a pair of connected transports (A↔B).
    pub fn pair() -> (Self, Self) {
        let (tx_a, rx_b) = mpsc::channel();
        let (tx_b, rx_a) = mpsc::channel();

        let a = Self { tx: tx_a, rx: rx_a, connected: true };
        let b = Self { tx: tx_b, rx: rx_b, connected: true };

        (a, b)
    }
}

impl Transport for MockTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        let raw = packet.to_bytes()?;
        let encoded = cobs::encode(&raw);
        self.tx
            .send(encoded)
            .map_err(|_| WireDeskError::Transport("mock channel closed".into()))
    }

    fn recv(&mut self) -> Result<Packet> {
        let encoded = self
            .rx
            .recv()
            .map_err(|_| WireDeskError::Transport("mock channel closed".into()))?;
        let raw = cobs::decode(&encoded)
            .map_err(|e| WireDeskError::Protocol(format!("COBS decode: {e}")))?;
        Packet::from_bytes(&raw)
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiredesk_protocol::message::Message;

    #[test]
    fn send_recv_hello() {
        let (mut a, mut b) = MockTransport::pair();
        let msg = Message::Hello { version: 1, client_name: "test".into() };
        let packet = Packet::new(msg.clone(), 1);

        a.send(&packet).unwrap();
        let received = b.recv().unwrap();

        assert_eq!(received.message, msg);
        assert_eq!(received.seq, 1);
    }

    #[test]
    fn send_recv_multiple() {
        let (mut a, mut b) = MockTransport::pair();

        let messages = vec![
            Message::Heartbeat,
            Message::MouseMove { x: 100, y: 200 },
            Message::KeyDown { scancode: 0x1E, modifiers: 0x01 },
            Message::ClipChunk { index: 0, data: vec![0, 1, 2, 255] },
        ];

        for (i, msg) in messages.iter().enumerate() {
            a.send(&Packet::new(msg.clone(), i as u16)).unwrap();
        }

        for (i, expected) in messages.iter().enumerate() {
            let received = b.recv().unwrap();
            assert_eq!(received.message, *expected);
            assert_eq!(received.seq, i as u16);
        }
    }

    #[test]
    fn bidirectional() {
        let (mut a, mut b) = MockTransport::pair();

        a.send(&Packet::new(Message::Hello { version: 1, client_name: "c".into() }, 0)).unwrap();
        let hello = b.recv().unwrap();
        assert!(matches!(hello.message, Message::Hello { .. }));

        b.send(&Packet::new(
            Message::HelloAck { version: 1, host_name: "h".into(), screen_w: 1920, screen_h: 1080 },
            1,
        )).unwrap();
        let ack = a.recv().unwrap();
        assert!(matches!(ack.message, Message::HelloAck { .. }));
    }

    #[test]
    fn is_connected() {
        let (a, _b) = MockTransport::pair();
        assert!(a.is_connected());
    }
}
