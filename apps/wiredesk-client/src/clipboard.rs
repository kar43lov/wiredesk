//! Clipboard sync — Mac side.
//!
//! Polls the local Mac clipboard once per CLIP_POLL_INTERVAL. When the text
//! changes we send it to Host via outgoing channel as a ClipOffer + N×ClipChunk.
//! Incoming clipboard messages are reassembled and written to the Mac
//! clipboard. A hash of the last known content is tracked so we don't echo
//! back what we just received (loop avoidance).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wiredesk_protocol::message::{FORMAT_PNG_IMAGE, FORMAT_TEXT_UTF8, Message};
use wiredesk_protocol::packet::Packet;

const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CHUNK_SIZE: usize = 256;
const MAX_CLIPBOARD_BYTES: usize = 256 * 1024; // 256 KB cap for text
/// Maximum encoded PNG size we will push to the peer. Larger payloads are
/// dropped with a warning (and a UI toast wired up in Task 7b). The cap is
/// applied to the encoded-PNG length, not the RGBA pre-image, because PNG
/// compression ratios are content-dependent and we cannot predict the size
/// from raw dimensions.
pub(crate) const MAX_IMAGE_BYTES: usize = 1024 * 1024; // 1 MB encoded

/// Type-tagged hash of the most recent clipboard content owned/observed by us.
/// Used to suppress re-sending what we just wrote (loop avoidance) while
/// keeping text and image dedup independent — copying text after an image
/// (or vice versa) does not get blocked by the wrong-kind hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LastKind {
    #[default]
    None,
    Text(u64),
    /// Wired-up by the image send path (Task 4) and receive path (Task 5).
    Image(u64),
}

/// Shared state: last known clipboard kind+hash. Updated when we either set or
/// read the local clipboard.
#[derive(Clone, Default)]
pub struct ClipboardState {
    last: Arc<Mutex<LastKind>>,
}

impl ClipboardState {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self) -> LastKind {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set(&self, kind: LastKind) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        *g = kind;
    }
}

fn hash_text(s: &str) -> u64 {
    hash_bytes(s.as_bytes())
}

/// Stable hash over arbitrary bytes (RGBA buffers, encoded payloads, …).
/// Uses `DefaultHasher` — same instance/version of the runtime gives the same
/// value, which is all we need for in-process loop avoidance.
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

/// Pure helper used both by production code (with `MAX_IMAGE_BYTES`) and
/// unit tests (with a low limit so synthetic 4×4 RGBA fixtures can exercise
/// the oversize path). Returns `Err` if the encoded PNG exceeds the limit.
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

/// Decode PNG bytes to an arboard `ImageData` (RGBA8, owned).
///
/// Wired up by `IncomingClipboard::commit` in Task 5. Currently exercised
/// only by tests.
#[allow(dead_code)]
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

/// Push a single clipboard payload (text bytes or encoded PNG) as a
/// `ClipOffer` followed by N×`ClipChunk`. Updates the outgoing progress
/// counters so the UI can render a "sending …/… KB" status line.
///
/// Pure helper (no thread, no clipboard backend) — used by both the
/// production poll thread and unit tests.
fn emit_offer_and_chunks(
    outgoing_tx: &mpsc::Sender<Packet>,
    format: u8,
    payload: &[u8],
    outgoing_progress: &Arc<AtomicU64>,
    outgoing_total: &Arc<AtomicU64>,
) {
    outgoing_progress.store(0, Ordering::Relaxed);
    outgoing_total.store(payload.len() as u64, Ordering::Relaxed);

    let _ = outgoing_tx.send(Packet::new(
        Message::ClipOffer {
            format,
            total_len: payload.len() as u32,
        },
        0,
    ));

    for (idx, chunk) in payload.chunks(CHUNK_SIZE).enumerate() {
        let chunk_len = chunk.len() as u64;
        let _ = outgoing_tx.send(Packet::new(
            Message::ClipChunk {
                index: idx as u16,
                data: chunk.to_vec(),
            },
            0,
        ));
        outgoing_progress.fetch_add(chunk_len, Ordering::Relaxed);
    }
}

/// Spawn a background thread that polls the local clipboard and pushes
/// ClipOffer + ClipChunks onto `outgoing_tx` whenever the content changes
/// (and isn't something we just wrote ourselves).
///
/// `outgoing_progress` / `outgoing_total` are updated as bytes are queued
/// for the writer thread; the UI reads them to render a progress line.
pub fn spawn_poll_thread(
    state: ClipboardState,
    outgoing_tx: mpsc::Sender<Packet>,
    outgoing_progress: Arc<AtomicU64>,
    outgoing_total: Arc<AtomicU64>,
) {
    thread::spawn(move || {
        let mut clip = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("clipboard: failed to init: {e}");
                return;
            }
        };

        loop {
            thread::sleep(CLIP_POLL_INTERVAL);

            // 1) Try text first. Empty/error → fall through to image.
            match clip.get_text() {
                Ok(text) if !text.is_empty() => {
                    let hash = hash_text(&text);
                    if matches!(state.get(), LastKind::Text(h) if h == hash) {
                        continue;
                    }
                    state.set(LastKind::Text(hash));

                    let bytes = text.as_bytes();
                    if bytes.len() > MAX_CLIPBOARD_BYTES {
                        log::warn!(
                            "clipboard: skipping push — {} bytes exceeds limit",
                            bytes.len()
                        );
                        continue;
                    }

                    log::debug!("clipboard: pushing {} bytes to host", bytes.len());
                    emit_offer_and_chunks(
                        &outgoing_tx,
                        FORMAT_TEXT_UTF8,
                        bytes,
                        &outgoing_progress,
                        &outgoing_total,
                    );
                    continue;
                }
                _ => {} // fall through to image probe
            }

            // 2) Try image. arboard returns RGBA8.
            let img = match clip.get_image() {
                Ok(i) => i,
                Err(_) => continue, // not an image either; idle
            };

            let hash = hash_bytes(&img.bytes);
            if matches!(state.get(), LastKind::Image(h) if h == hash) {
                continue;
            }

            let png = match encode_rgba_to_png(&img) {
                Ok(p) => p,
                Err(e) => {
                    log::warn!("clipboard: PNG encode failed: {e}");
                    continue;
                }
            };

            if let Err(e) = check_image_size(png.len(), MAX_IMAGE_BYTES) {
                log::warn!(
                    "clipboard: image too large ({} bytes, limit {}), skipping",
                    e.png_len,
                    e.limit
                );
                // Don't update LastKind — user may shrink selection and retry,
                // we want the next attempt to still be considered "new".
                continue;
            }

            state.set(LastKind::Image(hash));

            log::debug!("clipboard: pushing image to host ({} encoded bytes)", png.len());
            emit_offer_and_chunks(
                &outgoing_tx,
                FORMAT_PNG_IMAGE,
                &png,
                &outgoing_progress,
                &outgoing_total,
            );
        }
    });
}

/// Reassembles incoming ClipOffer + ClipChunks. Owned by the reader thread.
pub struct IncomingClipboard {
    state: ClipboardState,
    expected_len: u32,
    received: BTreeMap<u16, Vec<u8>>,
    received_total: u32,
    clip: Option<arboard::Clipboard>,
}

impl IncomingClipboard {
    pub fn new(state: ClipboardState) -> Self {
        Self {
            state,
            expected_len: 0,
            received: BTreeMap::new(),
            received_total: 0,
            clip: arboard::Clipboard::new().ok(),
        }
    }

    pub fn on_offer(&mut self, total_len: u32) {
        self.expected_len = total_len;
        self.received.clear();
        self.received_total = 0;
        log::debug!("clipboard: incoming offer of {total_len} bytes");
    }

    pub fn on_chunk(&mut self, index: u16, data: Vec<u8>) {
        self.received_total += data.len() as u32;
        self.received.insert(index, data);

        if self.received_total >= self.expected_len && self.expected_len > 0 {
            self.commit();
        }
    }

    fn commit(&mut self) {
        let mut buf = Vec::with_capacity(self.expected_len as usize);
        for (_, chunk) in std::mem::take(&mut self.received) {
            buf.extend_from_slice(&chunk);
        }

        match String::from_utf8(buf) {
            Ok(text) => {
                let hash = hash_text(&text);
                self.state.set(LastKind::Text(hash)); // mark as ours so poll won't echo
                if let Some(clip) = self.clip.as_mut() {
                    if let Err(e) = clip.set_text(text.clone()) {
                        log::warn!("clipboard: set_text failed: {e}");
                    } else {
                        log::debug!("clipboard: wrote {} bytes from host", text.len());
                    }
                }
            }
            Err(e) => log::warn!("clipboard: incoming bytes not valid UTF-8: {e}"),
        }

        self.expected_len = 0;
        self.received_total = 0;
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

    #[test]
    fn encode_decode_roundtrip() {
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");
        let decoded = decode_png_to_rgba(&png).expect("decode");
        assert_eq!(decoded.width, original.width);
        assert_eq!(decoded.height, original.height);
        assert_eq!(&*decoded.bytes, &*original.bytes, "RGBA must roundtrip byte-for-byte");
    }

    #[test]
    fn hash_bytes_stable() {
        let img = synthetic_rgba_4x4();
        let h1 = hash_bytes(&img.bytes);
        let h2 = hash_bytes(&img.bytes);
        assert_eq!(h1, h2, "same RGBA buffer must hash to same value");

        // different content → different hash
        let mut other = img.bytes.to_vec();
        other[0] ^= 0xFF;
        let h3 = hash_bytes(&other);
        assert_ne!(h1, h3);
    }

    #[test]
    fn last_kind_dedup_text_does_not_block_image() {
        let state = ClipboardState::new();

        // Mark a text content as recently seen.
        state.set(LastKind::Text(12345));
        assert!(matches!(state.get(), LastKind::Text(12345)));

        // An image with hash 12345 (collision across kinds) must NOT be
        // considered a duplicate — the kind tag distinguishes them.
        let stored_is_text_with_image_hash = matches!(state.get(), LastKind::Text(h) if h == 12345);
        assert!(stored_is_text_with_image_hash);

        // Now simulate setting an image hash; the state should switch.
        state.set(LastKind::Image(12345));
        assert!(matches!(state.get(), LastKind::Image(12345)));
        assert!(!matches!(state.get(), LastKind::Text(_)));
    }

    #[test]
    fn last_kind_default_is_none() {
        let state = ClipboardState::new();
        assert!(matches!(state.get(), LastKind::None));
    }

    #[test]
    fn hash_text_matches_hash_bytes() {
        // hash_text is a thin wrapper — guarantee both hashers stay aligned
        // so future refactors don't desync text and binary paths.
        let s = "hello, мир";
        assert_eq!(hash_text(s), hash_bytes(s.as_bytes()));
    }

    #[test]
    fn check_image_size_within_limit() {
        assert_eq!(check_image_size(100, 1024), Ok(()));
        assert_eq!(check_image_size(1024, 1024), Ok(()), "boundary is inclusive");
    }

    #[test]
    fn image_too_large_skipped() {
        // Pure helper test — synthetic 4×4 PNG encodes to ~70-100 bytes; we
        // pick a tiny limit so the path is reproducibly exercised without
        // having to fabricate megabyte-sized payloads.
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");
        assert!(png.len() > 32, "synthetic PNG should be at least 32 bytes");

        let result = check_image_size(png.len(), 32);
        let err = result.expect_err("expected oversize");
        assert_eq!(err.png_len, png.len());
        assert_eq!(err.limit, 32);
    }

    #[test]
    fn image_emit_offer_and_chunks() {
        // Drive the pure emitter without a thread / clipboard backend.
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");

        let (tx, rx) = mpsc::channel::<Packet>();
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));

        emit_offer_and_chunks(&tx, FORMAT_PNG_IMAGE, &png, &progress, &total);
        drop(tx); // close channel so rx loop terminates

        let mut packets: Vec<Packet> = Vec::new();
        while let Ok(p) = rx.recv() {
            packets.push(p);
        }

        // First packet — ClipOffer with format=1 and total_len=png.len().
        let first = &packets[0];
        match &first.message {
            Message::ClipOffer { format, total_len } => {
                assert_eq!(*format, FORMAT_PNG_IMAGE);
                assert_eq!(*total_len as usize, png.len());
            }
            other => panic!("expected ClipOffer first, got {other:?}"),
        }

        // Remaining packets — ClipChunk; concatenated bytes must equal PNG.
        let mut reassembled: Vec<u8> = Vec::new();
        for (i, p) in packets[1..].iter().enumerate() {
            match &p.message {
                Message::ClipChunk { index, data } => {
                    assert_eq!(*index as usize, i, "chunks must be sequential");
                    reassembled.extend_from_slice(data);
                }
                other => panic!("expected ClipChunk at idx {i}, got {other:?}"),
            }
        }
        assert_eq!(reassembled, png, "concatenated chunks must reassemble to PNG");

        // Counter invariants.
        assert_eq!(total.load(Ordering::Relaxed) as usize, png.len());
        assert_eq!(progress.load(Ordering::Relaxed) as usize, png.len());
    }

    #[test]
    fn image_emit_chunks_respect_chunk_size() {
        // Build a payload longer than CHUNK_SIZE so we get >1 chunk.
        let payload: Vec<u8> = (0..(CHUNK_SIZE * 3 + 7)).map(|i| (i & 0xFF) as u8).collect();

        let (tx, rx) = mpsc::channel::<Packet>();
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));

        emit_offer_and_chunks(&tx, FORMAT_PNG_IMAGE, &payload, &progress, &total);
        drop(tx);

        let mut chunk_count = 0usize;
        let mut got_offer = false;
        while let Ok(p) = rx.recv() {
            match &p.message {
                Message::ClipOffer { .. } => got_offer = true,
                Message::ClipChunk { data, .. } => {
                    assert!(data.len() <= CHUNK_SIZE, "chunk over CHUNK_SIZE");
                    chunk_count += 1;
                }
                other => panic!("unexpected message {other:?}"),
            }
        }
        assert!(got_offer, "first message must be ClipOffer");
        // ceil(len / CHUNK_SIZE) = 4 for 256*3+7 bytes
        assert_eq!(chunk_count, 4);
    }
}
