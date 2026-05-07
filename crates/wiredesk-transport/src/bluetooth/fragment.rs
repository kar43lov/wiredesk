//! Packet fragmentation for the BLE transport.
//!
//! BLE GATT writes / notifications are bounded by the negotiated ATT MTU
//! (default 247 bytes; minus a 3-byte ATT header → 244 bytes of effective
//! payload per write). WireDesk packets after COBS encoding can be up to
//! `MAX_FRAME_SIZE = 8192` bytes, so each `Transport::send` is split into
//! multiple BLE writes here and reassembled on the receiver side.
//!
//! Wire format per BLE write:
//!
//! ```text
//! [ChunkHeader (4 bytes)] [chunk_payload (0..=240 bytes)]
//!  packet_id u16 le
//!  chunk_idx u8
//!  total_chunks u8
//! ```
//!
//! `packet_id` disambiguates concurrent in-flight packets (it's only
//! "concurrent" when chunks of two packets interleave on the wire — rare
//! in practice but cheap to support, and necessary to avoid Reassembler
//! state collisions when the sender's id wraps near the boundary).
//!
//! The Reassembler holds a per-`packet_id` slot containing a `total_chunks`
//! bitmap and the chunk payloads keyed by `chunk_idx`. When all bits are
//! set the slot is finalised, returned to the caller, and freed. Slots
//! that haven't completed within `REASSEMBLY_TIMEOUT` are swept on the
//! next `feed_chunk_at` call so a single dropped chunk doesn't pin
//! per-packet buffers indefinitely.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 3-byte ATT header overhead consumed before our payload.
pub const ATT_HEADER_OVERHEAD: usize = 3;

/// Bytes consumed by [`ChunkHeader`] in front of each chunk's payload.
pub const CHUNK_HEADER_LEN: usize = 4;

/// Default ATT MTU we attempt to negotiate. 247 = 244 ATT payload + 3-byte
/// ATT header. Combined with the 4-byte ChunkHeader this leaves 240 bytes
/// of WireDesk payload per BLE write.
pub const DEFAULT_ATT_MTU: u16 = 247;

/// How long a partially-assembled packet can sit in the Reassembler before
/// we discard it. 5 seconds — long enough that ordinary BLE jitter (sub-
/// second) doesn't trip the sweep, short enough that a permanently-lost
/// chunk doesn't keep the buffer pinned.
pub const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Max chunks per WireDesk packet. With 240 bytes per chunk and `u8`
/// `total_chunks` the cap is 255 → ~60 KB, well above
/// `MAX_FRAME_SIZE = 8192`.
pub const MAX_TOTAL_CHUNKS: usize = 255;

/// Compute the effective WireDesk-payload-per-chunk for a given negotiated
/// ATT MTU. Falls back to 0 if the MTU is so small the headers don't fit.
pub fn max_chunk_payload(att_mtu: u16) -> usize {
    let mtu = att_mtu as usize;
    let after_att = mtu.saturating_sub(ATT_HEADER_OVERHEAD);
    after_att.saturating_sub(CHUNK_HEADER_LEN)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FragmentError {
    #[error("chunk too short: got {got} bytes, need at least {CHUNK_HEADER_LEN} for header")]
    HeaderTooShort { got: usize },
    #[error("invalid total_chunks=0 (must be ≥1)")]
    ZeroTotal,
    #[error("chunk_idx {idx} out of range for total_chunks={total}")]
    OutOfRange { idx: u8, total: u8 },
    #[error("chunk_payload {got} bytes exceeds max {max}")]
    PayloadTooLarge { got: usize, max: usize },
    #[error("payload would split into {chunks} chunks; cap is {MAX_TOTAL_CHUNKS}")]
    TooManyChunks { chunks: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkHeader {
    pub packet_id: u16,
    pub chunk_idx: u8,
    pub total_chunks: u8,
}

impl ChunkHeader {
    pub fn to_bytes(self) -> [u8; CHUNK_HEADER_LEN] {
        let pid = self.packet_id.to_le_bytes();
        [pid[0], pid[1], self.chunk_idx, self.total_chunks]
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, FragmentError> {
        if bytes.len() < CHUNK_HEADER_LEN {
            return Err(FragmentError::HeaderTooShort { got: bytes.len() });
        }
        let packet_id = u16::from_le_bytes([bytes[0], bytes[1]]);
        let chunk_idx = bytes[2];
        let total_chunks = bytes[3];
        if total_chunks == 0 {
            return Err(FragmentError::ZeroTotal);
        }
        if chunk_idx >= total_chunks {
            return Err(FragmentError::OutOfRange {
                idx: chunk_idx,
                total: total_chunks,
            });
        }
        Ok(Self {
            packet_id,
            chunk_idx,
            total_chunks,
        })
    }
}

/// Split a WireDesk packet payload into chunks ready to be written one-by-
/// one onto the BLE wire. `max_chunk_payload` is the bytes-per-chunk cap
/// (already excludes the ATT and ChunkHeader overheads — see
/// [`max_chunk_payload`]).
///
/// Returns `Err(TooManyChunks)` if the payload would require more than
/// `MAX_TOTAL_CHUNKS` chunks. An empty payload (0 bytes) yields a single
/// header-only chunk (`total_chunks=1, chunk_idx=0, no payload`) so the
/// receiver still gets a delivery signal.
pub fn split_packet(
    packet_id: u16,
    payload: &[u8],
    max_chunk_payload: usize,
) -> Result<Vec<Vec<u8>>, FragmentError> {
    assert!(
        max_chunk_payload > 0,
        "max_chunk_payload must be > 0; ATT MTU too small?"
    );

    let total_chunks = if payload.is_empty() {
        1usize
    } else {
        payload.len().div_ceil(max_chunk_payload)
    };
    if total_chunks > MAX_TOTAL_CHUNKS {
        return Err(FragmentError::TooManyChunks {
            chunks: total_chunks,
        });
    }
    let total_chunks_u8 = total_chunks as u8; // checked above

    let mut out = Vec::with_capacity(total_chunks);
    if payload.is_empty() {
        let header = ChunkHeader {
            packet_id,
            chunk_idx: 0,
            total_chunks: 1,
        };
        let mut buf = Vec::with_capacity(CHUNK_HEADER_LEN);
        buf.extend_from_slice(&header.to_bytes());
        out.push(buf);
        return Ok(out);
    }

    for (idx, slice) in payload.chunks(max_chunk_payload).enumerate() {
        let header = ChunkHeader {
            packet_id,
            chunk_idx: idx as u8,
            total_chunks: total_chunks_u8,
        };
        let mut buf = Vec::with_capacity(CHUNK_HEADER_LEN + slice.len());
        buf.extend_from_slice(&header.to_bytes());
        buf.extend_from_slice(slice);
        out.push(buf);
    }
    Ok(out)
}

/// Receiver-side reassembly buffer. Feed one chunk at a time via
/// [`Reassembler::feed_chunk_at`]; returns `Some(payload)` exactly once
/// per packet — when the last missing chunk arrives.
pub struct Reassembler {
    /// Per-packet_id reassembly slot.
    slots: HashMap<u16, Slot>,
}

struct Slot {
    /// First-received chunk's instant — used for stale-sweep.
    first_seen: Instant,
    /// Total expected chunks (from the first chunk we saw).
    total: u8,
    /// Per-chunk payload, indexed by `chunk_idx`. `None` until that
    /// chunk arrives.
    chunks: Vec<Option<Vec<u8>>>,
    /// How many chunks we've actually received — saves us re-scanning
    /// `chunks` on every feed.
    received: usize,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }

    /// Convenience wrapper that uses `Instant::now()`. Production code
    /// should prefer the `_at` variant only when it has a precomputed
    /// timestamp (e.g. tests) — using `Instant::now()` in `feed_chunk` is
    /// the same call as `feed_chunk_at(Instant::now(), …)`.
    pub fn feed_chunk(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, FragmentError> {
        self.feed_chunk_at(Instant::now(), bytes)
    }

    /// Feed one BLE-wire chunk into the reassembler. On the chunk that
    /// completes a packet, returns `Some(payload)` and frees the slot.
    /// Stale slots (older than `REASSEMBLY_TIMEOUT`) are swept on every
    /// call so a single permanently-dropped chunk doesn't keep its
    /// `packet_id` pinned forever.
    pub fn feed_chunk_at(
        &mut self,
        now: Instant,
        bytes: &[u8],
    ) -> Result<Option<Vec<u8>>, FragmentError> {
        // Sweep first so a fresh chunk for an old packet_id isn't merged
        // with stale state.
        self.sweep_expired(now);

        let header = ChunkHeader::from_bytes(bytes)?;
        let payload = bytes[CHUNK_HEADER_LEN..].to_vec();

        let slot = self.slots.entry(header.packet_id).or_insert_with(|| Slot {
            first_seen: now,
            total: header.total_chunks,
            chunks: vec![None; header.total_chunks as usize],
            received: 0,
        });

        // Defensive: a peer that shipped chunks with mismatched
        // `total_chunks` for the same packet_id is buggy — discard the
        // existing slot and start fresh on the new total. Otherwise we'd
        // either index out of range or never complete.
        if slot.total != header.total_chunks {
            log::warn!(
                "BLE reassembler: packet_id={} total_chunks changed {} → {}; discarding slot",
                header.packet_id,
                slot.total,
                header.total_chunks
            );
            *slot = Slot {
                first_seen: now,
                total: header.total_chunks,
                chunks: vec![None; header.total_chunks as usize],
                received: 0,
            };
        }

        // Duplicate chunk arrival — no-op (idempotent).
        let idx = header.chunk_idx as usize;
        if slot.chunks[idx].is_none() {
            slot.chunks[idx] = Some(payload);
            slot.received += 1;
        }

        if slot.received == slot.total as usize {
            // Ownership swap: take the slot out so we can move the chunks
            // out without an extra clone.
            let slot = self.slots.remove(&header.packet_id).expect("just-touched");
            let mut full = Vec::new();
            for chunk in slot.chunks {
                full.extend(chunk.expect("received == total → all slots filled"));
            }
            return Ok(Some(full));
        }

        Ok(None)
    }

    fn sweep_expired(&mut self, now: Instant) {
        self.slots
            .retain(|_, slot| now.duration_since(slot.first_seen) < REASSEMBLY_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_header_roundtrip() {
        let cases = [
            ChunkHeader {
                packet_id: 0,
                chunk_idx: 0,
                total_chunks: 1,
            },
            ChunkHeader {
                packet_id: 1,
                chunk_idx: 0,
                total_chunks: 5,
            },
            ChunkHeader {
                packet_id: 0xFFFE,
                chunk_idx: 4,
                total_chunks: 5,
            },
            ChunkHeader {
                packet_id: 12345,
                chunk_idx: 254,
                total_chunks: 255,
            },
        ];
        for h in cases {
            let bytes = h.to_bytes();
            let back = ChunkHeader::from_bytes(&bytes).expect("roundtrip");
            assert_eq!(back, h);
        }
    }

    #[test]
    fn chunk_header_rejects_short_buffer() {
        let err = ChunkHeader::from_bytes(&[0, 1, 0]).unwrap_err();
        assert_eq!(err, FragmentError::HeaderTooShort { got: 3 });
    }

    #[test]
    fn chunk_header_rejects_zero_total() {
        let err = ChunkHeader::from_bytes(&[0, 0, 0, 0]).unwrap_err();
        assert_eq!(err, FragmentError::ZeroTotal);
    }

    #[test]
    fn chunk_header_rejects_idx_at_or_beyond_total() {
        let err = ChunkHeader::from_bytes(&[0, 0, 5, 5]).unwrap_err();
        assert_eq!(err, FragmentError::OutOfRange { idx: 5, total: 5 });
        let err = ChunkHeader::from_bytes(&[0, 0, 7, 5]).unwrap_err();
        assert_eq!(err, FragmentError::OutOfRange { idx: 7, total: 5 });
    }

    #[test]
    fn max_chunk_payload_default_is_240() {
        assert_eq!(max_chunk_payload(247), 240);
    }

    #[test]
    fn max_chunk_payload_handles_tiny_mtu() {
        // ATT MTU smaller than ATT header + ChunkHeader → 0 payload room.
        // Saturating arithmetic: doesn't panic.
        assert_eq!(max_chunk_payload(7), 0); // 7 - 3 - 4 = 0
        assert_eq!(max_chunk_payload(3), 0);
        assert_eq!(max_chunk_payload(0), 0);
    }

    #[test]
    fn split_packet_single_chunk_under_mtu() {
        let payload = vec![0xAB; 100];
        let chunks = split_packet(7, &payload, 240).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), CHUNK_HEADER_LEN + 100);
        let h = ChunkHeader::from_bytes(&chunks[0]).unwrap();
        assert_eq!(h.packet_id, 7);
        assert_eq!(h.chunk_idx, 0);
        assert_eq!(h.total_chunks, 1);
        assert_eq!(&chunks[0][CHUNK_HEADER_LEN..], &payload[..]);
    }

    #[test]
    fn split_packet_multi_chunk_1000_bytes() {
        // 1000 bytes / 240 max = 5 chunks: 240+240+240+240+40
        let payload: Vec<u8> = (0..1000u32).map(|i| (i & 0xFF) as u8).collect();
        let chunks = split_packet(42, &payload, 240).unwrap();
        assert_eq!(chunks.len(), 5);

        let expected_payload_sizes = [240, 240, 240, 240, 40];
        let expected_wire_total: usize =
            expected_payload_sizes.iter().sum::<usize>() + 5 * CHUNK_HEADER_LEN;
        assert_eq!(expected_wire_total, 1020);

        for (i, chunk) in chunks.iter().enumerate() {
            let h = ChunkHeader::from_bytes(chunk).unwrap();
            assert_eq!(h.packet_id, 42);
            assert_eq!(h.chunk_idx as usize, i);
            assert_eq!(h.total_chunks, 5);
            assert_eq!(chunk.len() - CHUNK_HEADER_LEN, expected_payload_sizes[i]);
        }
        let total_wire: usize = chunks.iter().map(|c| c.len()).sum();
        assert_eq!(total_wire, 1020);
    }

    #[test]
    fn split_packet_max_frame_8192_bytes() {
        // 8192 bytes / 240 max = ⌈8192/240⌉ = 35 chunks.
        let payload = vec![0xAB; 8192];
        let chunks = split_packet(0, &payload, 240).unwrap();
        assert_eq!(chunks.len(), 35);
        let h = ChunkHeader::from_bytes(&chunks[0]).unwrap();
        assert_eq!(h.total_chunks, 35);
        // First 34 chunks are exactly 240 bytes payload, last is the remainder.
        for chunk in chunks.iter().take(34) {
            assert_eq!(chunk.len(), CHUNK_HEADER_LEN + 240);
        }
        // 8192 - 34 * 240 = 32 bytes in last chunk
        assert_eq!(chunks[34].len(), CHUNK_HEADER_LEN + 32);
    }

    #[test]
    fn split_packet_empty_payload_yields_one_empty_chunk() {
        let chunks = split_packet(0, &[], 240).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), CHUNK_HEADER_LEN);
        let h = ChunkHeader::from_bytes(&chunks[0]).unwrap();
        assert_eq!(h.total_chunks, 1);
        assert_eq!(h.chunk_idx, 0);
    }

    #[test]
    fn split_packet_exact_multiple_no_partial_last() {
        // 480 bytes / 240 max = exactly 2 chunks, no partial last.
        let payload = vec![0xCC; 480];
        let chunks = split_packet(0, &payload, 240).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len() - CHUNK_HEADER_LEN, 240);
        assert_eq!(chunks[1].len() - CHUNK_HEADER_LEN, 240);
    }

    #[test]
    fn split_packet_too_many_chunks_errors() {
        // 256 chunks would overflow u8 total_chunks → caught.
        let max = 240;
        let payload = vec![0u8; max * 256];
        let err = split_packet(0, &payload, max).unwrap_err();
        assert_eq!(err, FragmentError::TooManyChunks { chunks: 256 });
    }

    #[test]
    fn reassembler_in_order() {
        let payload: Vec<u8> = (0..1000u32).map(|i| (i & 0xFF) as u8).collect();
        let chunks = split_packet(1, &payload, 240).unwrap();
        let mut r = Reassembler::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let result = r.feed_chunk(chunk).unwrap();
            if i + 1 < chunks.len() {
                assert!(result.is_none(), "chunk {i} should not finalise yet");
            } else {
                assert_eq!(result.as_deref(), Some(&payload[..]));
            }
        }
    }

    #[test]
    fn reassembler_out_of_order() {
        let payload: Vec<u8> = (0..1000u32).map(|i| (i & 0xFF) as u8).collect();
        let chunks = split_packet(7, &payload, 240).unwrap();
        let mut r = Reassembler::new();
        // Feed in reverse order: 4, 3, 2, 1, 0.
        let order = [4usize, 3, 2, 1, 0];
        let mut last = None;
        for &i in &order {
            last = r.feed_chunk(&chunks[i]).unwrap();
        }
        assert_eq!(last.as_deref(), Some(&payload[..]));
    }

    #[test]
    fn reassembler_drops_incomplete_after_timeout() {
        let payload: Vec<u8> = (0..1000u32).map(|i| (i & 0xFF) as u8).collect();
        let chunks = split_packet(11, &payload, 240).unwrap();
        let mut r = Reassembler::new();

        let t0 = Instant::now();
        // Feed first 2 of 5 chunks.
        assert!(r.feed_chunk_at(t0, &chunks[0]).unwrap().is_none());
        assert!(r.feed_chunk_at(t0, &chunks[1]).unwrap().is_none());
        assert!(r.slots.contains_key(&11));

        // Jump well past the timeout. Now feed an unrelated chunk —
        // sweep should kick in and clear the stale packet_id=11 slot.
        let later = t0 + Duration::from_secs(6);
        let other_chunks = split_packet(99, &[0xAA; 50], 240).unwrap();
        assert!(r.feed_chunk_at(later, &other_chunks[0]).unwrap().is_some());
        assert!(!r.slots.contains_key(&11), "stale packet_id=11 swept");
    }

    #[test]
    fn reassembler_disambiguates_packet_ids() {
        let payload_a: Vec<u8> = (0..500u32).map(|i| (i & 0xFF) as u8).collect();
        let payload_b: Vec<u8> = (500..1500u32).map(|i| (i & 0xFF) as u8).collect();
        let chunks_a = split_packet(1, &payload_a, 240).unwrap();
        let chunks_b = split_packet(2, &payload_b, 240).unwrap();
        // chunks_a has 3 chunks (500 / 240 = 2.08 → 3); chunks_b has 5 (1000 / 240).
        assert_eq!(chunks_a.len(), 3);
        assert_eq!(chunks_b.len(), 5);

        let mut r = Reassembler::new();
        // Interleave.
        assert!(r.feed_chunk(&chunks_a[0]).unwrap().is_none());
        assert!(r.feed_chunk(&chunks_b[0]).unwrap().is_none());
        assert!(r.feed_chunk(&chunks_b[1]).unwrap().is_none());
        assert!(r.feed_chunk(&chunks_a[1]).unwrap().is_none());
        assert!(r.feed_chunk(&chunks_b[2]).unwrap().is_none());
        // Finish A.
        let result_a = r.feed_chunk(&chunks_a[2]).unwrap();
        assert_eq!(result_a.as_deref(), Some(&payload_a[..]));
        // B is still incomplete.
        assert!(r.feed_chunk(&chunks_b[3]).unwrap().is_none());
        let result_b = r.feed_chunk(&chunks_b[4]).unwrap();
        assert_eq!(result_b.as_deref(), Some(&payload_b[..]));
    }

    #[test]
    fn reassembler_idempotent_on_duplicate_chunk() {
        let payload = vec![0xAB; 240];
        let chunks = split_packet(0, &payload, 240).unwrap();
        // 240/240 = 1 chunk
        assert_eq!(chunks.len(), 1);
        let mut r = Reassembler::new();
        let result = r.feed_chunk(&chunks[0]).unwrap();
        assert_eq!(result.as_deref(), Some(&payload[..]));
        // After completion the slot is gone — feeding the same chunk again
        // simulates a duplicate from a flaky link starting a new (1-chunk)
        // packet under the same packet_id and finishes it, but doesn't crash.
        let result = r.feed_chunk(&chunks[0]).unwrap();
        assert_eq!(result.as_deref(), Some(&payload[..]));
    }
}
