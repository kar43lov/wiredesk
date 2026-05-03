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

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use wiredesk_protocol::message::{FORMAT_PNG_IMAGE, FORMAT_TEXT_UTF8, Message};

const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const CHUNK_SIZE: usize = 256;
const MAX_CLIPBOARD_BYTES: usize = 256 * 1024; // text cap
/// Codex iter2 D3: Session::tick() blocks on `transport.send` for every
/// message returned by `poll()` before reaching `transport.recv()`. A 1 MB
/// image transfer = ~4097 messages × ~22 ms each at 115200 baud = ~90 seconds
/// during which heartbeats and input cannot be received → connection dies on
/// the 6 s heartbeat timeout. Cap how many messages `poll()` returns per call
/// so the tick loop interleaves wire-sends with `recv()`. Remaining chunks
/// sit in `pending_outbox` and drain on subsequent ticks.
const MAX_MESSAGES_PER_POLL: usize = 8;
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
    /// RGBA hash of the most recent image rejected by the size cap. Lets the
    /// poll path short-circuit the expensive RGBA→PNG re-encode for the same
    /// buffer on every poll tick.
    OversizeImage(u64),
}

impl LastKind {
    /// True when this state has stamped the given RGBA hash either as a
    /// successfully-sent/received image (loop-avoidance dedup) or as an
    /// oversize-rejected image (CPU-saving short-circuit). Symmetric with
    /// the Mac side — duplication is intentional per CLAUDE.md.
    fn matches_image_hash(&self, hash: u64) -> bool {
        matches!(self, LastKind::Image(h) | LastKind::OversizeImage(h) if *h == hash)
    }
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

/// 64 MB upper bound on decoder allocations. Single source of truth for
/// "safe to decode": any PNG whose decoded RGBA buffer would exceed this
/// budget is rejected regardless of per-axis dimensions.
///
/// Codex iter6: a per-axis dimension cap (previously 4096) was strictly more
/// restrictive than the alloc budget — it rejected legitimate widescreen /
/// high-resolution screenshots (5K Retina ≈ 58.6 MB, inside the budget).
/// Dropped per-axis cap; rely on alloc budget + explicit post-decode check
/// for `to_rgba8()` (allocates independently of decoder `Limits`).
const DECODE_MAX_ALLOC: u64 = 64 * 1024 * 1024;

/// Decode PNG bytes to an arboard `ImageData` (RGBA8, owned).
///
/// Codex iter2 D2 + iter3 E1 + iter6: caps decoded allocations so a PNG bomb
/// (palette image expanding to hundreds of MB of RGBA) cannot blow up memory.
/// `image::Limits.max_alloc` covers the decoder's internal buffers; the
/// explicit post-decode `(w * h * 4) > DECODE_MAX_ALLOC` check covers the
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

/// Pure size-check helper — used by production poll (`MAX_IMAGE_BYTES`) and
/// by unit tests with a low limit so synthetic 4×4 fixtures hit the path.
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

/// Build a `ClipOffer` + N `ClipChunk` messages for one payload. Pure helper.
fn build_offer_and_chunks(format: u8, payload: &[u8]) -> Vec<Message> {
    let mut msgs = Vec::with_capacity(1 + payload.len().div_ceil(CHUNK_SIZE));
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

/// Hash the current clipboard content so the first poll-tick after startup
/// short-circuits via the `LastKind` dedup. Without this, a host restart
/// re-uploads whatever the user already had on the Win clipboard.
fn stamp_initial(clip: Option<&mut arboard::Clipboard>) -> LastKind {
    let Some(clip) = clip else {
        return LastKind::None;
    };
    if let Ok(text) = clip.get_text() {
        if !text.is_empty() {
            log::info!(
                "clipboard: pre-stamped existing text ({} bytes) — not sending on startup",
                text.len()
            );
            return LastKind::Text(hash_text(&text));
        }
    }
    if let Ok(img) = clip.get_image() {
        log::info!(
            "clipboard: pre-stamped existing image ({}x{}) — not sending on startup",
            img.width,
            img.height
        );
        return LastKind::Image(hash_bytes(&img.bytes));
    }
    LastKind::None
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

    // Progress counters — atomics so the always-on-top overlay (separate
    // UI thread) can poll without locking. Host stays single-threaded for
    // mutation: only the session-thread bumps these.
    //   - incoming_*: written by `on_offer` / `on_chunk` / `commit` /
    //     `reset_reassembly`.
    //   - outgoing_*: written by `poll()` (sets total when transfer starts,
    //     bumps progress per chunk drained from the outbox; cleared once
    //     progress reaches total or `reset()` runs).
    incoming_progress: Arc<AtomicU64>,
    incoming_total: Arc<AtomicU64>,
    outgoing_progress: Arc<AtomicU64>,
    outgoing_total: Arc<AtomicU64>,

    /// Codex iter2 D3: pending outbound messages from a started transfer.
    /// `poll()` builds the full offer+chunks list once, pushes everything
    /// here, and returns at most `MAX_MESSAGES_PER_POLL`. Subsequent ticks
    /// drain the rest. This keeps Session::tick interleaving sends with
    /// recv() so heartbeats and input keep flowing during image transfers.
    pending_outbox: VecDeque<Message>,

    /// Transient warning the UI should surface (e.g., "image too large to
    /// send"). Set inside `poll()`; consumed by the session thread once per
    /// tick via [`take_warning`]. Single-slot — repeated warnings within a
    /// tick overwrite the previous (rare; oversize-skip is the only writer).
    pending_warning: Option<String>,

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

/// Bundle of progress atomics shared between `ClipboardSync` and any
/// external observer (the always-on-top transfer overlay). All four counters
/// are owned by the session thread; the overlay thread only reads them.
#[derive(Clone, Default)]
pub struct ProgressCounters {
    pub outgoing_progress: Arc<AtomicU64>,
    pub outgoing_total: Arc<AtomicU64>,
    pub incoming_progress: Arc<AtomicU64>,
    pub incoming_total: Arc<AtomicU64>,
}

impl ClipboardSync {
    pub fn with_counters(counters: ProgressCounters) -> Self {
        let mut clip = arboard::Clipboard::new().ok();
        // Pre-stamp existing clipboard content so a fresh host process
        // doesn't try to push whatever the user happened to leave on the
        // Win clipboard from a previous session (or from a foreign app).
        // Only the user's NEXT explicit Cmd+C produces a different hash
        // and triggers a real outbound sync.
        let initial_last = stamp_initial(clip.as_mut());
        Self {
            clip,
            last: initial_last,
            last_poll: Instant::now(),
            expected_len: 0,
            expected_format: 0,
            received_total: 0,
            received: BTreeMap::new(),
            incoming_progress: counters.incoming_progress,
            incoming_total: counters.incoming_total,
            outgoing_progress: counters.outgoing_progress,
            outgoing_total: counters.outgoing_total,
            pending_outbox: VecDeque::new(),
            pending_warning: None,
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
            incoming_progress: Arc::new(AtomicU64::new(0)),
            incoming_total: Arc::new(AtomicU64::new(0)),
            outgoing_progress: Arc::new(AtomicU64::new(0)),
            outgoing_total: Arc::new(AtomicU64::new(0)),
            pending_outbox: VecDeque::new(),
            pending_warning: None,
            last_committed: None,
        }
    }

    /// Drain the most recent warning, if any. Called by the session thread
    /// after every tick — a `Some(msg)` is forwarded to the tray UI as a
    /// balloon notification ("image too large", etc.).
    pub fn take_warning(&mut self) -> Option<String> {
        self.pending_warning.take()
    }

    /// `true` while either side of clipboard sync is mid-transfer:
    /// outgoing chunks queued in `pending_outbox`, or incoming reassembly
    /// armed by a `ClipOffer` and not yet committed/aborted. Used by
    /// `Session::tick` to extend the heartbeat timeout while the wire is
    /// saturated by a large image — at 11 KB/s an 80 KB image takes ~7 s,
    /// during which the peer's heartbeats can be drowned out by chunk
    /// traffic and the strict 6 s timeout would falsely disconnect.
    pub fn transfer_in_flight(&self) -> bool {
        self.expected_len > 0 || !self.pending_outbox.is_empty()
    }

    /// Drop everything currently queued in `pending_outbox`. Called when
    /// the peer signals `ClipDecline` — they don't want this transfer,
    /// no point spending wire bandwidth on chunks they'll discard.
    /// Also resets the outgoing-progress counters so the UI doesn't
    /// stick at a stale percentage. Returns the number of packets
    /// dropped for logging purposes.
    pub fn cancel_outgoing(&mut self) -> usize {
        let n = self.pending_outbox.len();
        self.pending_outbox.clear();
        self.outgoing_progress.store(0, Ordering::Relaxed);
        self.outgoing_total.store(0, Ordering::Relaxed);
        n
    }

    /// Drain up to `MAX_MESSAGES_PER_POLL` messages from `pending_outbox`
    /// into a fresh Vec. Returned to the caller in arrival order. Any
    /// `ClipChunk` removed from the queue bumps `outgoing_progress` so the
    /// overlay sees per-chunk progress (matches Mac's writer-thread bump).
    /// The counters are NOT cleared on completion — the overlay latches the
    /// 100% state for ~1 s before fading out, and then the next transfer's
    /// `outgoing_total` write replaces the value naturally.
    fn drain_outbox(&mut self) -> Vec<Message> {
        let n = self.pending_outbox.len().min(MAX_MESSAGES_PER_POLL);
        let drained: Vec<Message> = self.pending_outbox.drain(..n).collect();
        for m in &drained {
            if let Message::ClipChunk { data, .. } = m {
                self.outgoing_progress
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
            }
        }
        drained
    }

    /// Called from session.tick(). Returns up to `MAX_MESSAGES_PER_POLL`
    /// messages to send. If a transfer is in flight (pending_outbox not
    /// empty) drain that batch first — DO NOT probe the local clipboard
    /// again, otherwise a 4097-message image transfer would be
    /// interleaved with new offers spawning more chunks. Only after the
    /// outbox empties does the next 500 ms poll-interval gate engage.
    pub fn poll(&mut self) -> Vec<Message> {
        if !self.pending_outbox.is_empty() {
            return self.drain_outbox();
        }
        if self.last_poll.elapsed() < CLIP_POLL_INTERVAL {
            return Vec::new();
        }
        self.last_poll = Instant::now();

        let Some(clip) = self.clip.as_mut() else {
            return Vec::new();
        };

        // 1) Text path.
        //
        // TODO: probe image even when text exists (codex C3). Rich
        // selections (web page with text + image) put BOTH on the
        // clipboard; we currently only forward the text. Two-phase
        // probe in the same tick would need LastKind split into
        // independent text and image hashes — deferred. See ignored
        // test `host_c3_rich_selection_image_dropped`.
        match clip.get_text() {
            Ok(text) if !text.is_empty() => {
                let hash = hash_text(&text);
                if matches!(self.last, LastKind::Text(h) if h == hash) {
                    return Vec::new();
                }
                // Codex iter3 E2 (acceptable): sender dedup is set on enqueue,
                // not on successful send. If transport fails mid-transfer,
                // retry happens only when clipboard content changes again.
                // Acceptable: heartbeat covers disconnect within 6s, app
                // restart clears state.
                self.last = LastKind::Text(hash);

                let bytes = text.as_bytes();
                if bytes.len() > MAX_CLIPBOARD_BYTES {
                    log::warn!(
                        "clipboard: skipping push — {} bytes exceeds limit",
                        bytes.len()
                    );
                    return Vec::new();
                }

                log::info!("clipboard: sending text to peer ({} bytes)", bytes.len());
                // Stamp totals BEFORE drain so the overlay's first read sees
                // a non-zero total and renders the in-flight string. Reset
                // progress to 0 — a previous transfer may have left it at
                // its terminal value (we keep it there so the 100% latch
                // works on the receiver side).
                self.outgoing_total.store(bytes.len() as u64, Ordering::Relaxed);
                self.outgoing_progress.store(0, Ordering::Relaxed);
                self.pending_outbox
                    .extend(build_offer_and_chunks(FORMAT_TEXT_UTF8, bytes));
                return self.drain_outbox();
            }
            _ => {} // fall through to image probe
        }

        // 2) Image path.
        let img = match clip.get_image() {
            Ok(i) => i,
            Err(_) => return Vec::new(),
        };

        let hash = hash_bytes(&img.bytes);
        // Short-circuit BEFORE the expensive RGBA→PNG encode for both:
        // - already-sent images (LastKind::Image),
        // - already-rejected oversized images (LastKind::OversizeImage).
        // Otherwise every poll tick re-encodes (~30-150 ms CPU) and re-logs
        // the warning for the SAME oversize buffer.
        if self.last.matches_image_hash(hash) {
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
                MAX_IMAGE_BYTES
            );
            // Surface a transient UI notification — the host has no chrome
            // panel of its own, so without this the user only sees a log
            // entry and an unexplained "no transfer started" silence. The
            // session thread takes the warning out via `take_warning` and
            // forwards it as a balloon notification.
            let kb = e.png_len / 1024;
            let limit_kb = MAX_IMAGE_BYTES / 1024;
            self.pending_warning = Some(format!(
                "Clipboard image too large to send: {} KB > {} KB limit. \
                 Copy a smaller selection.",
                kb, limit_kb
            ));
            // Stamp the RGBA hash so the next poll tick short-circuits for
            // the same buffer. A new RGBA (user re-copied) gives a new hash
            // and re-tries the encode path.
            self.last = LastKind::OversizeImage(hash);
            return Vec::new();
        }

        // Codex iter3 E2 (acceptable): sender dedup is set on enqueue, not on
        // successful send. If transport fails mid-transfer, retry happens only
        // when clipboard content changes again. Acceptable: heartbeat covers
        // disconnect within 6s, app restart clears state.
        self.last = LastKind::Image(hash);
        log::info!(
            "clipboard: sending image to peer ({} bytes)",
            png.len()
        );
        self.outgoing_total.store(png.len() as u64, Ordering::Relaxed);
        self.outgoing_progress.store(0, Ordering::Relaxed);
        self.pending_outbox
            .extend(build_offer_and_chunks(FORMAT_PNG_IMAGE, &png));
        self.drain_outbox()
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
            // Milestone logging — every 25% of expected_len.
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

    /// Test-only accessor — lets session.rs unit tests verify that the
    /// reset() call on disconnect / heartbeat-timeout / re-handshake actually
    /// drops in-flight reassembly state.
    #[cfg(test)]
    pub(crate) fn expected_len(&self) -> u32 {
        self.expected_len
    }

    /// Drop any in-flight reassembly state and zero progress counters.
    /// Called from the session loop on disconnect / new Hello so a partial
    /// transfer doesn't leak across sessions. Also drops any queued
    /// outbound messages — a 1 MB transfer that started before disconnect
    /// must NOT keep streaming after a reconnect (peer's last_kind already
    /// stamped, would just dedup, but the wire-time is wasted).
    ///
    /// Also clears the sender-side `last` dedup hash (Codex iter4 F2).
    /// Without this, if a transfer aborts mid-stream and the peer reconnects
    /// (or the session re-handshakes), the next poll-tick would see the same
    /// OS-clipboard content, match `LastKind`, and dedup → silent lost-update.
    /// Trade-off: after a brief disconnect both sides may resend their
    /// current clipboard contents (each thinks the other doesn't have it) —
    /// that's correct sync behaviour, better than a silent lost update.
    pub fn reset(&mut self) {
        self.reset_reassembly();
        self.pending_outbox.clear();
        self.last = LastKind::None;
        // Drop sender-side overlay totals too: a session boundary should
        // wipe the "Sending X" string, not leave a stale 100% banner.
        self.outgoing_progress.store(0, Ordering::Relaxed);
        self.outgoing_total.store(0, Ordering::Relaxed);
    }

    /// Cleanup that zeros reassembly state but preserves `self.last` (sender
    /// dedup) and the outbound queue (unrelated to incoming reassembly).
    ///
    /// Used by:
    ///   - successful-commit fall-through (preserves the freshly-stamped
    ///     `self.last` set by `commit_text` / `commit_image`),
    ///   - corruption branches in `commit()` — non-contiguous indices,
    ///     length mismatch (Phase 4 M1: full `reset()` would silently drop a
    ///     mid-flight outgoing transfer's `pending_outbox` and clear sender
    ///     dedup, both unrelated to a receive-side failure).
    ///
    /// Full `reset()` is reserved for session-boundary callers (Disconnect,
    /// heartbeat-timeout, re-Hello) where the entire link state is torn down.
    fn reset_reassembly(&mut self) {
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
            // Phase 4 M1: only clear reassembly state. Full reset() would also
            // drain pending_outbox (mid-flight outgoing transfer would be
            // silently dropped → peer waits forever) and clear sender-side
            // `last` (causing a redundant resend on the next poll tick).
            // A receive-side corruption is unrelated to our send queue.
            self.reset_reassembly();
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
            // Phase 4 M1: only clear reassembly state — see non-contiguous
            // branch above for rationale.
            self.reset_reassembly();
            return;
        }

        match self.expected_format {
            FORMAT_TEXT_UTF8 => self.commit_text(buf),
            FORMAT_PNG_IMAGE => self.commit_image(&buf),
            other => {
                log::warn!("clipboard: unknown format {other}, skipping {} bytes", buf.len());
            }
        }

        // End-of-commit cleanup: zero reassembly counters but keep the
        // freshly-stamped `self.last` (commit_text/image just set it) and
        // any queued outbound work. `received` is already drained via
        // mem::take above, so the BTreeMap clear is a no-op.
        self.reset_reassembly();
    }

    fn commit_text(&mut self, buf: Vec<u8>) {
        match String::from_utf8(buf) {
            Ok(text) => {
                let hash = hash_text(&text);
                #[cfg(test)]
                {
                    self.last_committed = Some(CommittedPayload::Text(text.clone()));
                }
                // Codex iter3 E3: write OS clipboard FIRST, mark hash on
                // success. If set_text fails, leaving `last` unchanged lets
                // the next poll detect the (still-stale) OS clipboard and
                // re-send instead of suppressing forever.
                let mut wrote_ok = self.clip.is_none(); // no backend (tests) → ok
                if let Some(clip) = self.clip.as_mut() {
                    match clip.set_text(text.clone()) {
                        Ok(()) => {
                            log::debug!("clipboard: wrote {} bytes from client", text.len());
                            wrote_ok = true;
                        }
                        Err(e) => log::warn!("clipboard: set_text failed: {e}"),
                    }
                }
                if wrote_ok {
                    self.last = LastKind::Text(hash);
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

        // Codex iter3 E3: write OS clipboard FIRST, mark hash on success.
        let mut wrote_ok = self.clip.is_none();
        if let Some(clip) = self.clip.as_mut() {
            match clip.set_image(img) {
                Ok(()) => {
                    log::debug!("clipboard: wrote image from client ({} encoded bytes)", buf.len());
                    wrote_ok = true;
                }
                Err(e) => log::warn!("clipboard: set_image failed: {e}"),
            }
        }
        if wrote_ok {
            self.last = LastKind::Image(hash);
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
    fn host_incoming_invalid_text_skipped() {
        // format=0 with non-UTF-8 bytes must NOT panic, NOT commit, and
        // leave LastKind at None so a subsequent valid push can proceed.
        let mut sync = ClipboardSync::new_for_test();
        let invalid = vec![0xFF, 0xFE, 0xFD];
        feed_offer(&mut sync, FORMAT_TEXT_UTF8, &invalid);

        assert!(sync.last_committed.is_none());
        assert!(matches!(sync.last, LastKind::None));
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

    /// Codex C3 deferred: see Mac-side `c3_rich_selection_image_dropped`
    /// for context. Symmetric gap on Host: Windows Ctrl+C on rich content
    /// puts text and CF_DIB simultaneously, and the poll only forwards text.
    #[test]
    #[ignore = "C3 deferred: rich-selection image+text not both forwarded"]
    fn host_c3_rich_selection_image_dropped() {
        panic!("C3 not yet implemented: text+image dual-send still missing");
    }

    #[test]
    fn host_on_offer_unknown_format_rejected() {
        // Codex C1: `ClipOffer { format=99, total_len=u32::MAX }` would have
        // been stashed (over_cap returns false for unknown formats) and
        // chunks accepted up to memory exhaustion. Verify the early-reject
        // branch leaves state clean and chunks are dropped.
        let mut sync = ClipboardSync::new_for_test();

        // Unknown format with deliberately large total_len — must NOT arm.
        sync.on_offer(0xFE, u32::MAX);

        assert_eq!(sync.expected_len, 0, "unknown format must not arm reassembly");
        assert_eq!(sync.expected_format, 0);
        assert_eq!(sync.incoming_total.load(Ordering::Relaxed), 0);
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);

        // Follow-up chunks must be dropped by the expected_len==0 guard.
        for i in 0..16u16 {
            sync.on_chunk(i, vec![0u8; 256]);
        }
        assert_eq!(sync.received.len(), 0, "post-rejection chunks must not buffer");
        assert_eq!(sync.received_total, 0);
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
    fn host_commit_clears_incoming_counters() {
        // After a successful reassembly, both incoming counters must be zero.
        let mut sync = ClipboardSync::new_for_test();
        let text = "hello";
        feed_offer(&mut sync, FORMAT_TEXT_UTF8, text.as_bytes());

        assert!(sync.last_committed.is_some());
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);
        assert_eq!(sync.incoming_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn host_on_offer_oversize_image_rejected() {
        // total_len above MAX_IMAGE_BYTES must NOT be stored — protects
        // commit() from a 4 GB Vec::with_capacity attempt.
        let mut sync = ClipboardSync::new_for_test();
        sync.on_offer(FORMAT_PNG_IMAGE, (MAX_IMAGE_BYTES as u32).saturating_add(1));

        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.expected_format, 0);
        assert_eq!(sync.incoming_total.load(Ordering::Relaxed), 0);
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn host_on_offer_oversize_text_rejected() {
        let mut sync = ClipboardSync::new_for_test();
        sync.on_offer(FORMAT_TEXT_UTF8, (MAX_CLIPBOARD_BYTES as u32).saturating_add(1));

        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.expected_format, 0);
        assert_eq!(sync.incoming_total.load(Ordering::Relaxed), 0);
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn host_on_chunk_non_contiguous_indices_drops_payload() {
        // Codex C2: indices {5, 7} pump received_total to expected_len but
        // leave gaps. commit() must refuse (contiguity guard) and reset.
        let mut sync = ClipboardSync::new_for_test();
        sync.on_offer(FORMAT_TEXT_UTF8, 512);
        sync.on_chunk(5, vec![b'a'; 256]);
        sync.on_chunk(7, vec![b'b'; 256]);

        assert!(sync.last_committed.is_none(), "non-contiguous must not commit");
        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.expected_format, 0);
        assert_eq!(sync.received_total, 0);
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);
        assert_eq!(sync.incoming_total.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn host_commit_failure_preserves_outbox_and_sender_dedup() {
        // Phase 4 M1: a receive-side commit failure (corrupt offer from
        // peer) must NOT drain `pending_outbox` (mid-flight outgoing
        // transfer would be silently dropped → peer waits forever) and
        // must NOT clear `self.last` (would force a redundant resend on
        // the next poll tick). Only reassembly state should be reset.
        //
        // Two failure paths exercised:
        //   a) non-contiguous indices (commit() ~line 501 branch),
        //   b) length mismatch (commit() ~line 521 branch).

        // --- (a) non-contiguous indices ---
        let mut sync = ClipboardSync::new_for_test();
        let outgoing = vec![0xAA; 768];
        for m in build_offer_and_chunks(FORMAT_PNG_IMAGE, &outgoing) {
            sync.pending_outbox.push_back(m);
        }
        let outbox_len_before = sync.pending_outbox.len();
        assert_eq!(outbox_len_before, 4, "fixture: 1 offer + 3 chunks");
        sync.last = LastKind::Image(0xDEAD);

        sync.on_offer(FORMAT_TEXT_UTF8, 512);
        sync.on_chunk(5, vec![b'a'; 256]);
        sync.on_chunk(7, vec![b'b'; 256]);

        assert!(
            sync.last_committed.is_none(),
            "non-contiguous must not commit"
        );
        assert_eq!(
            sync.last,
            LastKind::Image(0xDEAD),
            "sender dedup must be preserved across receive-side failure"
        );
        assert_eq!(
            sync.pending_outbox.len(),
            outbox_len_before,
            "outbound queue must be preserved across receive-side failure"
        );
        // Reassembly state IS cleared.
        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.expected_format, 0);
        assert_eq!(sync.received_total, 0);
        assert!(sync.received.is_empty());
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);
        assert_eq!(sync.incoming_total.load(Ordering::Relaxed), 0);

        // --- (b) length mismatch (replace chunk 0 with shorter buf) ---
        let mut sync = ClipboardSync::new_for_test();
        for m in build_offer_and_chunks(FORMAT_PNG_IMAGE, &outgoing) {
            sync.pending_outbox.push_back(m);
        }
        sync.last = LastKind::Image(0xDEAD);

        sync.on_offer(FORMAT_TEXT_UTF8, 768);
        sync.on_chunk(0, vec![b'a'; 200]);
        sync.on_chunk(0, vec![b'x'; 50]); // overwrite, counter unchanged
        sync.on_chunk(1, vec![b'b'; 256]);
        sync.on_chunk(2, vec![b'c'; 312]); // total reaches 768 → commit fires

        assert!(
            sync.last_committed.is_none(),
            "length mismatch must not commit"
        );
        assert_eq!(
            sync.last,
            LastKind::Image(0xDEAD),
            "sender dedup must be preserved across length-mismatch failure"
        );
        assert_eq!(
            sync.pending_outbox.len(),
            outbox_len_before,
            "outbound queue must be preserved across length-mismatch failure"
        );
        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.received_total, 0);
    }

    #[test]
    fn host_on_chunk_replaced_with_different_size_buffer_corruption_blocked() {
        // Codex iter2 D1: duplicate-index counter guard prevents
        // received_total overshoot, but BTreeMap::insert silently swaps
        // the stored bytes. If the swap is to a *different* length,
        // reassembled buf.len() != expected_len → commit() must refuse.
        let mut sync = ClipboardSync::new_for_test();

        sync.on_offer(FORMAT_TEXT_UTF8, 768);
        sync.on_chunk(0, vec![b'a'; 200]);
        assert_eq!(sync.received_total, 200);

        // Replace chunk 0 with 50 bytes — counter stays 200.
        sync.on_chunk(0, vec![b'x'; 50]);
        assert_eq!(sync.received_total, 200);

        sync.on_chunk(1, vec![b'b'; 256]);
        sync.on_chunk(2, vec![b'c'; 312]);
        // received_total = 768, but stored buf = 50+256+312 = 618.
        assert!(
            sync.last_committed.is_none(),
            "length mismatch must block commit"
        );
        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.received_total, 0);
    }

    #[test]
    fn host_decode_png_oversize_alloc_rejected() {
        // Codex iter6: per-axis dimension cap dropped — alloc budget is the
        // sole gate. 5000×4000 RGBA = ~76 MB > DECODE_MAX_ALLOC (64 MB).
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
    fn host_decode_png_palette_bomb_rejected() {
        // Codex iter3 E1 + iter6: explicit guard against a palette PNG that
        // would compress small but expand to ~256 MB of RGBA via to_rgba8().
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
    fn host_decode_png_5k_screenshot_succeeds() {
        // Codex iter6: 5K Retina screenshot (5120×2880) ≈ 58.6 MB RGBA —
        // inside the 64 MB budget. Regression test for the old per-axis cap.
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
    fn host_poll_drains_chunks_in_batches_not_all_at_once() {
        // Codex iter2 D3: poll() must return at most MAX_MESSAGES_PER_POLL
        // messages per call. A payload spanning many chunks should be
        // dripped out across multiple poll() calls so Session::tick()
        // interleaves wire-sends with recv() (heartbeat liveness).
        //
        // Drive the build_offer_and_chunks → outbox → drain path directly
        // via on_chunk-style fixture: push a synthetic large payload by
        // calling build_offer_and_chunks and seeding pending_outbox, then
        // exercise drain semantics across consecutive poll() calls. This
        // avoids needing a live arboard backend.
        let mut sync = ClipboardSync::new_for_test();

        // 2048 bytes → 1 ClipOffer + 8 chunks = 9 messages total.
        let payload = vec![0xABu8; 2048];
        let built = build_offer_and_chunks(FORMAT_TEXT_UTF8, &payload);
        assert_eq!(built.len(), 9, "fixture: 2048 / 256 = 8 chunks + 1 offer");
        for m in built {
            sync.pending_outbox.push_back(m);
        }
        assert_eq!(sync.pending_outbox.len(), 9);

        // First poll() call: returns up to MAX_MESSAGES_PER_POLL (= 8).
        let first = sync.poll();
        assert_eq!(
            first.len(),
            MAX_MESSAGES_PER_POLL,
            "first poll must return exactly MAX_MESSAGES_PER_POLL messages"
        );
        // 9 - 8 = 1 message remains in outbox.
        assert_eq!(sync.pending_outbox.len(), 1);

        // Second poll() call: drains the remainder (the trailing chunk).
        let second = sync.poll();
        assert_eq!(second.len(), 1, "second poll must drain remainder");
        assert!(sync.pending_outbox.is_empty());

        // Third poll() call: outbox empty → falls through to clipboard
        // probe path; with `clip = None` (test ctor) returns empty.
        let third = sync.poll();
        assert!(third.is_empty(), "no work pending → empty Vec");
    }

    #[test]
    fn host_reset_clears_outbox() {
        // Codex iter2 D3: reset() must drop queued outbound messages so
        // an in-flight transfer does not keep streaming after a session
        // reset (heartbeat-timeout / re-handshake).
        let mut sync = ClipboardSync::new_for_test();
        let payload = vec![0xAB; 2048];
        for m in build_offer_and_chunks(FORMAT_TEXT_UTF8, &payload) {
            sync.pending_outbox.push_back(m);
        }
        assert!(!sync.pending_outbox.is_empty());

        sync.reset();
        assert!(sync.pending_outbox.is_empty(), "reset must drain outbox");
    }

    #[test]
    fn host_on_chunk_duplicate_index_does_not_overcount() {
        // Duplicate index → received_total counts the first chunk only.
        // Otherwise a second-arrival overwrite would silently truncate the
        // buffer once received_total >= expected_len fires commit().
        let mut sync = ClipboardSync::new_for_test();
        sync.on_offer(FORMAT_TEXT_UTF8, 1024);
        sync.on_chunk(0, vec![b'a'; 256]);
        assert_eq!(sync.received_total, 256);
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 256);

        sync.on_chunk(0, vec![b'b'; 256]);
        assert_eq!(sync.received_total, 256, "duplicate index must not bump total");
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 256);
    }

    #[test]
    fn host_reset_clears_state() {
        let mut sync = ClipboardSync::new_for_test();

        sync.on_offer(FORMAT_PNG_IMAGE, 4096);
        sync.on_chunk(0, vec![0u8; 256]);
        sync.on_chunk(1, vec![0u8; 256]);
        // Stamp sender dedup so reset can prove it clears that too. Without
        // this, a mid-transfer abort leaves `last` stamped → after reconnect
        // the same OS-clipboard content would match the dedup and skip the
        // resend (silent lost-update).
        sync.last = LastKind::Text(0xDEAD_BEEF);
        assert!(sync.received_total > 0);
        assert!(sync.incoming_progress.load(Ordering::Relaxed) > 0);
        assert!(sync.incoming_total.load(Ordering::Relaxed) > 0);

        sync.reset();

        assert_eq!(sync.expected_len, 0);
        assert_eq!(sync.expected_format, 0);
        assert_eq!(sync.received_total, 0);
        assert!(sync.received.is_empty());
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);
        assert_eq!(sync.incoming_total.load(Ordering::Relaxed), 0);
        assert!(matches!(sync.last, LastKind::None), "reset must clear sender dedup");
    }

    #[test]
    fn host_on_chunk_without_offer_drops_data() {
        // Chunks arriving before any ClipOffer must NOT be buffered —
        // otherwise BTreeMap::insert grows unbounded and a misbehaving peer
        // can DoS us via memory pressure (M2/C1 finding).
        let mut sync = ClipboardSync::new_for_test();

        sync.on_chunk(0, vec![0u8; 256]);
        sync.on_chunk(1, vec![0u8; 256]);
        sync.on_chunk(2, vec![0u8; 256]);

        assert_eq!(sync.received.len(), 0, "chunks without offer must not buffer");
        assert_eq!(sync.received_total, 0);
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn host_on_chunk_after_oversize_offer_drops_data() {
        // After on_offer rejects an oversized payload, follow-up chunks must
        // be dropped — not accumulated in BTreeMap (M2/C1 memory leak).
        let mut sync = ClipboardSync::new_for_test();

        sync.on_offer(FORMAT_PNG_IMAGE, (MAX_IMAGE_BYTES as u32).saturating_add(1));
        assert_eq!(sync.expected_len, 0);

        for i in 0..16u16 {
            sync.on_chunk(i, vec![0u8; 256]);
        }

        assert_eq!(sync.received.len(), 0, "post-rejection chunks must not buffer");
        assert_eq!(sync.received_total, 0);
        assert_eq!(sync.incoming_progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn host_oversize_dedup_skips_repoll() {
        // Once an RGBA hash is stamped as LastKind::OversizeImage, the next
        // poll tick with the same RGBA must hit the dedup branch BEFORE
        // re-encoding (M1: avoid 30-150ms CPU + duplicate warn-log every 500ms).
        // Drives the production `LastKind::matches_image_hash` method.
        let img = synthetic_rgba_4x4();
        let hash = hash_bytes(&img.bytes);

        assert!(!LastKind::None.matches_image_hash(hash), "first tick must NOT skip");

        let last_oversize = LastKind::OversizeImage(hash);
        assert!(
            last_oversize.matches_image_hash(hash),
            "repeated oversize must short-circuit"
        );

        // Different RGBA gives a different hash → must NOT skip.
        let mut other = img.bytes.to_vec();
        other[0] ^= 0xFF;
        let other_hash = hash_bytes(&other);
        assert!(
            !last_oversize.matches_image_hash(other_hash),
            "different RGBA must re-try encode path"
        );
    }
}
