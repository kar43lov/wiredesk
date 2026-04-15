use wiredesk_core::error::{Result, WireDeskError};

use crate::crc;
use crate::message::{Message, MessageType};

/// Magic bytes: "WD"
const MAGIC: [u8; 2] = [0x57, 0x44];

/// Header size: magic(2) + type(1) + flags(1) + seq(2) + len(2) = 8
const HEADER_SIZE: usize = 8;

/// CRC size
const CRC_SIZE: usize = 2;

/// Max payload
pub const MAX_PAYLOAD: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketFlags(u8);

impl PacketFlags {
    pub const NONE: Self = Self(0);
    pub const ACK_REQUIRED: Self = Self(0x01);

    pub fn from_message(msg: &Message) -> Self {
        if msg.needs_ack() {
            Self::ACK_REQUIRED
        } else {
            Self::NONE
        }
    }

    pub fn bits(self) -> u8 {
        self.0
    }
}

/// A fully formed packet ready for COBS framing and transmission.
#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub msg_type: MessageType,
    pub flags: PacketFlags,
    pub seq: u16,
    pub message: Message,
}

impl Packet {
    pub fn new(message: Message, seq: u16) -> Self {
        let flags = PacketFlags::from_message(&message);
        Self {
            msg_type: message.msg_type(),
            flags,
            seq,
            message,
        }
    }

    /// Serialize to raw bytes (before COBS framing).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let payload = self.message.serialize();
        if payload.len() > MAX_PAYLOAD {
            return Err(WireDeskError::Protocol(format!(
                "payload too large: {} > {}",
                payload.len(),
                MAX_PAYLOAD
            )));
        }

        let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len() + CRC_SIZE);
        // Header
        buf.extend_from_slice(&MAGIC);
        buf.push(self.msg_type as u8);
        buf.push(self.flags.bits());
        buf.extend_from_slice(&self.seq.to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        // Payload
        buf.extend_from_slice(&payload);
        // CRC over header + payload
        let checksum = crc::compute(&buf);
        buf.extend_from_slice(&checksum.to_le_bytes());

        Ok(buf)
    }

    /// Deserialize from raw bytes (after COBS decoding).
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE + CRC_SIZE {
            return Err(WireDeskError::Protocol(format!(
                "packet too short: {} bytes",
                data.len()
            )));
        }

        // Verify magic
        if data[0..2] != MAGIC {
            return Err(WireDeskError::Protocol(format!(
                "bad magic: {:02X}{:02X}",
                data[0], data[1]
            )));
        }

        let msg_type = MessageType::try_from(data[2])?;
        let flags = PacketFlags(data[3]);
        let seq = u16::from_le_bytes([data[4], data[5]]);
        let payload_len = u16::from_le_bytes([data[6], data[7]]) as usize;

        let expected_total = HEADER_SIZE + payload_len + CRC_SIZE;
        if data.len() < expected_total {
            return Err(WireDeskError::Protocol(format!(
                "packet truncated: {} < {}",
                data.len(),
                expected_total
            )));
        }

        // Verify CRC
        let crc_offset = HEADER_SIZE + payload_len;
        let received_crc = u16::from_le_bytes([data[crc_offset], data[crc_offset + 1]]);
        let computed_crc = crc::compute(&data[..crc_offset]);
        if received_crc != computed_crc {
            return Err(WireDeskError::Protocol(format!(
                "CRC mismatch: received 0x{received_crc:04X}, computed 0x{computed_crc:04X}"
            )));
        }

        let payload = &data[HEADER_SIZE..HEADER_SIZE + payload_len];
        let message = Message::deserialize(msg_type, payload)?;

        Ok(Self {
            msg_type,
            flags,
            seq,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cobs;

    fn full_roundtrip(msg: Message) {
        let packet = Packet::new(msg.clone(), 42);
        let raw = packet.to_bytes().unwrap();
        let encoded = cobs::encode(&raw);

        // Encoded should have no internal zeros
        for &b in &encoded[..encoded.len() - 1] {
            assert_ne!(b, 0);
        }

        let decoded = cobs::decode(&encoded).unwrap();
        let parsed = Packet::from_bytes(&decoded).unwrap();

        assert_eq!(parsed.message, msg);
        assert_eq!(parsed.seq, 42);
    }

    #[test]
    fn full_roundtrip_hello() {
        full_roundtrip(Message::Hello { version: 1, client_name: "mac".into() });
    }

    #[test]
    fn full_roundtrip_hello_ack() {
        full_roundtrip(Message::HelloAck {
            version: 1,
            host_name: "win".into(),
            screen_w: 2560,
            screen_h: 1440,
        });
    }

    #[test]
    fn full_roundtrip_mouse_move() {
        full_roundtrip(Message::MouseMove { x: 0, y: 65535 });
    }

    #[test]
    fn full_roundtrip_key() {
        full_roundtrip(Message::KeyDown { scancode: 0x1E, modifiers: 0x05 });
    }

    #[test]
    fn full_roundtrip_clipboard_chunk() {
        let data: Vec<u8> = (0..256).map(|i| i as u8).collect();
        full_roundtrip(Message::ClipChunk { index: 0, data });
    }

    #[test]
    fn full_roundtrip_heartbeat() {
        full_roundtrip(Message::Heartbeat);
    }

    #[test]
    fn bad_magic() {
        let mut raw = Packet::new(Message::Heartbeat, 0).to_bytes().unwrap();
        raw[0] = 0xFF;
        assert!(Packet::from_bytes(&raw).is_err());
    }

    #[test]
    fn bad_crc() {
        let mut raw = Packet::new(Message::Heartbeat, 0).to_bytes().unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0xFF;
        assert!(Packet::from_bytes(&raw).is_err());
    }

    #[test]
    fn truncated_packet() {
        let raw = Packet::new(Message::Heartbeat, 0).to_bytes().unwrap();
        assert!(Packet::from_bytes(&raw[..5]).is_err());
    }

    #[test]
    fn max_payload() {
        // ClipChunk payload = 2 bytes index + data, so max data = MAX_PAYLOAD - 2
        let data = vec![0xAA; MAX_PAYLOAD - 2];
        let msg = Message::ClipChunk { index: 0, data };
        let packet = Packet::new(msg, 0);
        assert!(packet.to_bytes().is_ok());
    }

    #[test]
    fn over_max_payload() {
        let data = vec![0xAA; MAX_PAYLOAD];
        let msg = Message::ClipChunk { index: 0, data };
        let packet = Packet::new(msg, 0);
        assert!(packet.to_bytes().is_err());
    }
}
