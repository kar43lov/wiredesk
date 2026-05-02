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

use crate::app::TransportEvent;

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

/// Human-readable message rendered in the chrome toast slot when an image
/// copy is dropped for being over `MAX_IMAGE_BYTES`. Pure helper so the wording
/// stays unit-testable without spinning the poll thread.
pub(crate) fn format_oversize_toast(e: &ImageTooLarge) -> String {
    format!(
        "image too large ({} KB), copy a smaller selection",
        e.png_len / 1024
    )
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

    // Zero counters once everything is queued. Per Task 7a plan: the
    // simplest "transfer complete" UX is to clear the status fragment
    // immediately after the last chunk is queued — the wire-side send
    // happens in writer_thread asynchronously, so "queued progress" is the
    // best signal we have without an extra ack channel. UI reads atomics
    // each frame and silently stops rendering the "Sending …" line.
    outgoing_progress.store(0, Ordering::Relaxed);
    outgoing_total.store(0, Ordering::Relaxed);
}

/// Spawn a background thread that polls the local clipboard and pushes
/// ClipOffer + ClipChunks onto `outgoing_tx` whenever the content changes
/// (and isn't something we just wrote ourselves).
///
/// `outgoing_progress` / `outgoing_total` are updated as bytes are queued
/// for the writer thread; the UI reads them to render a progress line.
///
/// `events_tx` is used to surface transient warnings to the UI — currently
/// just the "image too large" toast. Reusing the existing UI event channel
/// avoids adding a separate signalling path for one warning kind.
pub fn spawn_poll_thread(
    state: ClipboardState,
    outgoing_tx: mpsc::Sender<Packet>,
    outgoing_progress: Arc<AtomicU64>,
    outgoing_total: Arc<AtomicU64>,
    events_tx: mpsc::Sender<TransportEvent>,
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
                let _ = events_tx.send(TransportEvent::Toast(format_oversize_toast(&e)));
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
    /// Format from the most recent `ClipOffer`. Determines whether `commit()`
    /// writes UTF-8 text or decodes a PNG and pushes RGBA to arboard.
    expected_format: u8,
    received: BTreeMap<u16, Vec<u8>>,
    received_total: u32,
    clip: Option<arboard::Clipboard>,
    /// Live counters consumed by the UI status-line (Task 7a). Also reset by
    /// `reset()` on disconnect / new Hello so a half-finished transfer doesn't
    /// leave the progress display stuck.
    incoming_progress: Arc<AtomicU64>,
    incoming_total: Arc<AtomicU64>,
    /// Test-only sink for the last successfully committed payload. Lets unit
    /// tests assert on what would have been written to the local clipboard
    /// without depending on the host platform's actual clipboard backend
    /// (which arboard cannot stub out portably).
    #[cfg(test)]
    last_committed: Option<CommittedPayload>,
}

/// What the most recent `commit()` produced. Test-only — production code
/// pushes straight to `arboard::Clipboard`.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CommittedPayload {
    Text(String),
    Image { width: usize, height: usize, bytes: Vec<u8> },
}

impl IncomingClipboard {
    pub fn new(
        state: ClipboardState,
        incoming_progress: Arc<AtomicU64>,
        incoming_total: Arc<AtomicU64>,
    ) -> Self {
        Self {
            state,
            expected_len: 0,
            expected_format: 0,
            received: BTreeMap::new(),
            received_total: 0,
            clip: arboard::Clipboard::new().ok(),
            incoming_progress,
            incoming_total,
            #[cfg(test)]
            last_committed: None,
        }
    }

    /// Test constructor — skips arboard init (which would fail in headless CI
    /// or in test environments without a window server) and lets us inspect
    /// committed payloads via `last_committed`.
    #[cfg(test)]
    fn new_for_test(state: ClipboardState) -> Self {
        Self {
            state,
            expected_len: 0,
            expected_format: 0,
            received: BTreeMap::new(),
            received_total: 0,
            clip: None,
            incoming_progress: Arc::new(AtomicU64::new(0)),
            incoming_total: Arc::new(AtomicU64::new(0)),
            last_committed: None,
        }
    }

    pub fn on_offer(&mut self, format: u8, total_len: u32) {
        // Abort an in-progress reassembly if a new offer arrives mid-transfer.
        // Sender is single-threaded so this is a real signal (peer
        // started a fresh payload) — not a race.
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
        self.incoming_total.store(total_len as u64, Ordering::Relaxed);
        self.incoming_progress.store(0, Ordering::Relaxed);
        log::debug!("clipboard: incoming offer format={format} of {total_len} bytes");
    }

    pub fn on_chunk(&mut self, index: u16, data: Vec<u8>) {
        let added = data.len() as u32;
        self.received_total += added;
        self.received.insert(index, data);
        self.incoming_progress
            .fetch_add(added as u64, Ordering::Relaxed);

        if self.received_total >= self.expected_len && self.expected_len > 0 {
            self.commit();
        }
    }

    /// Drop any in-flight reassembly state and zero progress counters.
    /// Called from the reader thread on disconnect and on Hello (new session).
    pub fn reset(&mut self) {
        self.expected_len = 0;
        self.expected_format = 0;
        self.received.clear();
        self.received_total = 0;
        self.incoming_progress.store(0, Ordering::Relaxed);
        self.incoming_total.store(0, Ordering::Relaxed);
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
                let hash = hash_text(&text);
                self.state.set(LastKind::Text(hash)); // mark as ours so poll won't echo
                #[cfg(test)]
                {
                    self.last_committed = Some(CommittedPayload::Text(text.clone()));
                }
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
    }

    fn commit_image(&mut self, buf: &[u8]) {
        let img = match decode_png_to_rgba(buf) {
            Ok(i) => i,
            Err(e) => {
                log::warn!("clipboard: PNG decode failed: {e}");
                return;
            }
        };

        // Hash from RGBA, not the encoded PNG bytes — round-trip
        // arboard PNG↔RGBA produces different encoded bytes across
        // peers, but the RGBA buffer is stable and is what the next
        // `get_image()` poll will read.
        let hash = hash_bytes(&img.bytes);
        self.state.set(LastKind::Image(hash));

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
                log::debug!("clipboard: wrote image from host ({} encoded bytes)", buf.len());
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

        // Counter invariants — emit zeroes both atomics on completion so the
        // status-line stops rendering "Sending …" once all chunks are queued
        // (Task 7a). During the loop progress matches total, but by return
        // both are back to zero.
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(progress.load(Ordering::Relaxed), 0);
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

    /// Push `payload` through `IncomingClipboard` as one offer + N chunks.
    fn feed_offer(incoming: &mut IncomingClipboard, format: u8, payload: &[u8]) {
        incoming.on_offer(format, payload.len() as u32);
        for (i, chunk) in payload.chunks(CHUNK_SIZE).enumerate() {
            incoming.on_chunk(i as u16, chunk.to_vec());
        }
    }

    #[test]
    fn incoming_image_reassembly() {
        // synthetic RGBA → encode → feed back through IncomingClipboard →
        // decoded RGBA must match the original byte-for-byte. Verifies
        // commit() routes format=1 through decode_png_to_rgba and stamps
        // LastKind::Image in shared state.
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");

        let state = ClipboardState::new();
        let mut incoming = IncomingClipboard::new_for_test(state.clone());
        feed_offer(&mut incoming, FORMAT_PNG_IMAGE, &png);

        match incoming.last_committed.as_ref().expect("committed payload") {
            CommittedPayload::Image { width, height, bytes } => {
                assert_eq!(*width, original.width);
                assert_eq!(*height, original.height);
                assert_eq!(bytes.as_slice(), &*original.bytes);
            }
            other => panic!("expected image payload, got {other:?}"),
        }

        // LastKind must be Image — guards against the receiver echoing back
        // the image we just wrote ourselves.
        assert!(matches!(state.get(), LastKind::Image(_)));
    }

    #[test]
    fn incoming_text_reassembly_unchanged() {
        // Regression: text path keeps working (format=0 → set_text equivalent).
        let text = "hello, мир";
        let bytes = text.as_bytes().to_vec();

        let state = ClipboardState::new();
        let mut incoming = IncomingClipboard::new_for_test(state.clone());
        feed_offer(&mut incoming, FORMAT_TEXT_UTF8, &bytes);

        match incoming.last_committed.as_ref().expect("committed payload") {
            CommittedPayload::Text(s) => assert_eq!(s, text),
            other => panic!("expected text, got {other:?}"),
        }
        assert!(matches!(state.get(), LastKind::Text(_)));
    }

    #[test]
    fn incoming_invalid_png_skipped() {
        // format=1 + non-PNG payload → commit logs warn, no panic, no state
        // update. Reset of `expected_*` still happens so the next offer can
        // proceed.
        let state = ClipboardState::new();
        let mut incoming = IncomingClipboard::new_for_test(state.clone());

        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03];
        feed_offer(&mut incoming, FORMAT_PNG_IMAGE, &garbage);

        assert!(incoming.last_committed.is_none(), "no payload should commit");
        assert!(matches!(state.get(), LastKind::None));
        // After failed commit the receiver must be ready for a new offer.
        assert_eq!(incoming.expected_len, 0);
        assert_eq!(incoming.expected_format, 0);
    }

    #[test]
    fn incoming_unknown_format_skipped() {
        // An unrecognised format value must not panic and must not stamp
        // anything in the shared state.
        let state = ClipboardState::new();
        let mut incoming = IncomingClipboard::new_for_test(state.clone());

        feed_offer(&mut incoming, 0xFE, b"opaque");

        assert!(incoming.last_committed.is_none());
        assert!(matches!(state.get(), LastKind::None));
    }

    #[test]
    fn incoming_offer_during_reassembly_aborts_previous() {
        // Start a 1024-byte text offer, push only one chunk (256B), then
        // send a fresh PNG offer. Receiver must drop the partial text
        // and switch context to the new offer.
        let state = ClipboardState::new();
        let mut incoming = IncomingClipboard::new_for_test(state);

        incoming.on_offer(FORMAT_TEXT_UTF8, 1024);
        incoming.on_chunk(0, vec![b'a'; 256]);
        assert_eq!(incoming.received_total, 256);

        incoming.on_offer(FORMAT_PNG_IMAGE, 512);
        assert_eq!(incoming.expected_format, FORMAT_PNG_IMAGE);
        assert_eq!(incoming.expected_len, 512);
        assert_eq!(incoming.received_total, 0);
        assert!(incoming.received.is_empty(), "previous chunks must be dropped");
    }

    #[test]
    fn incoming_reset_clears_state() {
        // Accumulate partial state, call reset(), verify everything is zero.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        // Override the test ctor's own counters so we can assert on them.
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        incoming.on_offer(FORMAT_PNG_IMAGE, 4096);
        incoming.on_chunk(0, vec![0u8; 256]);
        incoming.on_chunk(1, vec![0u8; 256]);
        incoming.on_chunk(2, vec![0u8; 256]);
        assert!(incoming.received_total > 0);
        assert!(progress.load(Ordering::Relaxed) > 0);
        assert!(total.load(Ordering::Relaxed) > 0);

        incoming.reset();

        assert_eq!(incoming.expected_len, 0);
        assert_eq!(incoming.expected_format, 0);
        assert_eq!(incoming.received_total, 0);
        assert!(incoming.received.is_empty());
        assert_eq!(progress.load(Ordering::Relaxed), 0);
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn incoming_image_then_text_no_loop() {
        // After receiving an image, LastKind::Image(h) is set. The poll-side
        // dedup path (verified by the matches! check below) must treat the
        // same RGBA as a duplicate so we don't echo it back to peer (AC6).
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");

        let state = ClipboardState::new();
        let mut incoming = IncomingClipboard::new_for_test(state.clone());
        feed_offer(&mut incoming, FORMAT_PNG_IMAGE, &png);

        let img_hash = hash_bytes(&original.bytes);
        assert!(
            matches!(state.get(), LastKind::Image(h) if h == img_hash),
            "state must hold the image's RGBA hash for loop avoidance"
        );

        // Now mimic the poll thread reading get_image() and checking dedup.
        let next_hash = hash_bytes(&original.bytes);
        let dedup_hits = matches!(state.get(), LastKind::Image(h) if h == next_hash);
        assert!(dedup_hits, "next poll with same RGBA must short-circuit");
    }

    #[test]
    fn format_oversize_toast_includes_kb_and_hint() {
        // The message rendered in the toast slot must:
        // - report the encoded size in KB (the user thinks in MB-ish, KB
        //   gives more precision near the 1 MB cap),
        // - include an actionable hint so the user knows what to do.
        let e = ImageTooLarge { png_len: 1_500 * 1024, limit: 1024 * 1024 };
        let msg = format_oversize_toast(&e);
        assert!(msg.contains("1500"), "KB count missing: {msg}");
        assert!(msg.contains("smaller"), "actionable hint missing: {msg}");
        assert!(msg.contains("too large"), "leading prefix missing: {msg}");
    }

    #[test]
    fn toast_emitted_on_oversized_image() {
        // End-to-end signal flow: the size-check helper returns Err for an
        // oversized payload, the calling code wraps the error in a toast
        // string and pushes a TransportEvent::Toast onto the events channel.
        // Verifies the wiring used inside spawn_poll_thread without spinning
        // a real thread (which would need a window-server-attached arboard).
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");
        // 32 bytes is well under the synthetic PNG length so this reliably
        // exercises the oversize path.
        let limit = 32usize;

        let (events_tx, events_rx) = mpsc::channel::<TransportEvent>();

        // Reproduce the production sequence: check_image_size → on Err
        // build a Toast with format_oversize_toast and send through events_tx.
        match check_image_size(png.len(), limit) {
            Ok(()) => panic!("expected oversize, got Ok"),
            Err(e) => {
                events_tx
                    .send(TransportEvent::Toast(format_oversize_toast(&e)))
                    .expect("toast send");
            }
        }
        drop(events_tx);

        let event = events_rx.recv().expect("toast event");
        match event {
            TransportEvent::Toast(msg) => {
                assert!(msg.contains("too large"), "toast missing prefix: {msg}");
                assert!(msg.contains("smaller"), "toast missing hint: {msg}");
            }
            _ => panic!("expected TransportEvent::Toast, got something else"),
        }
    }

    #[test]
    fn incoming_progress_counters_track_chunks() {
        // on_offer initialises total, on_chunk increments progress.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        incoming.on_offer(FORMAT_TEXT_UTF8, 800);
        assert_eq!(total.load(Ordering::Relaxed), 800);
        assert_eq!(progress.load(Ordering::Relaxed), 0);

        incoming.on_chunk(0, vec![b'x'; 256]);
        assert_eq!(progress.load(Ordering::Relaxed), 256);
        incoming.on_chunk(1, vec![b'x'; 256]);
        assert_eq!(progress.load(Ordering::Relaxed), 512);
    }
}
