use std::io::{Read, Write};
use std::time::Duration;

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::cobs;
use wiredesk_protocol::packet::Packet;

use crate::transport::Transport;

pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    read_buf: Vec<u8>,
    partial_timeouts: u32,
}

const MAX_PARTIAL_TIMEOUTS: u32 = 50; // ~5 sec at 100ms timeout

impl SerialTransport {
    pub fn open(port_name: &str, baud_rate: u32) -> Result<Self> {
        let mut port = serialport::new(port_name, baud_rate)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(|e| WireDeskError::Transport(format!("serial open {port_name}: {e}")))?;

        // Many USB-UART chips (CH340, FTDI, etc.) emit a stray byte when DTR
        // toggles on open. Wait briefly for the line to settle, then drain
        // anything that arrived during that window so it doesn't get glued to
        // the first real frame and produce "bad magic" errors.
        std::thread::sleep(Duration::from_millis(100));
        let mut scratch = [0u8; 256];
        loop {
            match port.read(&mut scratch) {
                Ok(n) if n > 0 => {
                    log::debug!("serial open: drained {n} byte(s) of startup junk");
                    continue;
                }
                _ => break,
            }
        }

        Ok(Self {
            port,
            read_buf: Vec::with_capacity(1024),
            partial_timeouts: 0,
        })
    }
}

impl Transport for SerialTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        let raw = packet.to_bytes()?;
        let encoded = cobs::encode(&raw);
        // Leading 0x00 forces a frame boundary so any line noise preceding
        // this packet ends up in its own (invalid, ignored) frame instead of
        // getting concatenated with our payload.
        self.port
            .write_all(&[0x00])
            .map_err(|e| WireDeskError::Transport(format!("serial write: {e}")))?;
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
        // Note: read_buf may contain a partial frame from a previous timeout
        let mut byte_buf = [0u8; 1];

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
                    if self.read_buf.is_empty() {
                        return Err(WireDeskError::Transport("recv timeout".into()));
                    }
                    // Partial frame in buffer — retry, but not forever
                    self.partial_timeouts += 1;
                    if self.partial_timeouts > MAX_PARTIAL_TIMEOUTS {
                        log::warn!("partial frame abandoned after {} timeouts ({} bytes)",
                            self.partial_timeouts, self.read_buf.len());
                        self.read_buf.clear();
                        self.partial_timeouts = 0;
                        return Err(WireDeskError::Transport("recv timeout (partial frame abandoned)".into()));
                    }
                    continue;
                }
                Err(e) => return Err(WireDeskError::Transport(format!("serial read: {e}"))),
            }
        }

        self.partial_timeouts = 0;
        let raw = cobs::decode(&self.read_buf)
            .map_err(|e| WireDeskError::Protocol(format!("COBS decode: {e}")))?;
        self.read_buf.clear();
        Packet::from_bytes(&raw)
    }

    fn is_connected(&self) -> bool {
        true // Serial port is connected if open
    }

    fn name(&self) -> &'static str {
        "serial"
    }
}
