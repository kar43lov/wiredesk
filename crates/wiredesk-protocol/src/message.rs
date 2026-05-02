use wiredesk_core::error::{Result, WireDeskError};

/// Protocol version
pub const VERSION: u8 = 1;

/// Clipboard payload formats for `Message::ClipOffer { format, .. }`.
///
/// Receivers MUST treat unknown values as opaque/skip, not as an error.
pub const FORMAT_TEXT_UTF8: u8 = 0;
pub const FORMAT_PNG_IMAGE: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    // Handshake
    Hello = 0x01,
    HelloAck = 0x02,
    // Input
    MouseMove = 0x10,
    MouseButton = 0x11,
    MouseScroll = 0x12,
    KeyDown = 0x13,
    KeyUp = 0x14,
    // Clipboard
    ClipOffer = 0x20,
    ClipChunk = 0x21,
    ClipAck = 0x22,
    // System
    Heartbeat = 0x30,
    Error = 0x31,
    Disconnect = 0x32,
    // Shell (terminal-over-serial)
    ShellOpen = 0x40,
    ShellInput = 0x41,
    ShellOutput = 0x42,
    ShellClose = 0x43,
    ShellExit = 0x44,
}

impl TryFrom<u8> for MessageType {
    type Error = WireDeskError;

    fn try_from(v: u8) -> Result<Self> {
        match v {
            0x01 => Ok(Self::Hello),
            0x02 => Ok(Self::HelloAck),
            0x10 => Ok(Self::MouseMove),
            0x11 => Ok(Self::MouseButton),
            0x12 => Ok(Self::MouseScroll),
            0x13 => Ok(Self::KeyDown),
            0x14 => Ok(Self::KeyUp),
            0x20 => Ok(Self::ClipOffer),
            0x21 => Ok(Self::ClipChunk),
            0x22 => Ok(Self::ClipAck),
            0x30 => Ok(Self::Heartbeat),
            0x31 => Ok(Self::Error),
            0x32 => Ok(Self::Disconnect),
            0x40 => Ok(Self::ShellOpen),
            0x41 => Ok(Self::ShellInput),
            0x42 => Ok(Self::ShellOutput),
            0x43 => Ok(Self::ShellClose),
            0x44 => Ok(Self::ShellExit),
            _ => Err(WireDeskError::Protocol(format!("unknown message type: 0x{v:02X}"))),
        }
    }
}

/// Payload variants for each message type.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello { version: u8, client_name: String },
    HelloAck { version: u8, host_name: String, screen_w: u16, screen_h: u16 },
    MouseMove { x: u16, y: u16 },
    MouseButton { button: u8, pressed: bool },
    MouseScroll { delta_x: i16, delta_y: i16 },
    KeyDown { scancode: u16, modifiers: u8 },
    KeyUp { scancode: u16, modifiers: u8 },
    ClipOffer { format: u8, total_len: u32 },
    ClipChunk { index: u16, data: Vec<u8> },
    ClipAck { index: u16 },
    Heartbeat,
    Error { code: u16, msg: String },
    Disconnect,
    ShellOpen { shell: String },           // "powershell", "cmd", "" for default
    ShellInput { data: Vec<u8> },          // bytes to write to shell stdin
    ShellOutput { data: Vec<u8> },         // bytes from shell stdout/stderr
    ShellClose,
    ShellExit { code: i32 },
}

impl Message {
    pub fn msg_type(&self) -> MessageType {
        match self {
            Self::Hello { .. } => MessageType::Hello,
            Self::HelloAck { .. } => MessageType::HelloAck,
            Self::MouseMove { .. } => MessageType::MouseMove,
            Self::MouseButton { .. } => MessageType::MouseButton,
            Self::MouseScroll { .. } => MessageType::MouseScroll,
            Self::KeyDown { .. } => MessageType::KeyDown,
            Self::KeyUp { .. } => MessageType::KeyUp,
            Self::ClipOffer { .. } => MessageType::ClipOffer,
            Self::ClipChunk { .. } => MessageType::ClipChunk,
            Self::ClipAck { .. } => MessageType::ClipAck,
            Self::Heartbeat => MessageType::Heartbeat,
            Self::Error { .. } => MessageType::Error,
            Self::Disconnect => MessageType::Disconnect,
            Self::ShellOpen { .. } => MessageType::ShellOpen,
            Self::ShellInput { .. } => MessageType::ShellInput,
            Self::ShellOutput { .. } => MessageType::ShellOutput,
            Self::ShellClose => MessageType::ShellClose,
            Self::ShellExit { .. } => MessageType::ShellExit,
        }
    }

    pub fn needs_ack(&self) -> bool {
        matches!(self, Self::ClipOffer { .. } | Self::ClipChunk { .. })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match self {
            Self::Hello { version, client_name } => {
                buf.push(*version);
                write_string(&mut buf, client_name, 32);
            }
            Self::HelloAck { version, host_name, screen_w, screen_h } => {
                buf.push(*version);
                write_string(&mut buf, host_name, 32);
                buf.extend_from_slice(&screen_w.to_le_bytes());
                buf.extend_from_slice(&screen_h.to_le_bytes());
            }
            Self::MouseMove { x, y } => {
                buf.extend_from_slice(&x.to_le_bytes());
                buf.extend_from_slice(&y.to_le_bytes());
            }
            Self::MouseButton { button, pressed } => {
                buf.push(*button);
                buf.push(*pressed as u8);
            }
            Self::MouseScroll { delta_x, delta_y } => {
                buf.extend_from_slice(&delta_x.to_le_bytes());
                buf.extend_from_slice(&delta_y.to_le_bytes());
            }
            Self::KeyDown { scancode, modifiers } | Self::KeyUp { scancode, modifiers } => {
                buf.extend_from_slice(&scancode.to_le_bytes());
                buf.push(*modifiers);
            }
            Self::ClipOffer { format, total_len } => {
                buf.push(*format);
                buf.extend_from_slice(&total_len.to_le_bytes());
            }
            Self::ClipChunk { index, data } => {
                buf.extend_from_slice(&index.to_le_bytes());
                buf.extend_from_slice(data);
            }
            Self::ClipAck { index } => {
                buf.extend_from_slice(&index.to_le_bytes());
            }
            Self::Heartbeat | Self::Disconnect | Self::ShellClose => {}
            Self::Error { code, msg } => {
                buf.extend_from_slice(&code.to_le_bytes());
                write_string(&mut buf, msg, 256);
            }
            Self::ShellOpen { shell } => {
                write_string(&mut buf, shell, 32);
            }
            Self::ShellInput { data } | Self::ShellOutput { data } => {
                buf.extend_from_slice(data);
            }
            Self::ShellExit { code } => {
                buf.extend_from_slice(&code.to_le_bytes());
            }
        }
        buf
    }

    pub fn deserialize(msg_type: MessageType, payload: &[u8]) -> Result<Self> {
        match msg_type {
            MessageType::Hello => {
                ensure_min_len(payload, 1 + 1)?; // version + at least 1 byte name len
                let version = payload[0];
                let client_name = read_string(&payload[1..])?;
                Ok(Self::Hello { version, client_name })
            }
            MessageType::HelloAck => {
                ensure_min_len(payload, 1 + 1 + 4)?;
                let version = payload[0];
                let (host_name, rest) = read_string_with_rest(&payload[1..])?;
                ensure_min_len(rest, 4)?;
                let screen_w = u16::from_le_bytes([rest[0], rest[1]]);
                let screen_h = u16::from_le_bytes([rest[2], rest[3]]);
                Ok(Self::HelloAck { version, host_name, screen_w, screen_h })
            }
            MessageType::MouseMove => {
                ensure_min_len(payload, 4)?;
                Ok(Self::MouseMove {
                    x: u16::from_le_bytes([payload[0], payload[1]]),
                    y: u16::from_le_bytes([payload[2], payload[3]]),
                })
            }
            MessageType::MouseButton => {
                ensure_min_len(payload, 2)?;
                Ok(Self::MouseButton { button: payload[0], pressed: payload[1] != 0 })
            }
            MessageType::MouseScroll => {
                ensure_min_len(payload, 4)?;
                Ok(Self::MouseScroll {
                    delta_x: i16::from_le_bytes([payload[0], payload[1]]),
                    delta_y: i16::from_le_bytes([payload[2], payload[3]]),
                })
            }
            MessageType::KeyDown => {
                ensure_min_len(payload, 3)?;
                Ok(Self::KeyDown {
                    scancode: u16::from_le_bytes([payload[0], payload[1]]),
                    modifiers: payload[2],
                })
            }
            MessageType::KeyUp => {
                ensure_min_len(payload, 3)?;
                Ok(Self::KeyUp {
                    scancode: u16::from_le_bytes([payload[0], payload[1]]),
                    modifiers: payload[2],
                })
            }
            MessageType::ClipOffer => {
                ensure_min_len(payload, 5)?;
                Ok(Self::ClipOffer {
                    format: payload[0],
                    total_len: u32::from_le_bytes([payload[1], payload[2], payload[3], payload[4]]),
                })
            }
            MessageType::ClipChunk => {
                ensure_min_len(payload, 2)?;
                Ok(Self::ClipChunk {
                    index: u16::from_le_bytes([payload[0], payload[1]]),
                    data: payload[2..].to_vec(),
                })
            }
            MessageType::ClipAck => {
                ensure_min_len(payload, 2)?;
                Ok(Self::ClipAck {
                    index: u16::from_le_bytes([payload[0], payload[1]]),
                })
            }
            MessageType::Heartbeat => Ok(Self::Heartbeat),
            MessageType::Disconnect => Ok(Self::Disconnect),
            MessageType::Error => {
                ensure_min_len(payload, 2 + 1)?;
                let code = u16::from_le_bytes([payload[0], payload[1]]);
                let msg = read_string(&payload[2..])?;
                Ok(Self::Error { code, msg })
            }
            MessageType::ShellOpen => {
                let shell = if payload.is_empty() {
                    String::new()
                } else {
                    read_string(payload)?
                };
                Ok(Self::ShellOpen { shell })
            }
            MessageType::ShellInput => Ok(Self::ShellInput { data: payload.to_vec() }),
            MessageType::ShellOutput => Ok(Self::ShellOutput { data: payload.to_vec() }),
            MessageType::ShellClose => Ok(Self::ShellClose),
            MessageType::ShellExit => {
                ensure_min_len(payload, 4)?;
                let code = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                Ok(Self::ShellExit { code })
            }
        }
    }
}

fn write_string(buf: &mut Vec<u8>, s: &str, max_len: usize) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(max_len).min(255);
    buf.push(len as u8);
    buf.extend_from_slice(&bytes[..len]);
}

fn read_string(data: &[u8]) -> Result<String> {
    if data.is_empty() {
        return Err(WireDeskError::Protocol("missing string length".into()));
    }
    let len = data[0] as usize;
    if data.len() < 1 + len {
        return Err(WireDeskError::Protocol("string truncated".into()));
    }
    String::from_utf8(data[1..1 + len].to_vec())
        .map_err(|e| WireDeskError::Protocol(format!("invalid utf8: {e}")))
}

fn read_string_with_rest(data: &[u8]) -> Result<(String, &[u8])> {
    if data.is_empty() {
        return Err(WireDeskError::Protocol("missing string length".into()));
    }
    let len = data[0] as usize;
    if data.len() < 1 + len {
        return Err(WireDeskError::Protocol("string truncated".into()));
    }
    let s = String::from_utf8(data[1..1 + len].to_vec())
        .map_err(|e| WireDeskError::Protocol(format!("invalid utf8: {e}")))?;
    Ok((s, &data[1 + len..]))
}

fn ensure_min_len(data: &[u8], min: usize) -> Result<()> {
    if data.len() < min {
        Err(WireDeskError::Protocol(format!(
            "payload too short: {} < {}",
            data.len(),
            min
        )))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: &Message) {
        let payload = msg.serialize();
        let parsed = Message::deserialize(msg.msg_type(), &payload).unwrap();
        assert_eq!(*msg, parsed);
    }

    #[test]
    fn roundtrip_hello() {
        roundtrip(&Message::Hello { version: 1, client_name: "test-client".into() });
    }

    #[test]
    fn roundtrip_hello_ack() {
        roundtrip(&Message::HelloAck {
            version: 1,
            host_name: "test-host".into(),
            screen_w: 1920,
            screen_h: 1080,
        });
    }

    #[test]
    fn roundtrip_mouse_move() {
        roundtrip(&Message::MouseMove { x: 32000, y: 16000 });
    }

    #[test]
    fn roundtrip_mouse_button() {
        roundtrip(&Message::MouseButton { button: 0, pressed: true });
        roundtrip(&Message::MouseButton { button: 2, pressed: false });
    }

    #[test]
    fn roundtrip_mouse_scroll() {
        roundtrip(&Message::MouseScroll { delta_x: -120, delta_y: 240 });
    }

    #[test]
    fn roundtrip_key_down() {
        roundtrip(&Message::KeyDown { scancode: 0x1E, modifiers: 0x03 });
    }

    #[test]
    fn roundtrip_key_up() {
        roundtrip(&Message::KeyUp { scancode: 0x1E, modifiers: 0 });
    }

    #[test]
    fn roundtrip_clip_offer() {
        roundtrip(&Message::ClipOffer { format: 1, total_len: 65536 });
    }

    #[test]
    fn roundtrip_clip_offer_text() {
        // Regression: text format (format=0) wire-format unchanged.
        roundtrip(&Message::ClipOffer { format: FORMAT_TEXT_UTF8, total_len: 1024 });
    }

    #[test]
    fn roundtrip_clip_offer_image() {
        roundtrip(&Message::ClipOffer { format: FORMAT_PNG_IMAGE, total_len: 245_760 });
    }

    #[test]
    fn clip_format_constants_are_distinct() {
        assert_eq!(FORMAT_TEXT_UTF8, 0);
        assert_eq!(FORMAT_PNG_IMAGE, 1);
        assert_ne!(FORMAT_TEXT_UTF8, FORMAT_PNG_IMAGE);
    }

    #[test]
    fn roundtrip_clip_chunk() {
        roundtrip(&Message::ClipChunk { index: 42, data: vec![1, 2, 3, 0, 255] });
    }

    #[test]
    fn roundtrip_clip_ack() {
        roundtrip(&Message::ClipAck { index: 42 });
    }

    #[test]
    fn roundtrip_heartbeat() {
        roundtrip(&Message::Heartbeat);
    }

    #[test]
    fn roundtrip_disconnect() {
        roundtrip(&Message::Disconnect);
    }

    #[test]
    fn roundtrip_error() {
        roundtrip(&Message::Error { code: 500, msg: "something broke".into() });
    }

    #[test]
    fn roundtrip_shell_open_default() {
        roundtrip(&Message::ShellOpen { shell: String::new() });
    }

    #[test]
    fn roundtrip_shell_open_powershell() {
        roundtrip(&Message::ShellOpen { shell: "powershell".into() });
    }

    #[test]
    fn roundtrip_shell_input() {
        roundtrip(&Message::ShellInput { data: b"ls -la\n".to_vec() });
    }

    #[test]
    fn roundtrip_shell_output() {
        // Output may contain arbitrary bytes including 0x00
        roundtrip(&Message::ShellOutput { data: vec![0, 1, 0xFF, b'x', b'y'] });
    }

    #[test]
    fn roundtrip_shell_close() {
        roundtrip(&Message::ShellClose);
    }

    #[test]
    fn roundtrip_shell_exit() {
        roundtrip(&Message::ShellExit { code: 0 });
        roundtrip(&Message::ShellExit { code: -1 });
        roundtrip(&Message::ShellExit { code: 130 });
    }

    #[test]
    fn unknown_message_type() {
        assert!(MessageType::try_from(0xFF).is_err());
    }

    #[test]
    fn truncated_payload() {
        assert!(Message::deserialize(MessageType::MouseMove, &[0x01]).is_err());
    }
}
