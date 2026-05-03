//! Clipboard sync — Mac side.
//!
//! Polls the local Mac clipboard once per CLIP_POLL_INTERVAL. When the text
//! changes we send it to Host via outgoing channel as a ClipOffer + N×ClipChunk.
//! Incoming clipboard messages are reassembled and written to the Mac
//! clipboard. A hash of the last known content is tracked so we don't echo
//! back what we just received (loop avoidance).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Hashes of the most recent clipboard content we observed/wrote, kept
/// in **independent slots per kind**. Without per-kind slots an alternating
/// text/image clipboard (e.g., a Whispr Flow dictation app writes text
/// while a screenshot stays on the OS clipboard) would loop: each text
/// write erases the image hash → next poll sees image as "new" → resends.
/// Bug captured in `2026-05-03 09:24` log session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LastSeen {
    pub text: Option<u64>,
    /// Successfully sent/received image hash (over RGBA bytes).
    pub image: Option<u64>,
    /// RGBA hash of the most recent image rejected by the size cap. Lets the
    /// poll thread short-circuit the expensive RGBA→PNG re-encode (and the
    /// repeated toast emission) for the same buffer on every 500 ms tick —
    /// AC4 expects one toast per oversize event, not one per poll.
    pub oversize_image: Option<u64>,
}

impl LastSeen {
    /// True when the given RGBA hash matches either the last sent/received
    /// image OR the last oversize-rejected image. Poll path uses this to
    /// skip the expensive RGBA→PNG re-encode for the same buffer.
    pub(crate) fn matches_image_hash(&self, hash: u64) -> bool {
        self.image == Some(hash) || self.oversize_image == Some(hash)
    }

    pub(crate) fn matches_text_hash(&self, hash: u64) -> bool {
        self.text == Some(hash)
    }
}

/// Legacy enum kept for unit tests that still use `state.set(LastKind::*)`.
/// Production code uses `LastSeen` and the per-kind setters directly.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // some variants are no longer used after the LastSeen split
pub(crate) enum LastKind {
    None,
    Text(u64),
    Image(u64),
    OversizeImage(u64),
}

/// Shared state: per-kind hashes of the last observed clipboard content.
#[derive(Clone, Default)]
pub struct ClipboardState {
    last: Arc<Mutex<LastSeen>>,
}

impl ClipboardState {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn get(&self) -> LastSeen {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn set_text(&self, hash: u64) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        g.text = Some(hash);
    }

    pub(crate) fn set_image(&self, hash: u64) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        g.image = Some(hash);
        // A successful image send/receive also clears any prior
        // oversize-stamp for the same buffer — the buffer's now considered
        // delivered, not rejected. (Different hash → no-op.)
        if g.oversize_image == Some(hash) {
            g.oversize_image = None;
        }
    }

    pub(crate) fn set_oversize_image(&self, hash: u64) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        g.oversize_image = Some(hash);
    }

    /// Test-only legacy setter. Maps `LastKind` variants onto the per-kind
    /// `LastSeen` slots so existing tests don't have to rewrite call sites.
    #[cfg(test)]
    pub(crate) fn set(&self, kind: LastKind) {
        match kind {
            LastKind::None => self.reset(),
            LastKind::Text(h) => self.set_text(h),
            LastKind::Image(h) => self.set_image(h),
            LastKind::OversizeImage(h) => self.set_oversize_image(h),
        }
    }

    /// Clear ALL sender-side dedup hashes. Called from the reader thread on
    /// disconnect / new HelloAck / transport error so that a mid-transfer
    /// abort doesn't leave a stale stamp — otherwise the very next poll-tick
    /// after reconnect would see the same OS-clipboard content, match the
    /// hash, and skip the resend (silent lost-update). Trade-off: after a
    /// brief disconnect both sides resend their current clipboards (each
    /// thinks the other doesn't have it) — better than a lost update.
    pub fn reset(&self) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        *g = LastSeen::default();
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
}

pub(crate) fn check_image_size(png_len: usize, limit: usize) -> Result<(), ImageTooLarge> {
    if png_len > limit {
        Err(ImageTooLarge { png_len })
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

/// 64 MB upper bound on decoder allocations. This is the single source of
/// truth for "safe to decode": any PNG whose decoded RGBA buffer would exceed
/// this budget is rejected, regardless of per-axis dimensions.
///
/// Codex iter6: a per-axis dimension cap (previously 4096) was strictly more
/// restrictive than the 64 MB budget — it rejected legitimate widescreen /
/// high-resolution screenshots (5K Retina = 5120×2880×4 ≈ 58.6 MB, well
/// inside the budget). We dropped the per-axis cap and rely on the alloc
/// budget alone, with an explicit post-decode check for `to_rgba8()` (which
/// allocates `width * height * 4` independent of the decoder's `Limits`).
const DECODE_MAX_ALLOC: u64 = 64 * 1024 * 1024;

/// Decode PNG bytes to an arboard `ImageData` (RGBA8, owned).
///
/// Codex iter2 D2 + iter3 E1 + iter6: caps decoded allocations so a PNG bomb
/// (e.g. palette image expanding to hundreds of MB of RGBA) cannot blow up
/// memory. `image::Limits.max_alloc` covers the decoder's internal buffers;
/// the explicit post-decode `(w * h * 4) > DECODE_MAX_ALLOC` check covers the
/// fresh `to_rgba8()` allocation, which is independent of `Limits`.
fn decode_png_to_rgba(bytes: &[u8]) -> Result<arboard::ImageData<'static>, image::ImageError> {
    use image::GenericImageView;
    use std::io::Cursor;
    let mut limits = image::Limits::default();
    // No max_image_width / max_image_height — alloc budget is the real cap.
    limits.max_alloc = Some(DECODE_MAX_ALLOC);
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    reader.limits(limits);
    let dyn_img = reader.decode()?;
    let (w, h) = dyn_img.dimensions();
    let alloc = (w as u64)
        .saturating_mul(h as u64)
        .saturating_mul(4);
    if alloc > DECODE_MAX_ALLOC {
        return Err(image::ImageError::Limits(
            image::error::LimitError::from_kind(image::error::LimitErrorKind::InsufficientMemory),
        ));
    }
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(rgba.into_raw()),
    })
}

/// Push a single clipboard payload (text bytes or encoded PNG) as a
/// `ClipOffer` followed by N×`ClipChunk`.
///
/// Progress accounting is intentionally NOT done here. mpsc is unbounded,
/// so packets sit in the channel for many seconds at 11 KB/s wire-throughput.
/// If the poll thread incremented counters here, UI would jump to ~100%
/// instantly and never reflect the actual send progress (AC5 violated).
/// The writer thread (`writer_thread` in main.rs) is the sole place that
/// updates `outgoing_progress` / `outgoing_total` — see
/// `apply_outgoing_progress` for the dispatch logic.
///
/// Pure helper (no thread, no clipboard backend) — used by both the
/// production poll thread and unit tests.
fn emit_offer_and_chunks(outgoing_tx: &mpsc::Sender<Packet>, format: u8, payload: &[u8]) {
    let _ = outgoing_tx.send(Packet::new(
        Message::ClipOffer {
            format,
            total_len: payload.len() as u32,
        },
        0,
    ));

    for (idx, chunk) in payload.chunks(CHUNK_SIZE).enumerate() {
        let _ = outgoing_tx.send(Packet::new(
            Message::ClipChunk {
                index: idx as u16,
                data: chunk.to_vec(),
            },
            0,
        ));
    }
}

/// Dispatch helper called by `writer_thread` for each packet AFTER it is
/// successfully written to the wire. Updates outgoing progress counters so
/// the UI sees real wire-state progress (AC5: ≥2 increments visible during
/// a typical 500 KB send at 11 KB/s).
///
/// - `ClipOffer`: reset progress to 0 and store new total. Counter reset
///   must happen BEFORE the offer is sent on the wire so a previous
///   completed transfer's "100%" doesn't linger past the new offer.
///   (We're already past `transport.send` here — the brief flicker between
///   reset and the first chunk is bounded by 11 KB/s wire pacing.)
/// - `ClipChunk`: add chunk length to progress.
/// - Other packets: no-op.
pub(crate) fn apply_outgoing_progress(
    msg: &Message,
    outgoing_progress: &Arc<AtomicU64>,
    outgoing_total: &Arc<AtomicU64>,
) {
    match msg {
        Message::ClipOffer { format, total_len } => {
            outgoing_total.store(*total_len as u64, Ordering::Relaxed);
            outgoing_progress.store(0, Ordering::Relaxed);
            log::info!("clipboard.send START format={format} total={total_len} bytes");
        }
        Message::ClipChunk { data, .. } => {
            let prev = outgoing_progress.fetch_add(data.len() as u64, Ordering::Relaxed);
            let new_progress = prev + data.len() as u64;
            let total = outgoing_total.load(Ordering::Relaxed);
            // Milestone logging — every 25% of total.
            if total > 0 {
                let prev_q = (prev * 4) / total;
                let new_q = (new_progress * 4) / total;
                if new_q > prev_q {
                    log::info!(
                        "clipboard.send {}/{} bytes ({}%)",
                        new_progress,
                        total,
                        (new_progress * 100) / total
                    );
                }
            }
            // Codex C4: when the last chunk hits the total, zero both
            // counters so the status-line stops showing "Sending clipboard
            // — 1024/1024 KB (100%)" until the next transfer or
            // disconnect. Without this the UI sticks at 100% indefinitely.
            if total > 0 && new_progress >= total {
                log::info!("clipboard.send DONE {} bytes", new_progress);
                outgoing_total.store(0, Ordering::Relaxed);
                outgoing_progress.store(0, Ordering::Relaxed);
            }
        }
        _ => {}
    }
}

/// Spawn a background thread that polls the local clipboard and pushes
/// ClipOffer + ClipChunks onto `outgoing_tx` whenever the content changes
/// (and isn't something we just wrote ourselves).
///
/// Outgoing progress counters live on the writer thread now (M3 fix) — the
/// poll thread only enqueues packets, the writer thread updates atomics
/// after each successful `transport.send`.
///
/// `events_tx` is used to surface transient warnings to the UI — currently
/// just the "image too large" toast. Reusing the existing UI event channel
/// avoids adding a separate signalling path for one warning kind.
pub fn spawn_poll_thread(
    state: ClipboardState,
    outgoing_tx: mpsc::Sender<Packet>,
    events_tx: mpsc::Sender<TransportEvent>,
    send_images: Arc<AtomicBool>,
    send_text: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let mut clip = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("clipboard: failed to init: {e}");
                return;
            }
        };

        // Startup pre-stamp: whatever's already on the clipboard when the
        // app launches gets its hash recorded as `LastSeen.{text,image}`
        // WITHOUT being sent to the peer. Without this, every restart
        // re-uploads the user's current clipboard contents (including
        // any image copied during a previous session). The user's NEXT
        // genuine Cmd+C produces a different hash and triggers normal
        // sync. Image hash is over RGBA bytes (consistent with the
        // runtime poll path). Stamp BOTH text and image — the OS
        // clipboard can hold both (NSPasteboard supports multiple types
        // for one copy).
        if let Ok(text) = clip.get_text() {
            if !text.is_empty() {
                state.set_text(hash_text(&text));
                log::info!(
                    "clipboard: pre-stamped existing text ({} bytes) — not sending on startup",
                    text.len()
                );
            }
        }
        if let Ok(img) = clip.get_image() {
            state.set_image(hash_bytes(&img.bytes));
            log::info!(
                "clipboard: pre-stamped existing image ({}x{}) — not sending on startup",
                img.width,
                img.height
            );
        }

        loop {
            thread::sleep(CLIP_POLL_INTERVAL);

            // 1) Probe text. Independent dedup slot (`LastSeen.text`) so
            // an alternating text/image clipboard (Whispr Flow + a
            // standing screenshot) doesn't loop. Runtime toggle gates
            // the path entirely.
            if send_text.load(Ordering::Relaxed) {
                if let Ok(text) = clip.get_text() {
                    if !text.is_empty() {
                        let hash = hash_text(&text);
                        if !state.get().matches_text_hash(hash) {
                            state.set_text(hash);
                            let bytes = text.as_bytes();
                            if bytes.len() > MAX_CLIPBOARD_BYTES {
                                log::warn!(
                                    "clipboard: skipping push — {} bytes exceeds limit",
                                    bytes.len()
                                );
                            } else {
                                log::debug!(
                                    "clipboard: pushing {} bytes to host",
                                    bytes.len()
                                );
                                emit_offer_and_chunks(
                                    &outgoing_tx,
                                    FORMAT_TEXT_UTF8,
                                    bytes,
                                );
                            }
                        }
                    }
                }
            }

            // 2) Probe image. Independent dedup slot. Runtime toggle gates.
            // Note: probing both text AND image in the same tick (instead
            // of falling through only on text-empty) is intentional — the
            // OS clipboard can hold both. This closes the codex C3 gap.
            if !send_images.load(Ordering::Relaxed) {
                continue;
            }
            let img = match clip.get_image() {
                Ok(i) => i,
                Err(_) => continue, // not an image
            };

            let hash = hash_bytes(&img.bytes);
            // Short-circuit BEFORE the expensive RGBA→PNG encode for both:
            // - already-sent images (LastSeen.image),
            // - already-rejected oversized images (LastSeen.oversize_image).
            // Otherwise every 500 ms tick re-encodes (~30-150 ms CPU) and
            // re-emits the toast for the SAME oversize buffer.
            if state.get().matches_image_hash(hash) {
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
                    MAX_IMAGE_BYTES
                );
                let _ = events_tx.send(TransportEvent::Toast(format_oversize_toast(&e)));
                // Stamp the RGBA hash so the next 500 ms tick short-circuits
                // for the same image. A new RGBA (user re-copied) gives a new
                // hash and re-tries the encode path.
                state.set_oversize_image(hash);
                continue;
            }

            // Codex iter3 E2 (acceptable): sender dedup is set on enqueue,
            // not on successful send. If transport fails mid-transfer, retry
            // happens only when clipboard content changes again. Acceptable:
            // heartbeat covers disconnect within 6s, app restart clears state.
            state.set_image(hash);

            log::debug!("clipboard: pushing image to host ({} encoded bytes)", png.len());
            emit_offer_and_chunks(&outgoing_tx, FORMAT_PNG_IMAGE, &png);
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
    /// Runtime toggle (Settings → Receive images): when off, image offers
    /// (`format=FORMAT_PNG_IMAGE`) are rejected on receipt. Text offers
    /// continue to be processed normally.
    receive_images: Arc<AtomicBool>,
    /// Runtime toggle (Settings → Receive text): when off, text offers
    /// (`format=FORMAT_TEXT_UTF8`) are rejected on receipt.
    receive_text: Arc<AtomicBool>,
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
        receive_images: Arc<AtomicBool>,
        receive_text: Arc<AtomicBool>,
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
            receive_images,
            receive_text,
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
            receive_images: Arc::new(AtomicBool::new(true)),
            receive_text: Arc::new(AtomicBool::new(true)),
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
        // Reject unknown format values BEFORE arming reassembly. Without this
        // a peer could send `ClipOffer { format=99, total_len=u32::MAX }` and
        // we'd accept up to 4 GB of chunks (the per-format size cap below
        // only fires for known formats). Reset state and bail out — chunks
        // for the unknown format will hit the expected_len==0 guard in
        // on_chunk and be dropped.
        if format != FORMAT_TEXT_UTF8 && format != FORMAT_PNG_IMAGE {
            log::warn!(
                "clipboard: incoming offer with unsupported format {format}, ignoring"
            );
            self.expected_len = 0;
            self.expected_format = 0;
            self.received.clear();
            self.received_total = 0;
            self.incoming_total.store(0, Ordering::Relaxed);
            self.incoming_progress.store(0, Ordering::Relaxed);
            return;
        }
        // Runtime toggle (Settings → Receive images): drop incoming image
        // offers when the user disabled image receive. Text offers continue.
        if format == FORMAT_TEXT_UTF8 && !self.receive_text.load(Ordering::Relaxed) {
            log::info!(
                "clipboard: incoming text offer ({total_len} bytes) ignored — receive_text disabled"
            );
            self.expected_len = 0;
            self.expected_format = 0;
            self.received.clear();
            self.received_total = 0;
            self.incoming_total.store(0, Ordering::Relaxed);
            self.incoming_progress.store(0, Ordering::Relaxed);
            return;
        }
        if format == FORMAT_PNG_IMAGE && !self.receive_images.load(Ordering::Relaxed) {
            log::info!(
                "clipboard: incoming image offer ({total_len} bytes) ignored — receive_images disabled"
            );
            self.expected_len = 0;
            self.expected_format = 0;
            self.received.clear();
            self.received_total = 0;
            self.incoming_total.store(0, Ordering::Relaxed);
            self.incoming_progress.store(0, Ordering::Relaxed);
            return;
        }
        // Bound peer-supplied total_len to local caps before allocating any
        // state. Without this a malicious or buggy peer could ask us to
        // allocate up to 4 GB inside `commit()` (Vec::with_capacity).
        let total_len_usize = total_len as usize;
        let over_cap = match format {
            FORMAT_PNG_IMAGE => total_len_usize > MAX_IMAGE_BYTES,
            FORMAT_TEXT_UTF8 => total_len_usize > MAX_CLIPBOARD_BYTES,
            _ => false,
        };
        if over_cap {
            log::warn!(
                "clipboard: incoming offer too large (format={format}, {total_len} bytes), ignoring"
            );
            // Leave reassembly state reset — chunks for this oversized offer
            // will be dropped by on_chunk's expected_len==0 guard.
            self.expected_len = 0;
            self.expected_format = 0;
            self.received.clear();
            self.received_total = 0;
            self.incoming_total.store(0, Ordering::Relaxed);
            self.incoming_progress.store(0, Ordering::Relaxed);
            return;
        }
        self.expected_len = total_len;
        self.expected_format = format;
        self.received.clear();
        self.received_total = 0;
        self.incoming_total.store(total_len as u64, Ordering::Relaxed);
        self.incoming_progress.store(0, Ordering::Relaxed);
        log::info!("clipboard.recv START format={format} total={total_len} bytes");
    }

    pub fn on_chunk(&mut self, index: u16, data: Vec<u8>) {
        // Drop chunks that arrive without (or after) an active offer:
        // - oversized offer was rejected (expected_len stays 0),
        // - chunks arrive before any offer,
        // - chunks arrive after a successful commit() zeroed expected_len.
        // Without this guard, BTreeMap::insert grows unbounded (memory leak).
        if self.expected_len == 0 {
            log::warn!("clipboard.recv chunk idx={index} dropped (no active offer)");
            return;
        }

        let added = data.len() as u32;
        // Only count this chunk if its index hasn't been seen before.
        // BTreeMap::insert silently overwrites duplicates, which would let
        // a duplicated index pump received_total over expected_len with a
        // truncated buffer — silent corruption.
        if self.received.insert(index, data).is_none() {
            // saturating_add: a malicious peer could otherwise overflow u32
            // by spamming chunks past expected_len before the >= guard fires.
            let prev_total = self.received_total;
            self.received_total = self.received_total.saturating_add(added);
            self.incoming_progress
                .fetch_add(added as u64, Ordering::Relaxed);
            // Milestone logging — every 25% of expected_len. Helps diagnose
            // mid-transfer stalls without spamming the log on every chunk.
            if self.expected_len > 0 {
                let prev_q = (prev_total * 4) / self.expected_len.max(1);
                let new_q = (self.received_total * 4) / self.expected_len.max(1);
                if new_q > prev_q {
                    log::info!(
                        "clipboard.recv {}/{} bytes ({}%)",
                        self.received_total,
                        self.expected_len,
                        (self.received_total * 100) / self.expected_len.max(1)
                    );
                }
            }
        }

        if self.received_total >= self.expected_len {
            log::info!(
                "clipboard.recv DONE {} bytes ({} chunks) → commit",
                self.received_total,
                self.received.len()
            );
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
        // Codex C2: verify chunk indices form a contiguous 0..N sequence
        // BEFORE concatenation. Without this guard a peer can drop chunk 3
        // and send chunk 7 of the same size — `received_total` reaches
        // `expected_len` (so commit fires) but the buffer has gaps with
        // later chunks shifted, silently corrupting the payload. Refuse to
        // commit on non-contiguous indices and reset state.
        let n = self.received.len();
        let contiguous = self.received.keys().enumerate().all(|(i, k)| *k as usize == i);
        if !contiguous {
            log::warn!(
                "clipboard: non-contiguous chunk indices ({n} chunks, expected 0..{n}), dropping payload"
            );
            self.reset();
            return;
        }

        let mut buf = Vec::with_capacity(self.expected_len as usize);
        for (_, chunk) in std::mem::take(&mut self.received) {
            buf.extend_from_slice(&chunk);
        }

        // Codex iter2 D1: even with the duplicate-index guard in on_chunk,
        // a peer can replace chunk K's stored bytes via BTreeMap::insert
        // overwrite with a different length — received_total tracked only
        // the first arrival, so the actual reassembled buffer length may
        // diverge from expected_len. Verify before decoding.
        if buf.len() as u32 != self.expected_len {
            log::warn!(
                "clipboard: reassembled length mismatch (got {} bytes, expected {}), dropping payload",
                buf.len(),
                self.expected_len,
            );
            self.reset();
            return;
        }

        match self.expected_format {
            FORMAT_TEXT_UTF8 => self.commit_text(buf),
            FORMAT_PNG_IMAGE => self.commit_image(&buf),
            other => {
                log::warn!("clipboard: unknown format {other}, skipping {} bytes", buf.len());
            }
        }

        // Single source of truth for state-zeroing. `received` is already
        // empty here (mem::take above), so reset()'s clear() is a no-op.
        self.reset();
    }

    fn commit_text(&mut self, buf: Vec<u8>) {
        match String::from_utf8(buf) {
            Ok(text) => {
                let hash = hash_text(&text);
                #[cfg(test)]
                {
                    self.last_committed = Some(CommittedPayload::Text(text.clone()));
                }
                // Codex iter3 E3: write the OS clipboard FIRST, then mark
                // hash as "ours" only on success. If set_text fails, the
                // OS clipboard still holds the old value — marking the
                // hash early would cause the next poll to short-circuit
                // and we'd never re-send the (still-stale) old content.
                // Leaving last unchanged lets poll detect any real change.
                let mut wrote_ok = self.clip.is_none(); // no backend → treat as "ours"
                if let Some(clip) = self.clip.as_mut() {
                    match clip.set_text(text.clone()) {
                        Ok(()) => {
                            log::debug!("clipboard: wrote {} bytes from host", text.len());
                            wrote_ok = true;
                        }
                        Err(e) => log::warn!("clipboard: set_text failed: {e}"),
                    }
                }
                if wrote_ok {
                    self.state.set_text(hash);
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

        #[cfg(test)]
        {
            self.last_committed = Some(CommittedPayload::Image {
                width: img.width,
                height: img.height,
                bytes: img.bytes.to_vec(),
            });
        }

        // Codex iter3 E3: write OS clipboard FIRST, mark hash on success.
        // If set_image fails the OS clipboard still holds the old value;
        // marking early would suppress the next poll from re-detecting the
        // stale content and we'd loop forever silently.
        let mut wrote_ok = self.clip.is_none(); // no backend (tests) → ok
        if let Some(clip) = self.clip.as_mut() {
            match clip.set_image(img) {
                Ok(()) => {
                    log::debug!("clipboard: wrote image from host ({} encoded bytes)", buf.len());
                    wrote_ok = true;
                }
                Err(e) => log::warn!("clipboard: set_image failed: {e}"),
            }
        }
        if wrote_ok {
            self.state.set_image(hash);
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
    fn last_kind_default_is_none() {
        let state = ClipboardState::new();
        let s = state.get();
        assert!(s.text.is_none() && s.image.is_none() && s.oversize_image.is_none());
    }

    #[test]
    fn clipboard_state_reset_clears_last_kind() {
        // Codex iter4 F1: `reset()` is the disconnect-side hook that drops
        // sender dedup. Without it, after a transfer aborts mid-stream the
        // hash stays stamped and the post-reconnect tick dedups → silent
        // lost-update. Verify each slot collapses to None.
        let state = ClipboardState::new();

        state.set_text(0xAABB_CCDD);
        assert_eq!(state.get().text, Some(0xAABB_CCDD));
        state.reset();
        assert!(state.get().text.is_none());

        state.set_image(0x1122_3344);
        assert_eq!(state.get().image, Some(0x1122_3344));
        state.reset();
        assert!(state.get().image.is_none());

        state.set_oversize_image(0x9999);
        assert_eq!(state.get().oversize_image, Some(0x9999));
        state.reset();
        assert!(state.get().oversize_image.is_none());
    }

    #[test]
    fn text_and_image_slots_are_independent() {
        // Regression for the Whispr Flow loop: stamping text must NOT
        // erase the image hash (or vice versa). Without this the poll
        // thread bounces between text and image dedup forever.
        let state = ClipboardState::new();
        state.set_text(0x1111);
        state.set_image(0x2222);
        let s = state.get();
        assert_eq!(s.text, Some(0x1111));
        assert_eq!(s.image, Some(0x2222));

        state.set_text(0x3333); // text update
        let s = state.get();
        assert_eq!(s.text, Some(0x3333));
        assert_eq!(s.image, Some(0x2222), "image hash must survive text update");
    }

    #[test]
    fn disconnect_clears_sender_dedup() {
        // Models the main.rs reader_thread sequence on disconnect: the same
        // hash that was JUST stamped (and would normally cause the next
        // poll-tick to skip the resend) must be cleared so the post-reconnect
        // tick goes through. Drives the `clipboard_state.reset()` calls at
        // HelloAck / Disconnect / transport-error sites in main.rs.
        let state = ClipboardState::new();
        let payload = "the user copied this right before the link dropped";
        let hash = hash_text(payload);

        // poll-tick stamps after sending
        state.set(LastKind::Text(hash));
        // ... link drops mid-stream, reader_thread observes the disconnect
        state.reset();

        // post-reconnect tick: same OS-clipboard content. Without reset, the
        // text hash slot would still match. With reset, it's None.
        assert!(
            !state.get().matches_text_hash(hash),
            "reset must re-arm the sender for resend after reconnect"
        );
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
    }

    #[test]
    fn image_emit_offer_and_chunks() {
        // Drive the pure emitter without a thread / clipboard backend.
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");

        let (tx, rx) = mpsc::channel::<Packet>();

        emit_offer_and_chunks(&tx, FORMAT_PNG_IMAGE, &png);
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
    }

    #[test]
    fn image_emit_chunks_respect_chunk_size() {
        // Build a payload longer than CHUNK_SIZE so we get >1 chunk.
        let payload: Vec<u8> = (0..(CHUNK_SIZE * 3 + 7)).map(|i| (i & 0xFF) as u8).collect();

        let (tx, rx) = mpsc::channel::<Packet>();

        emit_offer_and_chunks(&tx, FORMAT_PNG_IMAGE, &payload);
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

    #[test]
    fn apply_outgoing_progress_offer_resets_and_sets_total() {
        // Writer-thread dispatch: ClipOffer must reset progress and store new total.
        let progress = Arc::new(AtomicU64::new(999));
        let total = Arc::new(AtomicU64::new(0));

        let msg = Message::ClipOffer { format: FORMAT_PNG_IMAGE, total_len: 1234 };
        apply_outgoing_progress(&msg, &progress, &total);

        assert_eq!(progress.load(Ordering::Relaxed), 0, "offer must reset progress");
        assert_eq!(total.load(Ordering::Relaxed), 1234, "offer must store total");
    }

    #[test]
    fn apply_outgoing_progress_chunk_increments() {
        // Writer-thread dispatch: ClipChunk bumps progress by data.len().
        // Mid-transfer chunks do NOT touch total.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(1024));

        let msg1 = Message::ClipChunk { index: 0, data: vec![0u8; 256] };
        apply_outgoing_progress(&msg1, &progress, &total);
        assert_eq!(progress.load(Ordering::Relaxed), 256);

        let msg2 = Message::ClipChunk { index: 1, data: vec![0u8; 200] };
        apply_outgoing_progress(&msg2, &progress, &total);
        assert_eq!(progress.load(Ordering::Relaxed), 456);
        // Total untouched by mid-transfer chunks.
        assert_eq!(total.load(Ordering::Relaxed), 1024);
    }

    #[test]
    fn apply_outgoing_progress_terminal_chunk_zeros_counters() {
        // Codex C4: the chunk that pushes progress >= total (transfer
        // complete) MUST clear both counters so the status-line stops
        // rendering "Sending clipboard — 100%". Without this the UI sticks
        // until the next transfer or disconnect.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(512));

        // Mid-transfer chunk: 256 bytes. Counters keep climbing.
        let msg1 = Message::ClipChunk { index: 0, data: vec![0u8; 256] };
        apply_outgoing_progress(&msg1, &progress, &total);
        assert_eq!(progress.load(Ordering::Relaxed), 256);
        assert_eq!(total.load(Ordering::Relaxed), 512);

        // Terminal chunk: another 256 bytes. progress == total → zero both.
        let msg2 = Message::ClipChunk { index: 1, data: vec![0u8; 256] };
        apply_outgoing_progress(&msg2, &progress, &total);
        assert_eq!(progress.load(Ordering::Relaxed), 0, "terminal chunk must zero progress");
        assert_eq!(total.load(Ordering::Relaxed), 0, "terminal chunk must zero total");
    }

    #[test]
    fn apply_outgoing_progress_overshoot_chunk_zeros_counters() {
        // Defensive: if a peer sent a slightly-larger-than-expected last
        // chunk (or rounding pushes us past total), still zero out — the
        // UI must not stick at >100%.
        let progress = Arc::new(AtomicU64::new(400));
        let total = Arc::new(AtomicU64::new(512));

        let msg = Message::ClipChunk { index: 5, data: vec![0u8; 200] };
        apply_outgoing_progress(&msg, &progress, &total);
        assert_eq!(progress.load(Ordering::Relaxed), 0);
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn emit_then_dispatch_clears_counters_after_last_chunk() {
        // End-to-end: emit_offer_and_chunks → drain channel → run each
        // packet through apply_outgoing_progress (mirroring the real
        // writer-thread loop). After the last chunk, both counters must be
        // zero (Codex C4: stuck-progress fix).
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");

        let (tx, rx) = mpsc::channel::<Packet>();
        emit_offer_and_chunks(&tx, FORMAT_PNG_IMAGE, &png);
        drop(tx);

        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));

        // Walk the queue the way writer_thread does.
        while let Ok(p) = rx.recv() {
            apply_outgoing_progress(&p.message, &progress, &total);
        }

        // After the final chunk, counters are cleared so the status-line
        // hides immediately rather than sticking at 100%.
        assert_eq!(
            progress.load(Ordering::Relaxed),
            0,
            "progress must be zero after last chunk dispatched"
        );
        assert_eq!(
            total.load(Ordering::Relaxed),
            0,
            "total must be zero after last chunk dispatched"
        );
    }

    #[test]
    fn apply_outgoing_progress_other_msgs_noop() {
        // Writer-thread dispatch: non-clipboard messages must not touch counters.
        let progress = Arc::new(AtomicU64::new(42));
        let total = Arc::new(AtomicU64::new(99));

        apply_outgoing_progress(&Message::Heartbeat, &progress, &total);
        apply_outgoing_progress(
            &Message::MouseMove { x: 100, y: 100 },
            &progress,
            &total,
        );

        assert_eq!(progress.load(Ordering::Relaxed), 42);
        assert_eq!(total.load(Ordering::Relaxed), 99);
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
        assert!(state.get().image.is_some());
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
        assert!(state.get().text.is_some());
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
        let s = state.get();
        assert!(s.text.is_none() && s.image.is_none() && s.oversize_image.is_none());
        // After failed commit the receiver must be ready for a new offer.
        assert_eq!(incoming.expected_len, 0);
        assert_eq!(incoming.expected_format, 0);
    }

    #[test]
    fn incoming_invalid_text_skipped() {
        // format=0 with non-UTF-8 bytes must NOT panic, NOT commit, and
        // leave the dedup state at None so a subsequent valid push can
        // proceed. Mirrors `incoming_invalid_png_skipped` for the text path.
        let state = ClipboardState::new();
        let mut incoming = IncomingClipboard::new_for_test(state.clone());

        let invalid = vec![0xFF, 0xFE, 0xFD];
        feed_offer(&mut incoming, FORMAT_TEXT_UTF8, &invalid);

        assert!(incoming.last_committed.is_none(), "no payload should commit");
        let s = state.get();
        assert!(s.text.is_none() && s.image.is_none() && s.oversize_image.is_none());
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
        let s = state.get();
        assert!(s.text.is_none() && s.image.is_none() && s.oversize_image.is_none());
    }

    /// Codex C3 deferred: marker test documenting the rich-selection gap.
    /// macOS Cmd+C on a webpage with text + image puts both on the system
    /// clipboard. Current `spawn_poll_thread` returns after sending text
    /// (continue) and never probes the image, so the image-sync feature is
    /// silently bypassed for the most common copy scenario.
    ///
    /// The fix is non-trivial: `LastKind` must be split into independent
    /// `last_text_hash` / `last_image_hash` fields, the poll loop must be
    /// rewritten to send BOTH in the same tick when both are present and
    /// changed, and dedup must consult the right field per format. Left as
    /// a follow-up — this `#[ignore]`d test makes the gap discoverable.
    #[test]
    #[ignore = "C3 deferred: rich-selection image+text not both forwarded"]
    fn c3_rich_selection_image_dropped() {
        // Marker only — driving the actual production path requires a real
        // arboard backend with both text and image present, which is not
        // testable in CI. Failing this test on `cargo test -- --include-ignored`
        // would mean the gap is closed and the comment/TODO can be removed.
        panic!("C3 not yet implemented: text+image dual-send still missing");
    }

    #[test]
    fn on_offer_unknown_format_rejected() {
        // Codex C1: `ClipOffer { format=99, total_len=u32::MAX }` would have
        // been stashed (over_cap returns false for unknown formats) and
        // chunks accepted up to memory exhaustion. Verify the early-reject
        // branch leaves state clean and chunks are dropped.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        // Unknown format with deliberately large total_len — must NOT arm.
        incoming.on_offer(0xFE, u32::MAX);

        assert_eq!(incoming.expected_len, 0, "unknown format must not arm reassembly");
        assert_eq!(incoming.expected_format, 0);
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(progress.load(Ordering::Relaxed), 0);

        // Follow-up chunks must be dropped by the expected_len==0 guard
        // (no DoS via memory pressure).
        for i in 0..16u16 {
            incoming.on_chunk(i, vec![0u8; 256]);
        }
        assert_eq!(incoming.received.len(), 0, "post-rejection chunks must not buffer");
        assert_eq!(incoming.received_total, 0);
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
        // dedup path (matches_image_hash, used inside spawn_poll_thread) must
        // treat the same RGBA as a duplicate so we don't echo it back (AC6).
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");

        let state = ClipboardState::new();
        let mut incoming = IncomingClipboard::new_for_test(state.clone());
        feed_offer(&mut incoming, FORMAT_PNG_IMAGE, &png);

        let img_hash = hash_bytes(&original.bytes);
        assert_eq!(
            state.get().image,
            Some(img_hash),
            "state must hold the image's RGBA hash for loop avoidance"
        );

        // Same code path the poll thread runs.
        assert!(
            state.get().matches_image_hash(img_hash),
            "next poll with same RGBA must short-circuit"
        );
    }

    #[test]
    fn format_oversize_toast_includes_kb_and_hint() {
        // The message rendered in the toast slot must:
        // - report the encoded size in KB (the user thinks in MB-ish, KB
        //   gives more precision near the 1 MB cap),
        // - include an actionable hint so the user knows what to do.
        let e = ImageTooLarge { png_len: 1_500 * 1024 };
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
    fn commit_clears_incoming_counters() {
        // After a successful reassembly, both incoming counters must be
        // zero so the status-line stops showing "Receiving … 100%".
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        let text = "hello";
        feed_offer(&mut incoming, FORMAT_TEXT_UTF8, text.as_bytes());

        // Ensure commit ran (text was committed).
        assert!(incoming.last_committed.is_some());
        // Counters cleared.
        assert_eq!(progress.load(Ordering::Relaxed), 0);
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn on_offer_oversize_image_rejected() {
        // total_len above MAX_IMAGE_BYTES must NOT be stored — protects
        // commit() from a 4 GB Vec::with_capacity attempt.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        incoming.on_offer(FORMAT_PNG_IMAGE, (MAX_IMAGE_BYTES as u32).saturating_add(1));

        assert_eq!(incoming.expected_len, 0, "oversize offer must not be stored");
        assert_eq!(incoming.expected_format, 0);
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn on_offer_oversize_text_rejected() {
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        incoming.on_offer(FORMAT_TEXT_UTF8, (MAX_CLIPBOARD_BYTES as u32).saturating_add(1));

        assert_eq!(incoming.expected_len, 0);
        assert_eq!(incoming.expected_format, 0);
        assert_eq!(total.load(Ordering::Relaxed), 0);
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn on_chunk_non_contiguous_indices_drops_payload() {
        // Codex C2: a malicious peer can drop chunk 3 and send chunk 7 of
        // the same size, pumping received_total to expected_len so commit()
        // fires — but the buffer has gaps with later chunks shifted left,
        // silently corrupting the payload. With the contiguity guard,
        // commit() must refuse and reset state.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        // expected_len = 512 (two 256-byte chunks worth). Indices {5, 7}
        // are non-contiguous but received_total reaches 512 → commit fires.
        incoming.on_offer(FORMAT_TEXT_UTF8, 512);
        incoming.on_chunk(5, vec![b'a'; 256]);
        incoming.on_chunk(7, vec![b'b'; 256]);

        // No commit was performed — last_committed stays None.
        assert!(
            incoming.last_committed.is_none(),
            "non-contiguous indices must not commit a corrupt payload"
        );
        // State reset so a fresh offer can proceed.
        assert_eq!(incoming.expected_len, 0);
        assert_eq!(incoming.expected_format, 0);
        assert_eq!(incoming.received_total, 0);
        assert_eq!(progress.load(Ordering::Relaxed), 0);
        assert_eq!(total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn on_chunk_replaced_with_different_size_buffer_corruption_blocked() {
        // Codex iter2 D1: even with the duplicate-index counter guard, a
        // peer can replace chunk K's stored bytes via BTreeMap::insert
        // overwrite with a *different* length. received_total counted only
        // the first arrival (200 B), expected_len = 768. If chunk(0)'s
        // payload is later swapped to 50 B and chunks 1+2 deliver 256+312,
        // received_total reaches 768 but the reassembled buffer is
        // 50+256+312 = 618 B — silent corruption inside commit().
        // Verify commit() refuses on the length mismatch and resets.
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());

        incoming.on_offer(FORMAT_TEXT_UTF8, 768);
        incoming.on_chunk(0, vec![b'a'; 200]);
        assert_eq!(incoming.received_total, 200);

        // Replace chunk 0 with shorter content (50 B). Counter stays 200.
        incoming.on_chunk(0, vec![b'x'; 50]);
        assert_eq!(incoming.received_total, 200, "duplicate must not bump counter");

        incoming.on_chunk(1, vec![b'b'; 256]);
        incoming.on_chunk(2, vec![b'c'; 312]);
        // received_total = 200 + 256 + 312 = 768 → commit fires. But the
        // BTreeMap holds 50 + 256 + 312 = 618 bytes for the buffer.
        // Length-verification guard must refuse.
        assert!(
            incoming.last_committed.is_none(),
            "length mismatch must block commit (got 618 bytes vs expected 768)"
        );
        // State reset for the next offer.
        assert_eq!(incoming.expected_len, 0);
        assert_eq!(incoming.received_total, 0);
    }

    #[test]
    fn decode_png_oversize_alloc_rejected() {
        // Codex iter6: per-axis dimension cap dropped — alloc budget is the
        // sole gate. Build a 5000×4000 RGBA PNG: 5000 × 4000 × 4 = ~76 MB,
        // exceeds DECODE_MAX_ALLOC (64 MB) and must be rejected.
        let w: u32 = 5000;
        let h: u32 = 4000;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            rgba.extend_from_slice(&[0, 0, 0, 0xFF]);
        }
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
                .expect("encode oversize png");
        }
        let result = decode_png_to_rgba(&png);
        assert!(
            result.is_err(),
            "decode must reject {}×{} PNG ({} bytes RGBA exceeds 64 MB budget)",
            w,
            h,
            (w as u64) * (h as u64) * 4,
        );
    }

    #[test]
    fn decode_png_palette_bomb_rejected() {
        // Codex iter3 E1 + iter6: a palette PNG at 8000×8000 compresses to a
        // tiny file but expands to 8000×8000×4 = 256 MB of RGBA — well over
        // DECODE_MAX_ALLOC (64 MB). The decoder's alloc-limit catches this
        // (or the explicit post-decode check, belt-and-suspenders).
        let w: u32 = 8000;
        let h: u32 = 8000;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            rgba.extend_from_slice(&[0, 0, 0, 0xFF]);
        }
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
                .expect("encode palette-bomb png");
        }
        let result = decode_png_to_rgba(&png);
        assert!(
            result.is_err(),
            "decode must reject {}×{} PNG (would allocate ~256 MB RGBA)",
            w,
            h,
        );
    }

    #[test]
    fn decode_png_5k_screenshot_succeeds() {
        // Codex iter6: 5K Retina screenshot (5120×2880) is 5120×2880×4 ≈
        // 58.6 MB — inside the 64 MB budget. Must decode successfully.
        // Regression test for the old per-axis cap which rejected this.
        let w: u32 = 5120;
        let h: u32 = 2880;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..(w * h) {
            rgba.extend_from_slice(&[0, 0, 0, 0xFF]);
        }
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
                .expect("encode 5k png");
        }
        let result = decode_png_to_rgba(&png);
        assert!(
            result.is_ok(),
            "decode must accept {}×{} PNG (~{} MB RGBA, inside 64 MB budget)",
            w,
            h,
            (w as u64) * (h as u64) * 4 / (1024 * 1024),
        );
        let img = result.unwrap();
        assert_eq!(img.width, w as usize);
        assert_eq!(img.height, h as usize);
    }

    #[test]
    fn on_chunk_duplicate_index_does_not_overcount() {
        // Feed offer + two chunks with the same index. received_total must
        // count only the first chunk — duplicates silently overwrite via
        // BTreeMap::insert, but received_total must not race ahead of the
        // actual buffer size, otherwise commit() fires with a truncated
        // payload (silent corruption).
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        incoming.on_offer(FORMAT_TEXT_UTF8, 1024);
        incoming.on_chunk(0, vec![b'a'; 256]);
        assert_eq!(incoming.received_total, 256);
        assert_eq!(progress.load(Ordering::Relaxed), 256);

        // Same index again — must NOT increment received_total again.
        incoming.on_chunk(0, vec![b'b'; 256]);
        assert_eq!(
            incoming.received_total, 256,
            "duplicate index must not bump received_total"
        );
        assert_eq!(progress.load(Ordering::Relaxed), 256);
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

    #[test]
    fn on_chunk_without_offer_drops_data() {
        // Chunks arriving before any ClipOffer must NOT be buffered —
        // otherwise BTreeMap::insert grows unbounded and a misbehaving peer
        // can DoS us via memory pressure (M2/C1 finding).
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        incoming.on_chunk(0, vec![0u8; 256]);
        incoming.on_chunk(1, vec![0u8; 256]);
        incoming.on_chunk(2, vec![0u8; 256]);

        assert_eq!(incoming.received.len(), 0, "chunks without offer must not buffer");
        assert_eq!(incoming.received_total, 0);
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn on_chunk_after_oversize_offer_drops_data() {
        // After on_offer rejects an oversized payload (expected_len stays 0),
        // subsequent chunks for that aborted offer must be discarded — not
        // accumulated in BTreeMap (M2/C1 memory leak).
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let mut incoming = IncomingClipboard::new_for_test(ClipboardState::new());
        incoming.incoming_progress = progress.clone();
        incoming.incoming_total = total.clone();

        // Reject oversize.
        incoming.on_offer(FORMAT_PNG_IMAGE, (MAX_IMAGE_BYTES as u32).saturating_add(1));
        assert_eq!(incoming.expected_len, 0);

        // Peer keeps blasting chunks — they must all be dropped.
        for i in 0..16u16 {
            incoming.on_chunk(i, vec![0u8; 256]);
        }

        assert_eq!(incoming.received.len(), 0, "post-rejection chunks must not buffer");
        assert_eq!(incoming.received_total, 0);
        assert_eq!(progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn oversize_dedup_skips_repoll() {
        // Mirror the poll-thread short-circuit logic: once an RGBA hash is
        // stamped as LastKind::OversizeImage, the next tick with the same
        // RGBA must hit the dedup branch BEFORE re-encoding (so no toast,
        // no CPU). Drives the production `LastKind::matches_image_hash`
        // method used inside spawn_poll_thread.
        let state = ClipboardState::new();
        let img = synthetic_rgba_4x4();

        // First tick: no prior state, would proceed to encode → oversize.
        let hash = hash_bytes(&img.bytes);
        assert!(!state.get().matches_image_hash(hash), "first tick must NOT skip");

        // Stamp oversize as if poll thread just ran encode + check_image_size.
        state.set(LastKind::OversizeImage(hash));

        // Second tick: same RGBA → dedup branch must fire, skipping encode.
        assert!(
            state.get().matches_image_hash(hash),
            "repeated oversize must short-circuit"
        );

        // Different RGBA (user re-copied something else) → must NOT skip.
        let mut other = img.bytes.to_vec();
        other[0] ^= 0xFF;
        let other_hash = hash_bytes(&other);
        assert!(
            !state.get().matches_image_hash(other_hash),
            "different RGBA must re-try encode path"
        );
    }
}
