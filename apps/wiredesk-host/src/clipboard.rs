//! Clipboard sync — Windows side.
//!
//! Symmetric with the client: poll local clipboard, push changes to the peer
//! as ClipOffer + ClipChunks; reassemble incoming and write to local.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use wiredesk_protocol::message::Message;

const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const CHUNK_SIZE: usize = 256;
pub const FORMAT_TEXT_UTF8: u8 = 0;
const MAX_CLIPBOARD_BYTES: usize = 256 * 1024;

fn hash_text(s: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

pub struct ClipboardSync {
    clip: Option<arboard::Clipboard>,
    last_hash: u64,
    last_poll: Instant,
    incoming_expected: u32,
    incoming_total: u32,
    incoming_chunks: BTreeMap<u16, Vec<u8>>,
}

impl ClipboardSync {
    pub fn new() -> Self {
        Self {
            clip: arboard::Clipboard::new().ok(),
            last_hash: 0,
            last_poll: Instant::now(),
            incoming_expected: 0,
            incoming_total: 0,
            incoming_chunks: BTreeMap::new(),
        }
    }

    /// Called from session.tick(). Returns a list of messages to send if the
    /// local clipboard changed since last poll.
    pub fn poll(&mut self) -> Vec<Message> {
        if self.last_poll.elapsed() < CLIP_POLL_INTERVAL {
            return Vec::new();
        }
        self.last_poll = Instant::now();

        let Some(clip) = self.clip.as_mut() else {
            return Vec::new();
        };
        let text = match clip.get_text() {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };

        if text.is_empty() {
            return Vec::new();
        }

        let hash = hash_text(&text);
        if hash == self.last_hash {
            return Vec::new();
        }
        self.last_hash = hash;

        let bytes = text.as_bytes();
        if bytes.len() > MAX_CLIPBOARD_BYTES {
            log::warn!("clipboard: skipping push — {} bytes exceeds limit", bytes.len());
            return Vec::new();
        }

        log::debug!("clipboard: pushing {} bytes to client", bytes.len());

        let mut msgs = Vec::with_capacity(1 + bytes.len() / CHUNK_SIZE + 1);
        msgs.push(Message::ClipOffer {
            format: FORMAT_TEXT_UTF8,
            total_len: bytes.len() as u32,
        });
        for (idx, chunk) in bytes.chunks(CHUNK_SIZE).enumerate() {
            msgs.push(Message::ClipChunk {
                index: idx as u16,
                data: chunk.to_vec(),
            });
        }
        msgs
    }

    pub fn on_offer(&mut self, total_len: u32) {
        self.incoming_expected = total_len;
        self.incoming_total = 0;
        self.incoming_chunks.clear();
    }

    pub fn on_chunk(&mut self, index: u16, data: Vec<u8>) {
        self.incoming_total += data.len() as u32;
        self.incoming_chunks.insert(index, data);

        if self.incoming_total >= self.incoming_expected && self.incoming_expected > 0 {
            self.commit();
        }
    }

    fn commit(&mut self) {
        let mut buf = Vec::with_capacity(self.incoming_expected as usize);
        for (_, chunk) in std::mem::take(&mut self.incoming_chunks) {
            buf.extend_from_slice(&chunk);
        }

        match String::from_utf8(buf) {
            Ok(text) => {
                self.last_hash = hash_text(&text); // mark as ours
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

        self.incoming_expected = 0;
        self.incoming_total = 0;
    }
}
