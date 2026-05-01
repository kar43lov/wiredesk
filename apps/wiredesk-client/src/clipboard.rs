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

use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;

const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CHUNK_SIZE: usize = 256;
const FORMAT_TEXT_UTF8: u8 = 0;
const MAX_CLIPBOARD_BYTES: usize = 256 * 1024; // 256 KB cap

/// Shared state: last known clipboard hash. Updated when we either set or read
/// the local clipboard. Used to suppress re-sending content we just received.
#[derive(Clone, Default)]
pub struct ClipboardState {
    last_hash: Arc<Mutex<u64>>,
}

impl ClipboardState {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self) -> u64 {
        *self.last_hash.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, h: u64) {
        let mut g = self.last_hash.lock().unwrap_or_else(|e| e.into_inner());
        *g = h;
    }
}

fn hash_text(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
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
            if hash == state.get() {
                continue;
            }
            state.set(hash);

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
                self.state.set(hash); // mark as ours so poll won't echo
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
