//! Clipboard sync — Windows side.
//!
//! Symmetric with the Mac client: poll local clipboard, push changes to the
//! peer as ClipOffer + ClipChunks; reassemble incoming and write to local.
//!
//! Supports two formats over the existing `ClipOffer.format` field:
//!   - `FORMAT_TEXT_UTF8` (0) — UTF-8 text (256 KB cap).
//!   - `FORMAT_PNG_IMAGE` (1) — PNG-encoded RGBA image (1 MB encoded cap).
//!
//! Single-threaded: invoked from the Session tick loop, so the dedup state
//! doesn't need synchronisation (unlike the Mac side which polls in a
//! background thread).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use wiredesk_protocol::message::{FORMAT_PNG_IMAGE, FORMAT_TEXT_UTF8, Message};

const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const CHUNK_SIZE: usize = 256;
const MAX_CLIPBOARD_BYTES: usize = 256 * 1024; // text cap
/// Cap on encoded-PNG length pushed to the peer. Larger payloads are dropped
/// with a warning log (no UI on Host — see Mac client for toast).
pub(crate) const MAX_IMAGE_BYTES: usize = 1024 * 1024; // 1 MB encoded

/// Type-tagged hash of the most recent clipboard content owned/observed by us.
/// Mirrors the Mac-side enum (CLAUDE.md explicitly allows this duplication).
/// Image hash is taken over the RGBA buffer because PNG encode is not
/// deterministic across peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LastKind {
    #[default]
    None,
    Text(u64),
    Image(u64),
}

fn hash_text(s: &str) -> u64 {
    hash_bytes(s.as_bytes())
}

/// Stable hash over arbitrary bytes (RGBA buffers, encoded payloads, …).
fn hash_bytes(bytes: &[u8]) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// Encode an arboard `ImageData` (RGBA8) to PNG bytes.
fn encode_rgba_to_png(img: &arboard::ImageData<'_>) -> Result<Vec<u8>, image::ImageError> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out).write_image(
        &img.bytes,
        img.width as u32,
        img.height as u32,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(out)
}

/// Decode PNG bytes to an arboard `ImageData` (RGBA8, owned).
fn decode_png_to_rgba(bytes: &[u8]) -> Result<arboard::ImageData<'static>, image::ImageError> {
    let dyn_img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(rgba.into_raw()),
    })
}

/// Pure size-check helper — used by production poll (`MAX_IMAGE_BYTES`) and
/// by unit tests with a low limit so synthetic 4×4 fixtures hit the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ImageTooLarge {
    pub png_len: usize,
    pub limit: usize,
}

pub(crate) fn check_image_size(png_len: usize, limit: usize) -> Result<(), ImageTooLarge> {
    if png_len > limit {
        Err(ImageTooLarge { png_len, limit })
    } else {
        Ok(())
    }
}

/// Build a `ClipOffer` + N `ClipChunk` messages for one payload. Pure helper.
fn build_offer_and_chunks(format: u8, payload: &[u8]) -> Vec<Message> {
    let mut msgs = Vec::with_capacity(1 + payload.len() / CHUNK_SIZE + 1);
    msgs.push(Message::ClipOffer {
        format,
        total_len: payload.len() as u32,
    });
    for (idx, chunk) in payload.chunks(CHUNK_SIZE).enumerate() {
        msgs.push(Message::ClipChunk {
            index: idx as u16,
            data: chunk.to_vec(),
        });
    }
    msgs
}

pub struct ClipboardSync {
    clip: Option<arboard::Clipboard>,
    last: LastKind,
    last_poll: Instant,

    // Reassembly state for incoming offers.
    expected_len: u32,
    expected_format: u8,
    received_total: u32,
    received: BTreeMap<u16, Vec<u8>>,

    // Local progress counters — Host-side single-threaded, used for logging
    // (Task 8) and conceptual symmetry with the Mac UI.
    #[allow(dead_code)]
    outgoing_progress: u64,
    #[allow(dead_code)]
    outgoing_total: u64,
    #[allow(dead_code)]
    incoming_progress: u64,
    #[allow(dead_code)]
    incoming_total: u64,

    /// Test-only sink — captures the last successfully committed payload so
    /// unit tests can assert on outcomes without depending on a live arboard
    /// clipboard backend.
    #[cfg(test)]
    last_committed: Option<CommittedPayload>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CommittedPayload {
    Text(String),
    Image { width: usize, height: usize, bytes: Vec<u8> },
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self {
            clip: arboard::Clipboard::new().ok(),
            last: LastKind::None,
            last_poll: Instant::now(),
            expected_len: 0,
            expected_format: 0,
            received_total: 0,
            received: BTreeMap::new(),
            outgoing_progress: 0,
            outgoing_total: 0,
            incoming_progress: 0,
            incoming_total: 0,
            #[cfg(test)]
            last_committed: None,
        }
    }

    /// Test constructor — skips arboard init (which would fail in headless CI
    /// without a window server) and lets us inspect committed payloads.
    #[cfg(test)]
    fn new_for_test() -> Self {
        Self {
            clip: None,
            last: LastKind::None,
            last_poll: Instant::now(),
            expected_len: 0,
            expected_format: 0,
            received_total: 0,
            received: BTreeMap::new(),
            outgoing_progress: 0,
            outgoing_total: 0,
            incoming_progress: 0,
            incoming_total: 0,
            last_committed: None,
        }
    }

    /// Called from session.tick(). Returns a list of messages to send if the
    /// local clipboard changed since last poll. Tries text first, then image.
    pub fn poll(&mut self) -> Vec<Message> {
        if self.last_poll.elapsed() < CLIP_POLL_INTERVAL {
            return Vec::new();
        }
        self.last_poll = Instant::now();

        let Some(clip) = self.clip.as_mut() else {
            return Vec::new();
        };

        // 1) Text path.
        match clip.get_text() {
            Ok(text) if !text.is_empty() => {
                let hash = hash_text(&text);
                if matches!(self.last, LastKind::Text(h) if h == hash) {
                    return Vec::new();
                }
                self.last = LastKind::Text(hash);

                let bytes = text.as_bytes();
                if bytes.len() > MAX_CLIPBOARD_BYTES {
                    log::warn!(
                        "clipboard: skipping push — {} bytes exceeds limit",
                        bytes.len()
                    );
                    return Vec::new();
                }

                log::debug!("clipboard: pushing {} bytes to client", bytes.len());
                self.outgoing_total = bytes.len() as u64;
                self.outgoing_progress = bytes.len() as u64;
                return build_offer_and_chunks(FORMAT_TEXT_UTF8, bytes);
            }
            _ => {} // fall through to image probe
        }

        // 2) Image path.
        let img = match clip.get_image() {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };

        let hash = hash_bytes(&img.bytes);
        if matches!(self.last, LastKind::Image(h) if h == hash) {
            return Vec::new();
        }

        let png = match encode_rgba_to_png(&img) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("clipboard: PNG encode failed: {e}");
                return Vec::new();
            }
        };

        if let Err(e) = check_image_size(png.len(), MAX_IMAGE_BYTES) {
            log::warn!(
                "clipboard: image too large ({} bytes, limit {}), skipping",
                e.png_len,
                e.limit
            );
            // Don't update LastKind — user may shrink selection and retry.
            return Vec::new();
        }

        self.last = LastKind::Image(hash);
        log::info!(
            "clipboard: sending image to peer ({} bytes)",
            png.len()
        );
        self.outgoing_total = png.len() as u64;
        self.outgoing_progress = png.len() as u64;
        build_offer_and_chunks(FORMAT_PNG_IMAGE, &png)
    }

    pub fn on_offer(&mut self, format: u8, total_len: u32) {
        // Abort an in-progress reassembly if a new offer arrives mid-transfer.
        if self.received_total > 0 && self.received_total < self.expected_len {
            log::warn!(
                "clipboard: incoming offer aborted previous reassembly ({} of {} bytes accumulated)",
                self.received_total,
                self.expected_len
            );
        }
        self.expected_len = total_len;
        self.expected_format = format;
        self.received.clear();
        self.received_total = 0;
        self.incoming_total = total_len as u64;
        self.incoming_progress = 0;
        log::debug!(
            "clipboard: incoming offer format={format} of {total_len} bytes"
        );
    }

    pub fn on_chunk(&mut self, index: u16, data: Vec<u8>) {
        let added = data.len() as u32;
        self.received_total += added;
        self.incoming_progress += added as u64;
        self.received.insert(index, data);

        if self.received_total >= self.expected_len && self.expected_len > 0 {
            self.commit();
        }
    }

    /// Drop any in-flight reassembly state and zero progress counters.
    /// Called from the session loop on disconnect / new Hello so a partial
    /// transfer doesn't leak across sessions.
    pub fn reset(&mut self) {
        self.expected_len = 0;
        self.expected_format = 0;
        self.received.clear();
        self.received_total = 0;
        self.incoming_progress = 0;
        self.incoming_total = 0;
    }

    fn commit(&mut self) {
        let mut buf = Vec::with_capacity(self.expected_len as usize);
        for (_, chunk) in std::mem::take(&mut self.received) {
            buf.extend_from_slice(&chunk);
        }

        match self.expected_format {
            FORMAT_TEXT_UTF8 => self.commit_text(buf),
            FORMAT_PNG_IMAGE => self.commit_image(&buf),
            other => {
                log::warn!("clipboard: unknown format {other}, skipping {} bytes", buf.len());
            }
        }

        self.expected_len = 0;
        self.expected_format = 0;
        self.received_total = 0;
    }

    fn commit_text(&mut self, buf: Vec<u8>) {
        match String::from_utf8(buf) {
            Ok(text) => {
                self.last = LastKind::Text(hash_text(&text)); // mark as ours
                #[cfg(test)]
                {
                    self.last_committed = Some(CommittedPayload::Text(text.clone()));
                }
                if let Some(clip) = self.clip.as_mut() {
                    if let Err(e) = clip.set_text(text.clone()) {
                        log::warn!("clipboard: set_text failed: {e}");
                    } else {
                        log::debug!("clipboard: wrote {} bytes from client", text.len());
                    }
                }
            }
            Err(e) => log::warn!("clipboard: incoming bytes not valid UTF-8: {e}"),
        }
    }

    fn commit_image(&mut self, buf: &[u8]) {
        let img = match decode_png_to_rgba(buf) {
            Ok(i) => i,
            Err(e) => {
                log::warn!("clipboard: PNG decode failed: {e}");
                return;
            }
        };

        let hash = hash_bytes(&img.bytes);
        self.last = LastKind::Image(hash);

        log::info!(
            "clipboard: received image from peer ({} bytes)",
            buf.len()
        );

        #[cfg(test)]
        {
            self.last_committed = Some(CommittedPayload::Image {
                width: img.width,
                height: img.height,
                bytes: img.bytes.to_vec(),
            });
        }

        if let Some(clip) = self.clip.as_mut() {
            if let Err(e) = clip.set_image(img) {
                log::warn!("clipboard: set_image failed: {e}");
            } else {
                log::debug!("clipboard: wrote image from client ({} encoded bytes)", buf.len());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic 4×4 RGBA buffer with deterministic content.
    fn synthetic_rgba_4x4() -> arboard::ImageData<'static> {
        let mut bytes = Vec::with_capacity(4 * 4 * 4);
        for y in 0..4u8 {
            for x in 0..4u8 {
                bytes.push(x * 64); // R
                bytes.push(y * 64); // G
                bytes.push(0x80); // B
                bytes.push(0xFF); // A
            }
        }
        arboard::ImageData {
            width: 4,
            height: 4,
            bytes: std::borrow::Cow::Owned(bytes),
        }
    }

    /// Push `payload` through `ClipboardSync` as one offer + N chunks.
    fn feed_offer(sync: &mut ClipboardSync, format: u8, payload: &[u8]) {
        sync.on_offer(format, payload.len() as u32);
        for (i, chunk) in payload.chunks(CHUNK_SIZE).enumerate() {
            sync.on_chunk(i as u16, chunk.to_vec());
        }
    }

    #[test]
    fn host_encode_decode_roundtrip() {
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");
        let decoded = decode_png_to_rgba(&png).expect("decode");
        assert_eq!(decoded.width, original.width);
        assert_eq!(decoded.height, original.height);
        assert_eq!(&*decoded.bytes, &*original.bytes, "RGBA must roundtrip byte-for-byte");
    }

    #[test]
    fn host_hash_bytes_stable() {
        let img = synthetic_rgba_4x4();
        let h1 = hash_bytes(&img.bytes);
        let h2 = hash_bytes(&img.bytes);
        assert_eq!(h1, h2);

        let mut other = img.bytes.to_vec();
        other[0] ^= 0xFF;
        let h3 = hash_bytes(&other);
        assert_ne!(h1, h3);
    }

    #[test]
    fn host_hash_text_matches_hash_bytes() {
        let s = "hello, мир";
        assert_eq!(hash_text(s), hash_bytes(s.as_bytes()));
    }

    #[test]
    fn host_image_too_large_skipped() {
        // Pure helper test — synthetic PNG vs tiny limit.
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");
        assert!(png.len() > 32);

        let result = check_image_size(png.len(), 32);
        let err = result.expect_err("expected oversize");
        assert_eq!(err.png_len, png.len());
        assert_eq!(err.limit, 32);

        // Within-limit branch.
        assert_eq!(check_image_size(100, 1024), Ok(()));
        assert_eq!(check_image_size(1024, 1024), Ok(()), "boundary inclusive");
    }

    #[test]
    fn host_build_offer_and_chunks_shape() {
        // Build a payload longer than CHUNK_SIZE so we get >1 chunk.
        let payload: Vec<u8> = (0..(CHUNK_SIZE * 3 + 7)).map(|i| (i & 0xFF) as u8).collect();
        let msgs = build_offer_and_chunks(FORMAT_PNG_IMAGE, &payload);

        match &msgs[0] {
            Message::ClipOffer { format, total_len } => {
                assert_eq!(*format, FORMAT_PNG_IMAGE);
                assert_eq!(*total_len as usize, payload.len());
            }
            other => panic!("expected ClipOffer, got {other:?}"),
        }

        let mut reassembled: Vec<u8> = Vec::new();
        for (i, m) in msgs[1..].iter().enumerate() {
            match m {
                Message::ClipChunk { index, data } => {
                    assert_eq!(*index as usize, i);
                    assert!(data.len() <= CHUNK_SIZE);
                    reassembled.extend_from_slice(data);
                }
                other => panic!("expected ClipChunk at {i}, got {other:?}"),
            }
        }
        assert_eq!(reassembled, payload);
        // ceil((CHUNK_SIZE*3 + 7) / CHUNK_SIZE) = 4
        assert_eq!(msgs.len() - 1, 4);
    }

    #[test]
    fn host_incoming_image_reassembly() {
        // synthetic RGBA → encode → feed back through ClipboardSync →
        // decoded RGBA must match the original byte-for-byte.
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");

        let mut sync = ClipboardSync::new_for_test();
        feed_offer(&mut sync, FORMAT_PNG_IMAGE, &png);

        match sync.last_committed.as_ref().expect("committed payload") {
            CommittedPayload::Image { width, height, bytes } => {
                assert_eq!(*width, original.width);
                assert_eq!(*height, original.height);
                assert_eq!(bytes.as_slice(), &*original.bytes);
            }
            other => panic!("expected image payload, got {other:?}"),
        }
        assert!(matches!(sync.last, LastKind::Image(_)));
    }

    #[test]
    fn host_incoming_text_reassembly_unchanged() {
        // Regression: text path keeps working (format=0).
        let text = "hello, мир";
        let bytes = text.as_bytes().to_vec();

        let mut sync = ClipboardSync::new_for_test();
        feed_offer(&mut sync, FORMAT_TEXT_UTF8, &bytes);

        match sync.last_committed.as_ref().expect("committed payload") {
            CommittedPayload::Text(s) => assert_eq!(s, text),
            other => panic!("expected text, got {other:?}"),
        }
        assert!(matches!(sync.last, LastKind::Text(_)));
    }

    #[test]
    fn host_incoming_invalid_png_skipped() {
        // format=1 + non-PNG payload → no panic, no commit, no state update.
        let mut sync = ClipboardSync::new_for_test();
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03];
        feed_offer(&mut sync, FORMAT_PNG_IMAGE, &garbage);

        assert!(sync.last_committed.is_none());
        assert!(matches!(sync.last, LastKind::None));
        // Receiver ready for next offer.
        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.expected_format, 0);
    }

    #[test]
    fn host_incoming_unknown_format_skipped() {
        let mut sync = ClipboardSync::new_for_test();
        feed_offer(&mut sync, 0xFE, b"opaque");

        assert!(sync.last_committed.is_none());
        assert!(matches!(sync.last, LastKind::None));
    }

    #[test]
    fn host_incoming_offer_during_reassembly_aborts_previous() {
        // Start a 1024-byte text offer, push one 256-byte chunk, then send a
        // fresh PNG offer. Receiver must drop the partial text and switch.
        let mut sync = ClipboardSync::new_for_test();

        sync.on_offer(FORMAT_TEXT_UTF8, 1024);
        sync.on_chunk(0, vec![b'a'; 256]);
        assert_eq!(sync.received_total, 256);

        sync.on_offer(FORMAT_PNG_IMAGE, 512);
        assert_eq!(sync.expected_format, FORMAT_PNG_IMAGE);
        assert_eq!(sync.expected_len, 512);
        assert_eq!(sync.received_total, 0);
        assert!(sync.received.is_empty());
    }

    #[test]
    fn host_reset_clears_state() {
        let mut sync = ClipboardSync::new_for_test();

        sync.on_offer(FORMAT_PNG_IMAGE, 4096);
        sync.on_chunk(0, vec![0u8; 256]);
        sync.on_chunk(1, vec![0u8; 256]);
        assert!(sync.received_total > 0);
        assert!(sync.incoming_progress > 0);
        assert!(sync.incoming_total > 0);

        sync.reset();

        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.expected_format, 0);
        assert_eq!(sync.received_total, 0);
        assert!(sync.received.is_empty());
        assert_eq!(sync.incoming_progress, 0);
        assert_eq!(sync.incoming_total, 0);
    }
}
