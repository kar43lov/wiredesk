use std::io::{Read, Write};
use std::time::Duration;

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::cobs;
use wiredesk_protocol::packet::Packet;

use crate::transport::Transport;

pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    read_buf: Vec<u8>,
}

impl SerialTransport {
    pub fn open(port_name: &str, baud_rate: u32) -> Result<Self> {
        let port = serialport::new(port_name, baud_rate)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| WireDeskError::Transport(format!("serial open {port_name}: {e}")))?;

        Ok(Self {
            port,
            read_buf: Vec::with_capacity(1024),
        })
    }
}

impl Transport for SerialTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        let raw = packet.to_bytes()?;
        let encoded = cobs::encode(&raw);
        self.port
            .write_all(&encoded)
            .map_err(|e| WireDeskError::Transport(format!("serial write: {e}")))?;
        self.port
            .flush()
            .map_err(|e| WireDeskError::Transport(format!("serial flush: {e}")))?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Packet> {
        // Read until we find a 0x00 delimiter (COBS frame boundary)
        let mut byte_buf = [0u8; 1];
        self.read_buf.clear();

        loop {
            match self.port.read(&mut byte_buf) {
                Ok(1) => {
                    if byte_buf[0] == 0x00 {
                        if self.read_buf.is_empty() {
                            // Skip leading delimiters
                            continue;
                        }
                        // Add delimiter back for COBS decode
                        self.read_buf.push(0x00);
                        break;
                    }
                    self.read_buf.push(byte_buf[0]);

                    if self.read_buf.len() > 1024 {
                        // Discard and skip to next delimiter
                        self.read_buf.clear();
                        loop {
                            match self.port.read(&mut byte_buf) {
                                Ok(1) if byte_buf[0] == 0x00 => break,
                                Ok(_) => continue,
                                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
                                Err(_) => break,
                            }
                        }
                        return Err(WireDeskError::Protocol("frame too large".into()));
                    }
                }
                Ok(_) => continue,
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Err(WireDeskError::Transport("recv timeout".into()));
                }
                Err(e) => return Err(WireDeskError::Transport(format!("serial read: {e}"))),
            }
        }

        let raw = cobs::decode(&self.read_buf)
            .map_err(|e| WireDeskError::Protocol(format!("COBS decode: {e}")))?;
        Packet::from_bytes(&raw)
    }

    fn is_connected(&self) -> bool {
        true // Serial port is connected if open
    }

    fn name(&self) -> &'static str {
        "serial"
    }
}
