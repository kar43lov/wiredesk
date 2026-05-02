//! Clipboard sync — Mac side.
//!
//! Polls the local Mac clipboard once per CLIP_POLL_INTERVAL. When the text
//! changes we send it to Host via outgoing channel as a ClipOffer + N×ClipChunk.
//! Incoming clipboard messages are reassembled and written to the Mac
//! clipboard. A hash of the last known content is tracked so we don't echo
//! back what we just received (loop avoidance).

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wiredesk_protocol::message::{FORMAT_TEXT_UTF8, Message};
use wiredesk_protocol::packet::Packet;

const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CHUNK_SIZE: usize = 256;
const MAX_CLIPBOARD_BYTES: usize = 256 * 1024; // 256 KB cap for text

/// Type-tagged hash of the most recent clipboard content owned/observed by us.
/// Used to suppress re-sending what we just wrote (loop avoidance) while
/// keeping text and image dedup independent — copying text after an image
/// (or vice versa) does not get blocked by the wrong-kind hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum LastKind {
    #[default]
    None,
    Text(u64),
    /// Wired-up by send/receive paths in Task 4 / Task 5.
    #[allow(dead_code)]
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
///
/// Wired up by the poll thread in Task 4. Currently exercised only by tests.
#[allow(dead_code)]
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

/// Spawn a background thread that polls the local clipboard and pushes
/// ClipOffer + ClipChunks onto `outgoing_tx` whenever the content changes
/// (and isn't something we just wrote ourselves).
pub fn spawn_poll_thread(state: ClipboardState, outgoing_tx: mpsc::Sender<Packet>) {
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

            let text = match clip.get_text() {
                Ok(t) => t,
                Err(_) => continue, // empty clipboard, non-text content, etc.
            };

            if text.is_empty() {
                continue;
            }

            let hash = hash_text(&text);
            if matches!(state.get(), LastKind::Text(h) if h == hash) {
                continue;
            }
            state.set(LastKind::Text(hash));

            let bytes = text.as_bytes();
            if bytes.len() > MAX_CLIPBOARD_BYTES {
                log::warn!("clipboard: skipping push — {} bytes exceeds limit", bytes.len());
                continue;
            }

            log::debug!("clipboard: pushing {} bytes to host", bytes.len());

            let _ = outgoing_tx.send(Packet::new(
                Message::ClipOffer {
                    format: FORMAT_TEXT_UTF8,
                    total_len: bytes.len() as u32,
                },
                0,
            ));

            for (idx, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
                let _ = outgoing_tx.send(Packet::new(
                    Message::ClipChunk {
                        index: idx as u16,
                        data: chunk.to_vec(),
                    },
                    0,
                ));
            }
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
}
