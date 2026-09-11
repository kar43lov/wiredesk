//! Byte-stream framing shared by stream-shaped transports (RFCOMM today).
//!
//! Wire format is identical to `SerialTransport`: every packet is COBS-
//! encoded and terminated by a `0x00` delimiter, with a leading `0x00` in
//! front of each packet so line noise before it lands in its own (empty,
//! ignored) frame. Empty frames — consecutive delimiters — are skipped,
//! which is also what makes a lone `0x00` a valid idle keepalive.
//!
//! Unlike the serial reader, which pulls one byte per syscall, this reader
//! is fed whole `read()` buffers and hands back complete frames; the
//! stream transports read in 4–8 KB blocks at 100+ KB/s.

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::cobs;
use wiredesk_protocol::packet::Packet;

/// Hard frame-size limit before the reader discards and resyncs. Same
/// value as `SerialTransport`: `MAX_PAYLOAD` (4096) + header + CRC + COBS
/// overhead with margin.
pub const MAX_FRAME_SIZE: usize = 8192;

/// Encode one packet for the wire: `[0x00] [COBS(packet) incl. its trailing
/// 0x00 delimiter]` — byte-identical to what `SerialTransport::send` writes.
pub fn encode_frame(packet: &Packet) -> Result<Vec<u8>> {
    let raw = packet.to_bytes()?;
    let encoded = cobs::encode(&raw);
    let mut buf = Vec::with_capacity(encoded.len() + 1);
    buf.push(0x00);
    buf.extend_from_slice(&encoded);
    Ok(buf)
}

/// Incremental frame extractor. Feed it whatever the stream yields; pull
/// decoded packets with [`FrameReader::next_packet`].
#[derive(Debug, Default)]
pub struct FrameReader {
    /// Bytes of the frame currently being assembled (no delimiter).
    partial: Vec<u8>,
    /// `true` while skipping to the next delimiter after an oversize frame.
    resyncing: bool,
    /// Complete frames (with trailing delimiter, ready for COBS decode)
    /// not yet handed out.
    ready: std::collections::VecDeque<Vec<u8>>,
    /// Number of oversize frames discarded since the last `next_packet`
    /// that returned one — surfaced as a `Protocol` error so the storm
    /// detector sees it, then reset.
    oversize_dropped: u32,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Absorb a block of stream bytes.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if self.resyncing {
                if b == 0x00 {
                    self.resyncing = false;
                }
                continue;
            }
            if b == 0x00 {
                if !self.partial.is_empty() {
                    let mut frame = std::mem::take(&mut self.partial);
                    frame.push(0x00);
                    self.ready.push_back(frame);
                }
                // Empty frame (leading delimiter or keepalive) — skip.
                continue;
            }
            self.partial.push(b);
            if self.partial.len() > MAX_FRAME_SIZE {
                self.partial.clear();
                self.resyncing = true;
                self.oversize_dropped += 1;
            }
        }
    }

    /// `true` if a frame has been assembled but not yet returned.
    pub fn has_frame(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Bytes of the frame currently being assembled — for the caller's
    /// partial-frame timeout bookkeeping.
    pub fn partial_len(&self) -> usize {
        self.partial.len()
    }

    /// Drop a half-assembled frame (partial-frame timeout).
    pub fn abandon_partial(&mut self) {
        self.partial.clear();
    }

    /// Pop the next complete frame and decode it. `Ok(None)` when nothing
    /// is ready; `Err(Protocol)` for a frame that failed COBS/packet parse
    /// or for an oversize frame that was discarded during `feed`.
    pub fn next_packet(&mut self) -> Result<Option<Packet>> {
        if self.oversize_dropped > 0 {
            let n = self.oversize_dropped;
            self.oversize_dropped = 0;
            return Err(WireDeskError::Protocol(format!(
                "frame too large ({n} discarded)"
            )));
        }
        let Some(frame) = self.ready.pop_front() else {
            return Ok(None);
        };
        let raw = cobs::decode(&frame)
            .map_err(|e| WireDeskError::Protocol(format!("COBS decode: {e}")))?;
        Packet::from_bytes(&raw).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiredesk_protocol::message::Message;

    fn pkt(seq: u16) -> Packet {
        Packet::new(Message::Heartbeat, seq)
    }

    #[test]
    fn roundtrip_single_frame() {
        let mut r = FrameReader::new();
        let wire = encode_frame(&pkt(7)).unwrap();
        assert_eq!(wire.first(), Some(&0x00));
        assert_eq!(
            wire.last(),
            Some(&0x00),
            "COBS encode carries the delimiter"
        );
        r.feed(&wire[..wire.len() - 1]);
        assert!(!r.has_frame(), "no delimiter yet");
        r.feed(&wire[wire.len() - 1..]);
        assert!(r.has_frame());
        let p = r.next_packet().unwrap().expect("frame");
        assert_eq!(p.seq, 7);
        assert!(r.next_packet().unwrap().is_none());
    }

    #[test]
    fn frames_split_across_arbitrary_reads() {
        let mut wire = Vec::new();
        for s in 0..5u16 {
            wire.extend(encode_frame(&pkt(s)).unwrap());
        }
        let mut r = FrameReader::new();
        for chunk in wire.chunks(3) {
            r.feed(chunk);
        }
        let mut seqs = Vec::new();
        while let Some(p) = r.next_packet().unwrap() {
            seqs.push(p.seq);
        }
        assert_eq!(seqs, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn keepalive_zeros_between_frames_are_ignored() {
        let mut r = FrameReader::new();
        r.feed(&[0x00, 0x00, 0x00]);
        r.feed(&encode_frame(&pkt(1)).unwrap());
        r.feed(&[0x00, 0x00]);
        assert_eq!(r.next_packet().unwrap().unwrap().seq, 1);
        r.feed(&[0x00]);
        assert!(r.next_packet().unwrap().is_none());
    }

    #[test]
    fn oversize_frame_is_dropped_and_reported_once() {
        let mut r = FrameReader::new();
        r.feed(&vec![0x01; MAX_FRAME_SIZE + 10]);
        r.feed(&[0x00]);
        r.feed(&encode_frame(&pkt(3)).unwrap());
        let err = r.next_packet().unwrap_err().to_string();
        assert!(err.contains("frame too large"), "{err}");
        assert_eq!(r.next_packet().unwrap().unwrap().seq, 3);
    }

    #[test]
    fn corrupt_frame_is_protocol_error_and_stream_continues() {
        let mut r = FrameReader::new();
        r.feed(&[0x00, 0x05, 0x01, 0x00]); // COBS-valid bytes, bogus packet
        r.feed(&encode_frame(&pkt(9)).unwrap());
        assert!(r.next_packet().is_err());
        assert_eq!(r.next_packet().unwrap().unwrap().seq, 9);
    }

    #[test]
    fn abandon_partial_discards_and_keeps_going() {
        let mut r = FrameReader::new();
        r.feed(&[0x00, 0x11, 0x22]);
        assert_eq!(r.partial_len(), 2);
        r.abandon_partial();
        assert_eq!(r.partial_len(), 0);
        r.feed(&encode_frame(&pkt(2)).unwrap());
        assert_eq!(r.next_packet().unwrap().unwrap().seq, 2);
    }
}
