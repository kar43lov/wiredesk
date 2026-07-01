//! Clipboard sync — Mac side.
//!
//! Polls the local Mac clipboard once per CLIP_POLL_INTERVAL. When the text
//! changes we send it to Host via outgoing channel as a ClipOffer + N×ClipChunk.
//! Incoming clipboard messages are reassembled and written to the Mac
//! clipboard. A hash of the last known content is tracked so we don't echo
//! back what we just received (loop avoidance).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wiredesk_protocol::clip_file::{
    MAX_FILE_BYTES, MAX_FILE_PAYLOAD_BYTES, pack_first_chunk, sanitize_basename, unpack_first_chunk,
};
use wiredesk_protocol::message::{FORMAT_FILE, FORMAT_PNG_IMAGE, FORMAT_TEXT_UTF8, Message};
use wiredesk_protocol::packet::Packet;

use crate::app::TransportEvent;
use crate::clipboard_files;

const CLIP_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// Per-chunk byte cap. Bumped 256 → 1024 alongside the host-side
/// constant so a 20 MB image fits within the u16 chunk-index space
/// (1024 × 65535 ≈ 64 MB). Each chunk stays well under MAX_PAYLOAD = 4096.
const CHUNK_SIZE: usize = 1024;
const MAX_CLIPBOARD_BYTES: usize = 256 * 1024; // 256 KB cap for text
/// Maximum encoded PNG size we will push to the peer. Larger payloads are
/// dropped with a warning (and a UI toast wired up in Task 7b). The cap is
/// applied to the encoded-PNG length, not the RGBA pre-image, because PNG
/// compression ratios are content-dependent and we cannot predict the size
/// from raw dimensions.
/// Bumped 1 MB → 20 MB with the BLE transport (Plan C). Serial path
/// still won't push 20 MB sensibly (~30-min transfer at 11 KB/s) —
/// the cap is generous, not a performance guarantee.
pub(crate) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024; // 20 MB encoded

/// Hashes of the most recent clipboard content we observed/wrote, kept
/// in **independent slots per kind**. Without per-kind slots an alternating
/// text/image clipboard (e.g., a Whispr Flow dictation app writes text
/// while a screenshot stays on the OS clipboard) would loop: each text
/// write erases the image hash → next poll sees image as "new" → resends.
/// Bug captured in `2026-05-03 09:24` log session.
///
/// Text slot holds a small ring buffer (`TEXT_HISTORY`) of recent hashes,
/// not just the latest. Whispr Flow / TextExpander-style apps "save→
/// write→paste→restore" the clipboard around their inject — that
/// produces the alternating sequence `prev→new→prev→new` and a
/// single-hash slot would re-send `prev` on every restore. The history
/// ring lets us skip both `new` AND `prev` since both were just seen.
#[derive(Debug, Clone, Default)]
pub(crate) struct LastSeen {
    /// Last N text hashes (most recent first). N = `TEXT_HISTORY`.
    pub text_history: std::collections::VecDeque<u64>,
    /// Successfully sent/received image hash (over RGBA bytes).
    pub image: Option<u64>,
    /// RGBA hash of the most recent image rejected by the size cap. Lets the
    /// poll thread short-circuit the expensive RGBA→PNG re-encode (and the
    /// repeated toast emission) for the same buffer on every 500 ms tick —
    /// AC4 expects one toast per oversize event, not one per poll.
    pub oversize_image: Option<u64>,
    /// Hash of the most recent file content (raw file bytes) sent or
    /// received. Independent slot so a file sync doesn't compete with
    /// text/image dedup — the OS clipboard can carry multiple types.
    pub file: Option<u64>,
    /// Hash of the most recent file rejected by the size cap. Mirrors
    /// `oversize_image` — lets the poll thread short-circuit re-reading
    /// the same oversize file every tick (and avoid repeated toasts).
    pub oversize_file: Option<u64>,
}

/// How many recent text hashes the dedup history retains. 4 covers a
/// Whispr-Flow inject cycle (prev → transcript → prev) plus a buffer.
const TEXT_HISTORY: usize = 4;

impl LastSeen {
    /// True when the given RGBA hash matches either the last sent/received
    /// image OR the last oversize-rejected image. Poll path uses this to
    /// skip the expensive RGBA→PNG re-encode for the same buffer.
    pub(crate) fn matches_image_hash(&self, hash: u64) -> bool {
        self.image == Some(hash) || self.oversize_image == Some(hash)
    }

    pub(crate) fn matches_text_hash(&self, hash: u64) -> bool {
        self.text_history.contains(&hash)
    }

    /// True when the given content hash matches either the last sent/received
    /// file OR the last oversize-rejected file. Poll path uses this to skip
    /// re-reading the same file (and re-emitting toasts) on every tick.
    pub(crate) fn matches_file_hash(&self, hash: u64) -> bool {
        self.file == Some(hash) || self.oversize_file == Some(hash)
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
    File(u64),
    OversizeFile(u64),
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
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn set_text(&self, hash: u64) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        // Skip duplicate (already at front of history).
        if g.text_history.front() == Some(&hash) {
            return;
        }
        // Move to front if already present elsewhere — keeps the LRU order.
        if let Some(pos) = g.text_history.iter().position(|h| *h == hash) {
            g.text_history.remove(pos);
        }
        g.text_history.push_front(hash);
        while g.text_history.len() > TEXT_HISTORY {
            g.text_history.pop_back();
        }
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

    /// Mark `hash` as the most recently sent/received file content (raw bytes).
    /// Clears any matching `oversize_file` stamp so a re-copied smaller buffer
    /// with the same hash isn't blocked by the prior cap-rejection mark.
    pub(crate) fn set_file(&self, hash: u64) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        g.file = Some(hash);
        // Successful file send/receive clears any prior oversize-stamp
        // for the same buffer — buffer's now delivered, not rejected.
        if g.oversize_file == Some(hash) {
            g.oversize_file = None;
        }
    }

    /// Mark `hash` as the most recently size-rejected file content. Lets the
    /// next poll tick short-circuit re-reading the same file (and re-emitting
    /// the toast) until the user actually copies something else.
    pub(crate) fn set_oversize_file(&self, hash: u64) {
        let mut g = self.last.lock().unwrap_or_else(|e| e.into_inner());
        g.oversize_file = Some(hash);
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
            LastKind::File(h) => self.set_file(h),
            LastKind::OversizeFile(h) => self.set_oversize_file(h),
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

/// Pure-helper error returned when a file's on-disk size exceeds the per-
/// transfer cap. Mirrors `ImageTooLarge` so the poll thread can branch on a
/// typed error without going through `std::io::Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileTooLarge {
    pub size_bytes: usize,
}

/// Pure helper used both by the poll thread (with `MAX_FILE_BYTES`) and the
/// unit tests (with a low limit so synthetic fixtures can exercise the
/// oversize branch). Returns `Err` if `size_bytes > limit`. Encapsulates the
/// "is this file too big to ship" decision so it stays unit-testable without
/// touching the filesystem.
pub(crate) fn check_file_size(size_bytes: usize, limit: usize) -> Result<(), FileTooLarge> {
    if size_bytes > limit {
        Err(FileTooLarge { size_bytes })
    } else {
        Ok(())
    }
}

/// Human-readable toast string for files dropped at the size cap. Reports KB
/// like `format_oversize_toast` for consistency between image and file caps.
///
/// Signature takes `limit` for parity with the host-side variant; the Mac
/// wording stays terse on purpose because the chrome panel toast slot is
/// narrow and the user already knows the cap. The host variant is verbose
/// because it surfaces through a Win11 tray balloon where the formal
/// `"X KB > Y KB limit"` phrasing reads better. Same divergence applies to
/// the image-cap path (`format_oversize_toast` vs the host inline format).
pub(crate) fn format_oversize_file_toast(e: &FileTooLarge, _limit: usize) -> String {
    format!(
        "file too large ({} KB), copy a smaller file",
        e.size_bytes / 1024
    )
}

/// Result of running the outbound file-poll helper on a single pasteboard
/// path. Captures both success and the oversize-skip branch so the poll
/// thread (and unit tests) can stamp dedup slots / emit toasts without
/// touching the filesystem from inside the test runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FilePollOutcome {
    /// File read successfully. Caller emits offer + chunks then stamps `LastSeen.file`.
    Ready { name: String, hash: u64, packed: Vec<u8> },
    /// File exceeded `limit`. Caller stamps `LastSeen.oversize_file(path_hash)`
    /// and emits the toast.
    Oversize { path_hash: u64, err: FileTooLarge },
    /// Path failed sanity (empty basename, IO error, pack failure). Caller
    /// logs and skips this tick without stamping anything.
    Skipped(&'static str),
}

/// Outcome of the startup pre-stamp probe for a file URL on the pasteboard.
/// Mirrors the shape of `FilePollOutcome` but only carries the bits the
/// pre-stamp path needs (no packed payload — we never emit, we only stamp).
///
/// Task 9b: at process boot the poll thread inspects the OS clipboard once
/// and records hashes of whatever's already there so the first poll tick
/// after launch doesn't re-upload the user's pre-existing clipboard. Files
/// need their own variant because the content lives behind a path, not
/// inline like text/image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FilePreStampOutcome {
    /// File read successfully; caller stamps `LastSeen.file(hash)`.
    Stamped { name: String, hash: u64 },
    /// File exceeded `limit`. Caller logs a warning and skips stamping —
    /// stamping the oversize slot pre-emptively would suppress the user's
    /// next genuine attempt to copy that same file (they might want the
    /// toast). Let the runtime poll tick handle it.
    Oversize { size_bytes: usize },
    /// Path failed sanity (empty basename, IO error, …). Caller logs at
    /// debug and skips.
    Skipped(&'static str),
}

/// Pre-stamp helper for a file URL discovered on the pasteboard at startup.
///
/// Symmetric with `pack_file_or_warn` but stripped of the packing step — we
/// don't need to build a wire payload, we just need the content hash so the
/// runtime poll-tick dedup short-circuits on the same content.
///
/// Pure helper: I/O is unavoidable (we have to read the file to hash content),
/// but pulled out of the startup block so unit tests can drive it with a
/// `tempfile`-backed path and a low limit.
pub(crate) fn pre_stamp_file_path(
    path: &std::path::Path,
    limit: usize,
) -> FilePreStampOutcome {
    use std::fs;

    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) if !n.is_empty() => n.to_owned(),
        _ => return FilePreStampOutcome::Skipped("empty or non-UTF-8 basename"),
    };

    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return FilePreStampOutcome::Skipped("stat failed"),
    };
    let size_u64 = meta.len();
    let size_usize = size_u64 as usize;
    if (size_u64 as u128) != (size_usize as u128) {
        return FilePreStampOutcome::Skipped("file larger than usize");
    }

    if check_file_size(size_usize, limit).is_err() {
        return FilePreStampOutcome::Oversize { size_bytes: size_usize };
    }

    let content = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return FilePreStampOutcome::Skipped("read failed"),
    };

    let hash = hash_bytes(&content);
    FilePreStampOutcome::Stamped { name, hash }
}

/// Pure(-ish) helper for the outbound file branch. Reads `path`, hashes the
/// content, checks the size cap, and packs the first chunk via
/// `wiredesk_protocol::clip_file::pack_first_chunk`.
///
/// Hashing is over **content** (not filename) — copy-rename-paste produces the
/// same hash → dedup catches it. Path-hash is used only to stamp the
/// oversize slot so a sticky too-big file doesn't re-toast every tick.
///
/// I/O is unavoidable (we have to read the file), but the helper is pulled out
/// of `spawn_poll_thread` so a unit test can drive it directly with a
/// `tempfile`-backed path.
pub(crate) fn pack_file_or_warn(
    path: &std::path::Path,
    limit: usize,
) -> FilePollOutcome {
    use std::fs;

    let name = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) if !n.is_empty() => n.to_owned(),
        _ => return FilePollOutcome::Skipped("empty or non-UTF-8 basename"),
    };

    // Stat first so we can short-circuit oversize files without reading the
    // entire content into memory. `metadata().len()` is u64; cast checked.
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return FilePollOutcome::Skipped("stat failed"),
    };
    let size_u64 = meta.len();
    let size_usize = size_u64 as usize;
    if (size_u64 as u128) != (size_usize as u128) {
        return FilePollOutcome::Skipped("file larger than usize");
    }

    if let Err(e) = check_file_size(size_usize, limit) {
        // Hash the path (not the content) so the dedup stamp is cheap and
        // stable across ticks without re-reading the oversize file. Different
        // file at same path → different mtime won't help us, but a NEW file
        // path → different hash → toast re-emitted as expected.
        let path_hash = hash_bytes(path.to_string_lossy().as_bytes());
        return FilePollOutcome::Oversize { path_hash, err: e };
    }

    let content = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return FilePollOutcome::Skipped("read failed"),
    };

    let hash = hash_bytes(&content);
    let packed = match pack_first_chunk(&name, &content) {
        Ok(p) => p,
        Err(_) => return FilePollOutcome::Skipped("pack failed"),
    };
    FilePollOutcome::Ready { name, hash, packed }
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
#[cfg(test)]
pub(crate) fn apply_outgoing_progress(
    msg: &Message,
    outgoing_progress: &Arc<AtomicU64>,
    outgoing_total: &Arc<AtomicU64>,
) {
    apply_outgoing_progress_inner(msg, outgoing_progress, outgoing_total, None);
}

/// Format-specific toast string surfaced when the host returns a
/// `ClipDecline` for our outgoing offer (Task 7d).
///
/// FORMAT_FILE produces the explicit "Peer declined file" message the
/// brief calls for; other formats fall back to the generic copy used by
/// previous releases. Pure helper — unit-tested without spinning up the
/// reader thread.
pub fn send_decline_toast(format: u8) -> String {
    match format {
        FORMAT_FILE => "Peer declined file (Receive files off)".to_string(),
        _ => "Host declined the clipboard transfer".to_string(),
    }
}

/// React to a host-side `ClipDecline { format }` arriving in the reader
/// thread (Task 7d). Pure helper: flips the shared `outgoing_cancel` flag —
/// which the writer-thread reads to drain any queued ClipOffer/ClipChunk
/// packets from the outbox instead of pumping them onto the wire — and
/// returns the toast string the GUI should display.
///
/// Extracted so the contract ("decline → cancel armed + toast string for
/// UI") is unit-testable without spinning up the real reader/writer
/// threads. `main.rs` calls this and dispatches the toast to its mpsc
/// channel; tests can call it directly and assert both the cancel-flag
/// state transition and the wording.
pub fn apply_clip_decline(format: u8, outgoing_cancel: &Arc<AtomicBool>) -> String {
    log::info!("clipboard: host declined our offer (format={format}); aborting send");
    outgoing_cancel.store(true, Ordering::Release);
    send_decline_toast(format)
}

/// Same as [`apply_clip_decline`] but also clears the `current_outgoing_label`
/// slot. When a FORMAT_FILE offer is declined mid-flight, the writer thread
/// drops queued ClipOffer/ClipChunk packets without ever running
/// [`apply_outgoing_progress_with_label`] (which clears the label on DONE),
/// so the status bar would otherwise stick at "Sending file 'X.pdf' — 0/N KB"
/// until disconnect or the next outgoing file. Clearing the slot here keeps
/// the UI honest.
pub fn apply_clip_decline_with_label(
    format: u8,
    outgoing_cancel: &Arc<AtomicBool>,
    current_outgoing_label: &Arc<Mutex<String>>,
) -> String {
    let toast = apply_clip_decline(format, outgoing_cancel);
    if let Ok(mut g) = current_outgoing_label.lock() {
        g.clear();
    }
    toast
}

/// Format-label string for the wire-progress START/DONE log line.
///
/// Task 7d: distinct labels for text / image / file make grep/tail of
/// `client.log` more useful when investigating clipboard incidents — at
/// 3 Mbaud a 20 MB file send + a 256 KB text send happen back-to-back and
/// numeric format codes lose context.
pub(crate) fn format_label(format: u8) -> &'static str {
    match format {
        FORMAT_TEXT_UTF8 => "TEXT",
        FORMAT_PNG_IMAGE => "IMAGE",
        FORMAT_FILE => "FILE",
        _ => "UNKNOWN",
    }
}

/// Same as [`apply_outgoing_progress`] but with an optional
/// `current_outgoing_label` slot which is cleared when the transfer
/// completes (FORMAT_FILE: status-line filename is no longer relevant
/// once DONE fires). For text/image transfers the slot is left untouched —
/// it should already be empty.
pub(crate) fn apply_outgoing_progress_with_label(
    msg: &Message,
    outgoing_progress: &Arc<AtomicU64>,
    outgoing_total: &Arc<AtomicU64>,
    current_outgoing_label: &Arc<Mutex<String>>,
) {
    apply_outgoing_progress_inner(
        msg,
        outgoing_progress,
        outgoing_total,
        Some(current_outgoing_label),
    );
}

fn apply_outgoing_progress_inner(
    msg: &Message,
    outgoing_progress: &Arc<AtomicU64>,
    outgoing_total: &Arc<AtomicU64>,
    current_outgoing_label: Option<&Arc<Mutex<String>>>,
) {
    match msg {
        Message::ClipOffer { format, total_len } => {
            outgoing_total.store(*total_len as u64, Ordering::Relaxed);
            outgoing_progress.store(0, Ordering::Relaxed);
            log::info!(
                "clipboard.send START format={} total={total_len} bytes",
                format_label(*format)
            );
        }
        Message::ClipChunk { data, .. } => {
            let prev = outgoing_progress.fetch_add(data.len() as u64, Ordering::Relaxed);
            let new_progress = prev + data.len() as u64;
            let total = outgoing_total.load(Ordering::Relaxed);
            // Milestone logging — every 25% of total. `checked_div` keeps
            // the divide-by-zero guard idiomatic (total == 0 → None → no log)
            // instead of a manual `if total > 0` wrapper.
            if let (Some(prev_q), Some(new_q)) =
                ((prev * 4).checked_div(total), (new_progress * 4).checked_div(total))
            {
                if new_q > prev_q {
                    log::info!(
                        "clipboard.send {}/{} bytes ({}%)",
                        new_progress,
                        total,
                        (new_progress * 100).checked_div(total).unwrap_or(0)
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
                // Task 7d: clear the filename slot once the file transfer
                // completes — the status-line should fall back to the
                // generic "Sending clipboard" label for the next text/image
                // send (which doesn't touch this slot).
                if let Some(slot) = current_outgoing_label {
                    if let Ok(mut g) = slot.lock() {
                        g.clear();
                    }
                }
            }
        }
        _ => {}
    }
}

/// What the poll thread should do with a freshly-probed clipboard text value.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TextSendDecision {
    /// Ship it now (stable across two ticks, or a synthetic-paste kick).
    Send,
    /// New/changing value — hold one tick and re-check next poll.
    Hold,
    /// Already the last value we sent — nothing to do.
    Skip,
}

/// Decide whether outbound clipboard text should ship this tick.
///
/// `already_sent` — the value equals our last committed `LastSeen.text`.
/// `pending` — the hash we held on the *previous* poll tick (debounce state).
/// `kicked` — a synthetic Cmd+V (Whispr Flow) woke us this tick.
///
/// Debounce rationale: copy-on-select terminals (Ghostty
/// `copy-on-select = clipboard`, iTerm) rewrite the pasteboard on every
/// selection-extend while the user drags the mouse. Each intermediate
/// fragment (`ping`, `-n`, …) has a distinct hash, so without a stability
/// gate every fragment races onto the wire as its own clipboard sync and
/// Host pastes whichever landed last. Requiring the value to survive two
/// consecutive poll ticks drops the transient drag states and ships only the
/// settled selection. Synthetic Cmd+V bypasses the gate: Whispr writes the
/// clipboard exactly once and the paste dispatcher's wait-gate needs the
/// value on the wire immediately, with no extra tick of latency.
///
/// Known limitation (accepted): the debounce opens a ~400 ms window where a
/// freshly-copied value is held while a *physical* Cmd+V (which is forwarded
/// to Host immediately, unlike synthetic paste, with no wait-gate) would land
/// on Host's previous clipboard. In WireDesk's capture architecture this is
/// practically unreachable: forwarding a physical Cmd+V to Host requires
/// capture-mode, but producing a new Mac clipboard value (mouse-select in
/// Ghostty / Cmd+C) requires being OUT of capture — the two states are
/// mutually exclusive and switching between them takes far longer than the
/// window. Gating physical paste like synthetic paste would add latency to
/// every Ctrl+V on Host, which isn't worth closing an unreachable gap.
///
/// Known limitation #2 (accepted): for a single clipboard item that carries
/// BOTH text and an image/file (rich browser/Word selection), the held text
/// ships one tick AFTER the image/file branches, and since Host commits each
/// format with `EmptyClipboard` (single-format clipboard), the late text
/// overwrites the binary content — Host ends up with text instead of the
/// image. Pure-text (terminal copy-on-select) and pure-image (screenshot)
/// clipboards are unaffected; only mixed items regress, which is rare in the
/// terminal-centric workflow. A full fix needs NSPasteboard type-probing to
/// detect mixed items and skip the debounce for them — disproportionate here.
///
/// Pure so it's unit-testable without spinning the poll thread.
pub(crate) fn decide_text_send(
    hash: u64,
    already_sent: bool,
    pending: Option<u64>,
    kicked: bool,
) -> TextSendDecision {
    if already_sent {
        TextSendDecision::Skip
    } else if kicked || pending == Some(hash) {
        TextSendDecision::Send
    } else {
        TextSendDecision::Hold
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
/// Shared `outgoing_text_in_flight` is set true while a Mac→Host text-clipboard
/// sync is in flight on the wire; the synthetic-combo dispatcher holds
/// Whispr-Flow-style synthetic Cmd+V until this clears (plus a small grace)
/// so the paste lands on the *new* clipboard, not the previous one.
///
/// `poll_kick_rx` lets the keyboard tap wake this thread immediately on
/// detection of a synthetic Cmd+V (e.g. Whispr Flow paste). Without the
/// kick, Whispr can fire its Cmd+V while we're mid-`thread::sleep`, miss
/// the next 500/200 ms tick, and the dispatcher's wait-for-in-flight gate
/// never trips for the *current* clipboard write.
#[allow(clippy::too_many_arguments)]
pub fn spawn_poll_thread(
    state: ClipboardState,
    outgoing_tx: mpsc::Sender<Packet>,
    events_tx: mpsc::Sender<TransportEvent>,
    send_images: Arc<AtomicBool>,
    send_text: Arc<AtomicBool>,
    send_files: Arc<AtomicBool>,
    outgoing_text_in_flight: Arc<AtomicBool>,
    poll_kick_rx: mpsc::Receiver<()>,
    current_outgoing_label: Arc<Mutex<String>>,
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

        // Tracks NSPasteboard.changeCount for the file-URL branch. Initialised
        // at -1 so the first poll always inspects the pasteboard (changeCount
        // is monotonically increasing from 0). `clipboard_files::poll_file_url`
        // bumps the counter eagerly so a sticky multi-file selection or
        // non-file URL doesn't trigger a re-scan on every tick.
        let mut file_change_count: i64 = -1;

        // Startup pre-stamp for file URLs (Task 9b). Mirrors the text/image
        // blocks above: if the pasteboard already carries a file URL when the
        // app launches, hash its content and stamp `LastSeen.file` so the
        // FIRST poll tick doesn't re-upload a file the user copied during a
        // previous session. Oversize files are NOT stamped — let the runtime
        // poll path show the toast on the user's next observation. This call
        // also bumps `file_change_count` to the current pasteboard value so
        // the runtime loop doesn't re-detect the same pre-existing URL as
        // "new" on its first tick (which would re-emit through the outbound
        // file branch despite the stamp racing the OS read).
        if let Some(path) = clipboard_files::poll_file_url(&mut file_change_count) {
            match pre_stamp_file_path(&path, MAX_FILE_BYTES) {
                FilePreStampOutcome::Stamped { name, hash } => {
                    state.set_file(hash);
                    log::info!(
                        "clipboard: pre-stamped existing file '{name}' ({} content hash) — not sending on startup",
                        hash
                    );
                }
                FilePreStampOutcome::Oversize { size_bytes } => {
                    log::warn!(
                        "clipboard: pre-existing file at {} is {} bytes (over {} cap) — skipping stamp; runtime poll will surface the toast",
                        path.display(),
                        size_bytes,
                        MAX_FILE_BYTES,
                    );
                }
                FilePreStampOutcome::Skipped(reason) => {
                    log::debug!(
                        "clipboard: pre-existing file at {} not stamped — {reason}",
                        path.display()
                    );
                }
            }
        }

        // Debounce state for outbound text: the hash we saw last tick but did
        // not yet ship (waiting to confirm it's stable, not a copy-on-select
        // drag fragment). See `decide_text_send`.
        let mut pending_text_hash: Option<u64> = None;

        loop {
            // Sleep up to CLIP_POLL_INTERVAL, but wake immediately if the
            // keyboard tap signals that a synthetic Cmd+V just fired —
            // we need to read the freshly-updated clipboard before the
            // dispatcher's wait-on-in-flight gate has a chance to rely
            // on stale state. Drain any extra kicks queued during the
            // active poll cycle so we don't poll twice in a row.
            // `paste_kicked` means a synthetic Cmd+V paste (Whispr Flow) woke
            // us — the kick is sent ONLY for real paste events
            // (`is_synthetic_paste` in keyboard_tap), and it bypasses the text
            // debounce so the freshly written value reaches the wire this tick
            // before the paste dispatcher's wait-gate checks it.
            // A kick that lands between `recv_timeout` returning Timeout and
            // the drain below must still count — otherwise the drain swallows
            // it with the flag left false and the Whispr text is held one tick
            // instead of shipped immediately.
            let mut paste_kicked = poll_kick_rx.recv_timeout(CLIP_POLL_INTERVAL).is_ok();
            while poll_kick_rx.try_recv().is_ok() {
                paste_kicked = true;
            }

            // Clear the in-flight flag at the start of each tick. By now
            // the previous text-send (if any) has been drained from
            // outgoing_tx by writer_thread, written on the wire, and Host
            // has committed — 500 ms is plenty for typical 50–500 byte
            // dictation strings (~5 ms wire time at 11 KB/s). The
            // synthetic-combo dispatcher uses this flag to defer
            // Whispr-Flow's Cmd+V until the new clipboard reaches Host.
            outgoing_text_in_flight.store(false, Ordering::Release);

            // 1) Probe file FIRST, before text. Finder's Cmd+C on a file
            // eventually exposes the filename as plain text on the same
            // pasteboard item, ALONGSIDE the `public.file-url` entry — but
            // Finder resolves that text representation lazily (observed
            // delay ranges from ~200ms to ~9s after the file-url appears,
            // presumably a promise/lazy-provider resolving on its own
            // schedule). Once it lands, the text debounce ships it, it
            // queues behind the (possibly still-in-flight) file transfer on
            // the wire, and Host commits each format via its own
            // `EmptyClipboard` — so the filename-text silently overwrites
            // the file we just delivered (100% repro on every single-file
            // Finder copy, confirmed via host.log: `clipboard.recv DONE
            // <file>` immediately followed by `clipboard.recv DONE
            // <len(filename utf8)>`, then Explorer paste yields the
            // filename string instead of the file). Since the delay isn't
            // bounded to "this tick" or even "a couple of ticks", the text
            // branch below instead compares the candidate text against
            // `current_outgoing_label` — which holds the just-sent file's
            // name for the file's entire on-wire lifetime — and skips any
            // text that matches it, regardless of which tick it surfaces on.
            'file: {
                // Always poll so `file_change_count` tracks the pasteboard
                // even while the opt-in is OFF. If we skipped the probe when
                // disabled, the counter would go stale; enabling "Send files"
                // later would then make the next tick treat an
                // already-copied file as new and send it WITHOUT a fresh
                // Cmd+C — leaking a file the user copied while sending was
                // off, breaking the opt-in privacy guarantee (Codex review).
                let path = match clipboard_files::poll_file_url(&mut file_change_count) {
                    Some(p) => p,
                    None => break 'file,
                };
                // Detection done (counter synced) — only SEND when the
                // "Send files (Mac → Host)" toggle is on. Default off, so a
                // plain Cmd+C on a file never leaves the Mac.
                if !send_files.load(Ordering::Relaxed) {
                    break 'file;
                }

                match pack_file_or_warn(&path, MAX_FILE_BYTES) {
                    FilePollOutcome::Ready { name, hash, packed } => {
                        // Dedup against both file and oversize_file slots. The
                        // matches_file_hash covers both so a re-copied small file
                        // doesn't get re-emitted, and a previously-rejected
                        // oversize file with the same content hash (impossible
                        // in practice — content + size both checked — but
                        // defensive) also short-circuits.
                        if state.get().matches_file_hash(hash) {
                            break 'file;
                        }
                        // NB: do NOT stamp `state.set_file(hash)` here. The
                        // pasteboard `changeCount` guard in `poll_file_url`
                        // already prevents re-emitting the same clipboard on
                        // later ticks, so a send-side stamp is redundant — and
                        // it broke retry: after a host ClipDecline (receive
                        // files off) or a user cancel, a re-copy of the *same*
                        // file would match the stale stamp and never resend
                        // (Codex review). Loop-avoidance for the Host→Mac
                        // receive path is handled by `commit_file`, which
                        // stamps the content hash before writing the file URL
                        // back to the pasteboard. The `matches_file_hash`
                        // check above still consults that commit stamp.
                        log::debug!(
                            "clipboard: pushing file '{}' to host ({} content bytes)",
                            name,
                            packed.len(),
                        );
                        // Task 7d: stash the filename so the UI status-line
                        // can render "Sending file 'X.pdf' — ..." instead
                        // of the generic "Sending clipboard". `apply_outgoing_progress`
                        // clears it when the transfer reaches DONE.
                        if let Ok(mut g) = current_outgoing_label.lock() {
                            *g = name.clone();
                        }
                        emit_offer_and_chunks(&outgoing_tx, FORMAT_FILE, &packed);
                    }
                    FilePollOutcome::Oversize { path_hash, err } => {
                        // Use the path hash (not content hash) to stamp the
                        // oversize slot — we never read the content, and a
                        // different file at the same path would have a
                        // different path string anyway.
                        if state.get().matches_file_hash(path_hash) {
                            break 'file;
                        }
                        log::warn!(
                            "clipboard: file too large ({} bytes, limit {}), skipping",
                            err.size_bytes,
                            MAX_FILE_BYTES,
                        );
                        let _ = events_tx.send(TransportEvent::Toast(
                            format_oversize_file_toast(&err, MAX_FILE_BYTES),
                        ));
                        state.set_oversize_file(path_hash);
                    }
                    FilePollOutcome::Skipped(reason) => {
                        log::debug!("clipboard: file poll skipped — {reason}");
                    }
                }
            }

            // 2) Probe text. Independent dedup slot (`LastSeen.text`) so
            // an alternating text/image clipboard (Whispr Flow + a
            // standing screenshot) doesn't loop. Runtime toggle gates
            // the path entirely.
            if send_text.load(Ordering::Relaxed) {
                // Probe text. A non-empty value runs the debounce; anything
                // else (empty clipboard, or an image/file-only clipboard where
                // `get_text` errs) must DROP any pending debounce hash —
                // otherwise a stale pending survives the non-text interlude
                // and a later re-copy of the same text would match it and ship
                // on its first sighting, skipping the stability gate.
                match clip.get_text() {
                    Ok(text) if !text.is_empty() => {
                        let hash = hash_text(&text);
                        // Race guard (see comment above the file probe): a
                        // candidate text value that exactly matches the name
                        // of a file transfer still on the wire is Finder's
                        // lazily-resolved filename-as-text for the SAME
                        // Cmd+C, not a genuine independent text copy. Stamp
                        // it as already-sent (without shipping) so it never
                        // reaches the wire and overwrites the file on Host.
                        let is_pending_file_name = current_outgoing_label
                            .lock()
                            .map(|g| !g.is_empty() && *g == text)
                            .unwrap_or(false);
                        if is_pending_file_name {
                            state.set_text(hash);
                            pending_text_hash = None;
                        } else {
                            let already_sent = state.get().matches_text_hash(hash);
                            match decide_text_send(
                                hash,
                                already_sent,
                                pending_text_hash,
                                paste_kicked,
                            ) {
                                TextSendDecision::Skip => {
                                    pending_text_hash = None;
                                }
                                TextSendDecision::Hold => {
                                    // copy-on-select drag fragment (or any value
                                    // not yet stable for two ticks) — hold and
                                    // re-check next poll instead of shipping it.
                                    pending_text_hash = Some(hash);
                                }
                                TextSendDecision::Send => {
                                    pending_text_hash = None;
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
                                        outgoing_text_in_flight
                                            .store(true, Ordering::Release);
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
                    _ => {
                        pending_text_hash = None;
                    }
                }
            } else {
                // Text sending disabled — drop pending so re-enabling later
                // doesn't treat a value held before the toggle as "stable".
                pending_text_hash = None;
            }

            // 3) Probe image. Independent dedup slot. Runtime toggle gates.
            // Note: probing both text AND image in the same tick (instead
            // of falling through only on text-empty) is intentional — the
            // OS clipboard can hold both. This closes the codex C3 gap.
            //
            // The image branch is wrapped in a labeled inner block so each
            // early-exit (cap rejection, encode failure, etc.) falls through
            // to the end of the tick instead of skipping the whole tick.
            // The OS clipboard can carry text + image + file URLs from the
            // same Cmd+C; we don't want a stale image to suppress file sync.
            'image: {
                if !send_images.load(Ordering::Relaxed) {
                    break 'image;
                }
                let img = match clip.get_image() {
                    Ok(i) => i,
                    Err(_) => break 'image, // not an image
                };

                let hash = hash_bytes(&img.bytes);
                // Short-circuit BEFORE the expensive RGBA→PNG encode for both:
                // - already-sent images (LastSeen.image),
                // - already-rejected oversized images (LastSeen.oversize_image).
                // Otherwise every 500 ms tick re-encodes (~30-150 ms CPU) and
                // re-emits the toast for the SAME oversize buffer.
                if state.get().matches_image_hash(hash) {
                    break 'image;
                }

                let png = match encode_rgba_to_png(&img) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("clipboard: PNG encode failed: {e}");
                        break 'image;
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
                    break 'image;
                }

                // Codex iter3 E2 (acceptable): sender dedup is set on enqueue,
                // not on successful send. If transport fails mid-transfer, retry
                // happens only when clipboard content changes again. Acceptable:
                // heartbeat covers disconnect within 6s, app restart clears state.
                state.set_image(hash);

                log::debug!("clipboard: pushing image to host ({} encoded bytes)", png.len());
                emit_offer_and_chunks(&outgoing_tx, FORMAT_PNG_IMAGE, &png);
            }
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
    /// Runtime toggle (Settings → Receive files): when off, file offers
    /// (`format=FORMAT_FILE`) are rejected on receipt with `ClipDecline`.
    /// Wired in Task 7a; UI surface lands in Task 8.
    receive_files: Arc<AtomicBool>,
    /// Optional override for the cache directory used by inbound file
    /// commits. Production code leaves this `None` and the commit path falls
    /// back to `dirs::cache_dir()/WireDesk/`. Tests inject a tempdir so the
    /// file-write branch can be exercised without polluting the real cache.
    cache_dir_override: Option<PathBuf>,
    /// Path of the file most recently written by `commit_file()`. Tracked so
    /// that `reset()` (called on disconnect / new HelloAck / mid-transfer
    /// abort) can remove a partially-written file rather than leaving stale
    /// fragments in the cache. Cleared on a fresh `commit_file()`.
    in_flight_file_path: Option<PathBuf>,
    /// Test-only sink for the last successfully committed payload. Lets unit
    /// tests assert on what would have been written to the local clipboard
    /// without depending on the host platform's actual clipboard backend
    /// (which arboard cannot stub out portably).
    #[cfg(test)]
    last_committed: Option<CommittedPayload>,
}

/// What the most recent `commit()` produced. Test-only — production code
/// pushes straight to `arboard::Clipboard` (text/image) or `set_file_url`
/// (file).
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CommittedPayload {
    Text(String),
    Image { width: usize, height: usize, bytes: Vec<u8> },
    /// File written to the cache dir. Carries the absolute path, sanitized
    /// basename used to compose it, and the raw content for byte-equal
    /// roundtrip assertions.
    File { path: PathBuf, name: String, content: Vec<u8> },
}

impl IncomingClipboard {
    pub fn new(
        state: ClipboardState,
        incoming_progress: Arc<AtomicU64>,
        incoming_total: Arc<AtomicU64>,
        receive_images: Arc<AtomicBool>,
        receive_text: Arc<AtomicBool>,
        receive_files: Arc<AtomicBool>,
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
            receive_files,
            cache_dir_override: None,
            in_flight_file_path: None,
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
            receive_files: Arc::new(AtomicBool::new(true)),
            cache_dir_override: None,
            in_flight_file_path: None,
            last_committed: None,
        }
    }

    /// Test-only: redirect file commits to a caller-provided directory so
    /// tempdir-backed tests don't pollute the real `~/Library/Caches/WireDesk`.
    #[cfg(test)]
    fn set_cache_dir_override(&mut self, dir: PathBuf) {
        self.cache_dir_override = Some(dir);
    }

    /// Returns `Some(Message::ClipDecline)` when the offer is rejected
    /// for a *peer-policy* reason (Settings toggle disabled). The caller
    /// (reader thread) is expected to forward that decline back to the
    /// sender so the sender can drop its outbox and stop saturating the
    /// link with chunks the receiver will silently discard. Unsupported
    /// formats and over-cap offers don't trigger a decline — those are
    /// "the peer is broken" cases, not "the peer didn't ask for this".
    pub fn on_offer(&mut self, format: u8, total_len: u32) -> Option<Message> {
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
        if format != FORMAT_TEXT_UTF8 && format != FORMAT_PNG_IMAGE && format != FORMAT_FILE {
            log::warn!(
                "clipboard: incoming offer with unsupported format {format}, ignoring"
            );
            self.expected_len = 0;
            self.expected_format = 0;
            self.received.clear();
            self.received_total = 0;
            self.incoming_total.store(0, Ordering::Relaxed);
            self.incoming_progress.store(0, Ordering::Relaxed);
            return None;
        }
        // Runtime toggle (Settings → Receive images): drop incoming image
        // offers when the user disabled image receive. Text offers continue.
        if format == FORMAT_TEXT_UTF8 && !self.receive_text.load(Ordering::Relaxed) {
            log::info!(
                "clipboard: incoming text offer ({total_len} bytes) declined — receive_text disabled"
            );
            self.expected_len = 0;
            self.expected_format = 0;
            self.received.clear();
            self.received_total = 0;
            self.incoming_total.store(0, Ordering::Relaxed);
            self.incoming_progress.store(0, Ordering::Relaxed);
            return Some(Message::ClipDecline { format });
        }
        if format == FORMAT_PNG_IMAGE && !self.receive_images.load(Ordering::Relaxed) {
            log::info!(
                "clipboard: incoming image offer ({total_len} bytes) declined — receive_images disabled"
            );
            self.expected_len = 0;
            self.expected_format = 0;
            self.received.clear();
            self.received_total = 0;
            self.incoming_total.store(0, Ordering::Relaxed);
            self.incoming_progress.store(0, Ordering::Relaxed);
            return Some(Message::ClipDecline { format });
        }
        if format == FORMAT_FILE && !self.receive_files.load(Ordering::Relaxed) {
            log::info!(
                "clipboard: incoming file offer ({total_len} bytes) declined — receive_files disabled"
            );
            self.expected_len = 0;
            self.expected_format = 0;
            self.received.clear();
            self.received_total = 0;
            self.incoming_total.store(0, Ordering::Relaxed);
            self.incoming_progress.store(0, Ordering::Relaxed);
            return Some(Message::ClipDecline { format });
        }
        // Bound peer-supplied total_len to local caps before allocating any
        // state. Without this a malicious or buggy peer could ask us to
        // allocate up to 4 GB inside `commit()` (Vec::with_capacity).
        let total_len_usize = total_len as usize;
        let over_cap = match format {
            FORMAT_PNG_IMAGE => total_len_usize > MAX_IMAGE_BYTES,
            FORMAT_TEXT_UTF8 => total_len_usize > MAX_CLIPBOARD_BYTES,
            // File cap = MAX_FILE_BYTES + MAX_FILENAME_LEN + 2-byte name_len
            // header. Centralised in `clip_file::MAX_FILE_PAYLOAD_BYTES` so
            // both sides stay in sync with `pack_first_chunk`'s wire layout.
            FORMAT_FILE => total_len_usize > MAX_FILE_PAYLOAD_BYTES,
            _ => false,
        };
        if over_cap {
            log::warn!(
                "clipboard: incoming offer too large (format={format}, {total_len} bytes), ignoring"
            );
            // Full reset() — drops reassembly state AND removes any hypothetical
            // partial file from a prior aborted commit (reset() handles the
            // in_flight_file_path slot). Chunks for this oversized offer
            // will be dropped by on_chunk's expected_len==0 guard.
            self.reset();
            return None;
        }
        self.expected_len = total_len;
        self.expected_format = format;
        self.received.clear();
        self.received_total = 0;
        self.incoming_total.store(total_len as u64, Ordering::Relaxed);
        self.incoming_progress.store(0, Ordering::Relaxed);
        log::info!("clipboard.recv START format={format} total={total_len} bytes");
        None
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
    ///
    /// Also removes any partially-written file from the cache directory so
    /// an aborted file transfer doesn't leave a truncated fragment behind.
    /// (This applies only when `commit_file` already wrote bytes before the
    /// disconnect — there's no streaming write path; the file is materialized
    /// once at commit time. The partial case is "first transfer's file was
    /// written, then a second offer arrived mid-stream and the first is now
    /// stale" — we remove it because the user's clipboard no longer points
    /// at it anyway.)
    pub fn reset(&mut self) {
        self.expected_len = 0;
        self.expected_format = 0;
        self.received.clear();
        self.received_total = 0;
        self.incoming_progress.store(0, Ordering::Relaxed);
        self.incoming_total.store(0, Ordering::Relaxed);
        if let Some(path) = self.in_flight_file_path.take() {
            match std::fs::remove_file(&path) {
                Ok(()) => log::debug!(
                    "clipboard: removed in-flight file on reset: {}",
                    path.display()
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => log::warn!(
                    "clipboard: failed to remove in-flight file {}: {e}",
                    path.display()
                ),
            }
        }
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
            FORMAT_FILE => self.commit_file(&buf),
            other => {
                log::warn!("clipboard: unknown format {other}, skipping {} bytes", buf.len());
            }
        }

        // Single source of truth for state-zeroing. `received` is already
        // empty here (mem::take above), so reset()'s clear() is a no-op.
        // After a successful commit_file the `in_flight_file_path` slot is
        // cleared inline (the file is now "delivered"), so reset() here
        // is a no-op for the file-cleanup branch.
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

    /// Commit a reassembled `FORMAT_FILE` payload: unpack `[name_len][name]
    /// [content]`, sanitize the basename, write the content to the cache
    /// directory, then point the OS pasteboard at the resulting file URL via
    /// `clipboard_files::set_file_url`.
    ///
    /// Failures (unpack / sanitize / IO) leave the receiver ready for a new
    /// offer. `set_file_url` errors are logged but don't fail the commit —
    /// the file is still in the cache, and a future poll-tick will see it on
    /// the pasteboard (or the user can drag it manually from the cache dir
    /// as a fallback).
    ///
    /// The dedup hash (LastSeen.file) is stamped on the *content* hash
    /// (matching the outbound branch in `pack_file_or_warn`) so a
    /// copy-rename-paste roundtrip doesn't loop.
    fn commit_file(&mut self, buf: &[u8]) {
        let (raw_name, content) = match unpack_first_chunk(buf) {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "clipboard: file unpack failed ({e}), dropping {} bytes",
                    buf.len()
                );
                return;
            }
        };
        let basename = sanitize_basename(&raw_name);
        let dir = self.resolve_cache_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("clipboard: cache dir create failed: {e}");
            return;
        }
        let path = dir.join(&basename);
        let hash = hash_bytes(&content);

        // Stamp the in-flight path BEFORE writing so a panic/abort mid-write
        // leaves a reset()-removable breadcrumb. After a successful write the
        // path is cleared (the file is "delivered" and the user owns it).
        self.in_flight_file_path = Some(path.clone());

        if let Err(e) = std::fs::write(&path, &content) {
            log::warn!("clipboard: file write failed at {}: {e}", path.display());
            // Clean up any partial bytes the OS wrote before the error. Leave
            // `in_flight_file_path` cleared so subsequent reset() doesn't
            // double-remove.
            let _ = std::fs::remove_file(&path);
            self.in_flight_file_path = None;
            return;
        }

        #[cfg(test)]
        {
            self.last_committed = Some(CommittedPayload::File {
                path: path.clone(),
                name: basename.clone(),
                content: content.clone(),
            });
        }

        // Point the OS pasteboard at the new file. FFI errors are non-fatal
        // — the bytes are on disk and the user can still recover them. We
        // skip the FFI call entirely on non-macOS (the stub returns
        // `PasteboardUnavailable`, which is noisy on the log but expected).
        //
        // CRITICAL: only stamp `LastSeen.file` when the pasteboard write
        // actually succeeded. If the FFI failed, the OS clipboard still
        // points at whatever the user had before — stamping our content
        // hash anyway would mean the next poll tick treats the user's
        // (un-replaced) clipboard as "fresh" and re-emits it, creating a
        // bidirectional re-emit loop. Mirrors `commit_text` / `commit_image`.
        #[cfg(target_os = "macos")]
        let wrote_ok = match clipboard_files::set_file_url(&path) {
            Ok(()) => {
                log::debug!(
                    "clipboard: wrote file {} ({} content bytes) from host",
                    path.display(),
                    content.len()
                );
                true
            }
            Err(e) => {
                log::warn!(
                    "clipboard: set_file_url failed for {}: {e}",
                    path.display()
                );
                false
            }
        };
        #[cfg(not(target_os = "macos"))]
        let wrote_ok = {
            log::debug!(
                "clipboard: wrote file {} ({} content bytes) from host (no pasteboard backend)",
                path.display(),
                content.len()
            );
            // No real pasteboard backend — treat as "ours" so tests stamp the
            // dedup slot deterministically. Matches `commit_text` / `commit_image`
            // which set `wrote_ok = self.clip.is_none()`.
            true
        };

        if wrote_ok {
            self.state.set_file(hash);
        }
        // Successfully delivered → file no longer "in-flight"; reset() won't
        // remove it on next disconnect. Even when the FFI failed we clear
        // the slot — the bytes are on disk and we don't want reset() to wipe
        // a file the user might still want to recover from cache.
        self.in_flight_file_path = None;
    }

    /// Resolve the directory where inbound files are materialised. Honours
    /// the test override; otherwise falls back to `dirs::cache_dir()
    /// /WireDesk` (e.g. `~/Library/Caches/WireDesk` on macOS).
    ///
    /// If `dirs::cache_dir()` returns `None` (extremely rare — only when
    /// `$HOME` is unset), we fall back to `std::env::temp_dir().join
    /// ("WireDesk")` so the commit doesn't fail outright on misconfigured
    /// environments.
    fn resolve_cache_dir(&self) -> PathBuf {
        if let Some(dir) = self.cache_dir_override.as_ref() {
            return dir.clone();
        }
        default_cache_dir()
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

/// Resolve the WireDesk cache directory used by inbound-file commits.
///
/// Mac convention: `dirs::cache_dir()` → `~/Library/Caches/WireDesk`.
/// `dirs::cache_dir()` only returns `None` if `$HOME` is unset (extremely
/// rare), in which case we fall back to `std::env::temp_dir()/WireDesk`
/// so callers always get a usable path. Shared with the startup vacuum
/// hook (`run_startup_vacuum`) so the directory it cleans matches the
/// one `IncomingClipboard::commit` writes into.
pub(crate) fn default_cache_dir() -> PathBuf {
    match dirs::cache_dir() {
        Some(d) => d.join("WireDesk"),
        None => std::env::temp_dir().join("WireDesk"),
    }
}

/// Run the cache-vacuum sweep at process startup. Removes inbound-file
/// cache entries older than `older_than` from `default_cache_dir()`.
///
/// Non-fatal: per-file errors are absorbed by `vacuum_cache_dir` (logged
/// at `warn`); a missing directory returns `Ok(0)` (first-run case);
/// any enumeration failure is logged at `warn` and the process keeps
/// booting. The 24h default matches the brief: cache lives a day, then
/// gets cleared on next start. Callers (main.rs) pass
/// `Duration::from_secs(24 * 3600)` for production.
pub fn run_startup_vacuum(older_than: Duration) {
    let dir = default_cache_dir();
    match wiredesk_core::cache_vacuum::vacuum_cache_dir(&dir, older_than) {
        Ok(0) => {
            log::debug!(
                "cache vacuum: nothing to remove under {}",
                dir.display()
            );
        }
        Ok(n) => {
            log::info!(
                "cache vacuum: removed {n} stale file(s) under {}",
                dir.display()
            );
        }
        Err(e) => {
            log::warn!(
                "cache vacuum: enumeration of {} failed: {e}",
                dir.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- decide_text_send (copy-on-select debounce) ---

    #[test]
    fn debounce_holds_new_value_then_sends_when_stable() {
        // Tick 1: brand-new value, nothing pending → hold.
        assert_eq!(
            decide_text_send(0xAAAA, false, None, false),
            TextSendDecision::Hold
        );
        // Tick 2: same value pending → confirmed stable → send.
        assert_eq!(
            decide_text_send(0xAAAA, false, Some(0xAAAA), false),
            TextSendDecision::Send
        );
    }

    #[test]
    fn debounce_keeps_holding_while_value_keeps_changing() {
        // Drag fragment A pending, now clipboard shows fragment B → still hold
        // (B is new this tick). This is the copy-on-select drag: every tick a
        // different hash, so nothing ever ships until the drag settles.
        assert_eq!(
            decide_text_send(0xBBBB, false, Some(0xAAAA), false),
            TextSendDecision::Hold
        );
    }

    #[test]
    fn already_sent_value_is_skipped_regardless_of_pending() {
        assert_eq!(
            decide_text_send(0xAAAA, true, None, false),
            TextSendDecision::Skip
        );
        assert_eq!(
            decide_text_send(0xAAAA, true, Some(0xAAAA), false),
            TextSendDecision::Skip
        );
    }

    #[test]
    fn synthetic_kick_bypasses_debounce() {
        // Whispr Flow synthetic Cmd+V: new value, nothing pending, but kicked
        // → send immediately (no extra tick of latency for dictation paste).
        assert_eq!(
            decide_text_send(0xCCCC, false, None, true),
            TextSendDecision::Send
        );
    }

    #[test]
    fn kick_on_already_sent_value_still_skips() {
        // A kick must not re-send a value we already committed (would loop on
        // Whispr's own paste of unchanged clipboard).
        assert_eq!(
            decide_text_send(0xCCCC, true, None, true),
            TextSendDecision::Skip
        );
    }

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
        assert!(s.text_history.is_empty() && s.image.is_none() && s.oversize_image.is_none());
    }

    #[test]
    fn clipboard_state_reset_clears_last_kind() {
        // Codex iter4 F1: `reset()` is the disconnect-side hook that drops
        // sender dedup. Without it, after a transfer aborts mid-stream the
        // hash stays stamped and the post-reconnect tick dedups → silent
        // lost-update. Verify each slot collapses to None.
        let state = ClipboardState::new();

        state.set_text(0xAABB_CCDD);
        assert!(state.get().matches_text_hash(0xAABB_CCDD));
        state.reset();
        assert!(state.get().text_history.is_empty());

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
        assert!(s.matches_text_hash(0x1111));
        assert_eq!(s.image, Some(0x2222));

        state.set_text(0x3333); // text update
        let s = state.get();
        assert!(s.matches_text_hash(0x3333));
        assert!(
            s.matches_text_hash(0x1111),
            "older text hash should remain in history (LRU)"
        );
        assert_eq!(s.image, Some(0x2222), "image hash must survive text update");
    }

    #[test]
    fn text_history_dedups_whispr_inject_pattern() {
        // Whispr Flow saves clipboard, writes transcript, paste's, restores.
        // The poll thread sees: prev → new → prev → newer → prev → ...
        // With a single-hash slot the resends loop forever; with the LRU
        // history both `prev` and `new` stay in window so neither resends
        // until something genuinely fresh arrives.
        let state = ClipboardState::new();
        state.set_text(0xAAAA); // prev (e.g. host-pushed text)
        state.set_text(0xBBBB); // Whispr writes transcript
        // Whispr restores prev — must NOT mark as new.
        assert!(state.get().matches_text_hash(0xAAAA));
        // New transcript still detected as new.
        assert!(!state.get().matches_text_hash(0xCCCC));
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
        assert!(!state.get().text_history.is_empty());
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
        assert!(s.text_history.is_empty() && s.image.is_none() && s.oversize_image.is_none());
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
        assert!(s.text_history.is_empty() && s.image.is_none() && s.oversize_image.is_none());
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
        assert!(s.text_history.is_empty() && s.image.is_none() && s.oversize_image.is_none());
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
    fn lastseen_file_slot_independent_from_image() {
        // Set the same hash value as image, then as file. Both slots must
        // hold it independently (R3 coverage — alternating image/file
        // clipboard mustn't loop the way single-slot dedup would).
        let state = ClipboardState::new();
        let h = 0xDEAD_BEEF_u64;
        state.set_image(h);
        state.set_file(h);
        let s = state.get();
        assert_eq!(s.image, Some(h));
        assert_eq!(s.file, Some(h));
        assert!(s.matches_image_hash(h));
        assert!(s.matches_file_hash(h));
    }

    #[test]
    fn lastseen_file_dedup_per_slot() {
        // Set a file hash. matches_file_hash(same) → true; matches_image_hash
        // for the same hash → false (slot independence).
        let state = ClipboardState::new();
        let h = 0x1234_5678_u64;
        state.set_file(h);
        let s = state.get();
        assert!(s.matches_file_hash(h));
        assert!(!s.matches_image_hash(h), "file hash must not match image slot");
        assert!(!s.matches_text_hash(h), "file hash must not match text slot");
    }

    #[test]
    fn reset_clears_file_slot() {
        // set_file → reset → file slot is None.
        let state = ClipboardState::new();
        state.set_file(0xAAAA);
        state.set_oversize_file(0xBBBB);
        assert_eq!(state.get().file, Some(0xAAAA));
        assert_eq!(state.get().oversize_file, Some(0xBBBB));

        state.reset();

        let s = state.get();
        assert!(s.file.is_none(), "reset must clear file slot");
        assert!(s.oversize_file.is_none(), "reset must clear oversize_file slot");
    }

    #[test]
    fn set_file_clears_matching_oversize_stamp() {
        // Mirror of set_image behaviour: once an oversize-rejected file is
        // successfully delivered (e.g. user re-copied a smaller version that
        // hashes to the same content — improbable but possible) the oversize
        // stamp must be cleared so subsequent reads see it as "ok".
        let state = ClipboardState::new();
        let h = 0xC0DE_u64;
        state.set_oversize_file(h);
        assert_eq!(state.get().oversize_file, Some(h));

        state.set_file(h);

        let s = state.get();
        assert_eq!(s.file, Some(h));
        assert!(s.oversize_file.is_none(), "set_file must clear matching oversize_file");
    }

    #[test]
    fn lastseen_rapid_text_image_file_text_no_slot_aliasing() {
        // sequence: set_text(A) → set_image(B) → set_file(C) → set_text(D).
        // All four slots must remain independent — no aliasing where one
        // setter quietly clears another's hash.
        let state = ClipboardState::new();
        state.set_text(0xAAAA);
        state.set_image(0xBBBB);
        state.set_file(0xCCCC);
        state.set_text(0xDDDD);

        let s = state.get();
        assert!(s.matches_text_hash(0xDDDD), "latest text in history");
        assert!(s.matches_text_hash(0xAAAA), "older text still in history (LRU)");
        assert_eq!(s.image, Some(0xBBBB), "image survives all text+file writes");
        assert_eq!(s.file, Some(0xCCCC), "file survives subsequent text write");
        assert!(s.matches_file_hash(0xCCCC));
        assert!(!s.matches_file_hash(0xAAAA), "text hash must not match file slot");
    }

    #[test]
    fn lastkind_file_oversize_distinct_test_only() {
        // Test-only LastKind enum: File and OversizeFile map onto distinct
        // LastSeen slots. Smoke test that the legacy `set()` helper threads
        // both variants through correctly.
        let state = ClipboardState::new();
        state.set(LastKind::File(0x1111));
        state.set(LastKind::OversizeFile(0x2222));
        let s = state.get();
        assert_eq!(s.file, Some(0x1111));
        assert_eq!(s.oversize_file, Some(0x2222));
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

    // ------------------------------------------------------------
    // Task 6b: outbound file sync — pure helpers + dedup behaviour
    // ------------------------------------------------------------

    #[test]
    fn check_file_size_within_limit() {
        assert_eq!(check_file_size(100, 1024), Ok(()));
        assert_eq!(check_file_size(1024, 1024), Ok(()), "boundary is inclusive");
    }

    #[test]
    fn check_file_size_over_limit_reports_bytes() {
        let err = check_file_size(2048, 1024).expect_err("expected oversize");
        assert_eq!(err.size_bytes, 2048);
    }

    #[test]
    fn mac_format_oversize_file_toast_includes_kb_and_hint() {
        // Toast must report KB precision near the cap + actionable hint so
        // the user knows what to do next. Mirrors host-side
        // `host_format_oversize_file_toast_includes_kb_and_limit`; the
        // wording diverges by design (chrome-panel toast vs Win11 tray
        // balloon) — see `format_oversize_file_toast` doc-comment.
        let e = FileTooLarge { size_bytes: 25_000 * 1024 };
        let msg = format_oversize_file_toast(&e, MAX_FILE_BYTES);
        assert!(msg.contains("25000"), "KB count missing: {msg}");
        assert!(msg.contains("smaller"), "actionable hint missing: {msg}");
        assert!(msg.contains("too large"), "leading prefix missing: {msg}");
    }

    #[test]
    fn pack_file_or_warn_ready_for_normal_file() {
        // Drive the helper with a tempfile containing 4 KB of synthetic
        // content. Expect Ready { name, hash, packed } with correct shape.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content: Vec<u8> = (0..4096).map(|i| (i & 0xFF) as u8).collect();
        tmp.write_all(&content).expect("write");
        let path = tmp.path().to_owned();

        let outcome = pack_file_or_warn(&path, MAX_FILE_BYTES);
        match outcome {
            FilePollOutcome::Ready { name, hash, packed } => {
                assert_eq!(name, path.file_name().unwrap().to_string_lossy());
                assert_eq!(hash, hash_bytes(&content), "hash must be over file content");
                // Packed layout: [u16 LE name_len][name][content].
                assert!(packed.len() > content.len(), "packed must include header");
                // First two bytes = name byte-length (LE).
                let name_len = u16::from_le_bytes([packed[0], packed[1]]) as usize;
                assert_eq!(name_len, name.len());
                // Content tail must equal original bytes.
                let tail = &packed[2 + name_len..];
                assert_eq!(tail, content.as_slice(), "content must round-trip");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn pack_file_or_warn_oversize_emits_path_hash_and_err() {
        // Synthetic limit much smaller than file → Oversize branch fires
        // without reading the content (helper short-circuits on stat).
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content = vec![0xAB_u8; 2048];
        tmp.write_all(&content).expect("write");
        let path = tmp.path().to_owned();

        let outcome = pack_file_or_warn(&path, 512);
        match outcome {
            FilePollOutcome::Oversize { path_hash, err } => {
                assert_eq!(err.size_bytes, 2048);
                assert_eq!(
                    path_hash,
                    hash_bytes(path.to_string_lossy().as_bytes()),
                    "path hash must be over the path string"
                );
            }
            other => panic!("expected Oversize, got {other:?}"),
        }
    }

    #[test]
    fn pack_file_or_warn_missing_file_skipped() {
        // Non-existent path → Skipped("stat failed").
        let path = std::path::PathBuf::from("/nonexistent/wiredesk-test-FILE-DNE-XYZ.bin");
        match pack_file_or_warn(&path, MAX_FILE_BYTES) {
            FilePollOutcome::Skipped(_) => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn mac_outbound_dedup_skips_same_file_hash() {
        // Stamp LastSeen.file with the hash of a known content blob, then
        // verify the helper Ready path would short-circuit via
        // matches_file_hash. The poll thread checks matches_file_hash before
        // calling emit_offer_and_chunks — this test mirrors that gate.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content = vec![0x42_u8; 1024];
        tmp.write_all(&content).expect("write");
        let path = tmp.path().to_owned();

        let state = ClipboardState::new();
        let outcome = pack_file_or_warn(&path, MAX_FILE_BYTES);
        let hash = match outcome {
            FilePollOutcome::Ready { hash, .. } => hash,
            other => panic!("expected Ready, got {other:?}"),
        };

        // Simulate the prior tick having stamped this hash.
        state.set_file(hash);
        assert!(
            state.get().matches_file_hash(hash),
            "stamped hash must short-circuit the next tick"
        );

        // A different file content → different hash → must NOT skip.
        let mut tmp2 = tempfile::NamedTempFile::new().expect("tempfile2");
        let content2 = vec![0x99_u8; 1024];
        tmp2.write_all(&content2).expect("write");
        let outcome2 = pack_file_or_warn(tmp2.path(), MAX_FILE_BYTES);
        let hash2 = match outcome2 {
            FilePollOutcome::Ready { hash, .. } => hash,
            other => panic!("expected Ready, got {other:?}"),
        };
        assert_ne!(hash, hash2);
        assert!(
            !state.get().matches_file_hash(hash2),
            "different content hash must NOT dedup"
        );
    }

    #[test]
    fn mac_outbound_emits_offer_and_chunks_for_file() {
        // End-to-end: synthesize 4 KB content + name → pack via helper →
        // run through emit_offer_and_chunks → drain queue → assert offer
        // shape (format=FORMAT_FILE, total_len=packed_len) + chunks reassemble.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content: Vec<u8> = (0..(CHUNK_SIZE * 4 + 17)).map(|i| (i & 0xFF) as u8).collect();
        tmp.write_all(&content).expect("write");
        let path = tmp.path().to_owned();

        let outcome = pack_file_or_warn(&path, MAX_FILE_BYTES);
        let packed = match outcome {
            FilePollOutcome::Ready { packed, .. } => packed,
            other => panic!("expected Ready, got {other:?}"),
        };

        let (tx, rx) = mpsc::channel::<Packet>();
        emit_offer_and_chunks(&tx, FORMAT_FILE, &packed);
        drop(tx);

        let mut packets = Vec::new();
        while let Ok(p) = rx.recv() {
            packets.push(p);
        }
        assert!(packets.len() > 2, "must emit offer + ≥2 chunks for 4 KB+");

        // Offer assertion.
        match &packets[0].message {
            Message::ClipOffer { format, total_len } => {
                assert_eq!(*format, FORMAT_FILE);
                assert_eq!(*total_len as usize, packed.len());
            }
            other => panic!("expected ClipOffer first, got {other:?}"),
        }

        // Chunks reassemble byte-for-byte → packed payload (header + content).
        let mut reassembled = Vec::new();
        for (i, p) in packets[1..].iter().enumerate() {
            match &p.message {
                Message::ClipChunk { index, data } => {
                    assert_eq!(*index as usize, i, "chunks must be sequential");
                    reassembled.extend_from_slice(data);
                }
                other => panic!("expected ClipChunk at idx {i}, got {other:?}"),
            }
        }
        assert_eq!(reassembled, packed);
    }

    #[test]
    fn mac_outbound_oversize_emits_toast_only() {
        // Drive the helper with a tiny limit and a small file → Oversize.
        // Verify that the calling code shape (mirroring spawn_poll_thread)
        // emits a TransportEvent::Toast and stamps oversize_file BUT does
        // NOT emit ClipOffer/ClipChunk packets.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content = vec![0u8; 2048];
        tmp.write_all(&content).expect("write");
        let path = tmp.path().to_owned();

        let state = ClipboardState::new();
        let (packets_tx, packets_rx) = mpsc::channel::<Packet>();
        let (events_tx, events_rx) = mpsc::channel::<TransportEvent>();

        // Reproduce the production branch shape.
        match pack_file_or_warn(&path, 256) {
            FilePollOutcome::Oversize { path_hash, err } => {
                events_tx
                    .send(TransportEvent::Toast(format_oversize_file_toast(&err, 256)))
                    .expect("toast send");
                state.set_oversize_file(path_hash);
            }
            other => panic!("expected Oversize, got {other:?}"),
        }
        drop(packets_tx);
        drop(events_tx);

        // No packets emitted.
        assert!(packets_rx.recv().is_err(), "no offer/chunk packets for oversize");

        // Toast emitted with expected wording.
        let evt = events_rx.recv().expect("toast event");
        match evt {
            TransportEvent::Toast(msg) => {
                assert!(msg.contains("too large"), "toast missing prefix: {msg}");
                assert!(msg.contains("smaller"), "toast missing hint: {msg}");
            }
            _ => panic!("expected Toast"),
        }

        // Oversize slot stamped.
        let path_hash = hash_bytes(path.to_string_lossy().as_bytes());
        assert!(
            state.get().matches_file_hash(path_hash),
            "oversize_file slot must be stamped"
        );
    }

    #[test]
    fn mac_outbound_oversize_path_hash_cached() {
        // After stamping oversize_file for a given path, the next poll tick
        // with the same path → matches_file_hash short-circuits BEFORE
        // re-emitting the toast (mirrors the spawn_poll_thread guard).
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(&vec![0u8; 4096]).expect("write");
        let path = tmp.path().to_owned();

        let state = ClipboardState::new();

        // First tick: oversize → stamp.
        let path_hash = hash_bytes(path.to_string_lossy().as_bytes());
        state.set_oversize_file(path_hash);

        // Second tick logic: matches_file_hash must be true → branch
        // short-circuits BEFORE running pack_file_or_warn / toast.
        assert!(
            state.get().matches_file_hash(path_hash),
            "repeated oversize path must hit dedup branch"
        );
    }

    // ------------------------------------------------------------
    // Task 7a: receive_files flag + ClipDecline path for FORMAT_FILE
    // ------------------------------------------------------------

    /// Build an IncomingClipboard with `receive_files` pre-set so we can
    /// drive the policy branch from tests without going through main.rs.
    fn incoming_with_receive_files(state: ClipboardState, on: bool) -> IncomingClipboard {
        let mut inc = IncomingClipboard::new_for_test(state);
        inc.receive_files = Arc::new(AtomicBool::new(on));
        inc
    }

    #[test]
    fn mac_incoming_file_declined_when_flag_off() {
        // receive_files=false → on_offer(FORMAT_FILE) must emit ClipDecline
        // AND leave reassembly state un-armed so subsequent chunks for the
        // declined offer hit the expected_len==0 drop guard.
        let state = ClipboardState::new();
        let mut inc = incoming_with_receive_files(state, false);

        let reply = inc.on_offer(FORMAT_FILE, 4096);
        match reply {
            Some(Message::ClipDecline { format }) => assert_eq!(format, FORMAT_FILE),
            other => panic!("expected ClipDecline {{ FORMAT_FILE }}, got {other:?}"),
        }
        // No reassembly armed.
        assert_eq!(inc.expected_len, 0);
        assert_eq!(inc.expected_format, 0);
        // Counters cleared.
        assert_eq!(inc.incoming_total.load(Ordering::Relaxed), 0);
        assert_eq!(inc.incoming_progress.load(Ordering::Relaxed), 0);

        // Follow-up chunks must be dropped by the expected_len==0 guard.
        for i in 0..8u16 {
            inc.on_chunk(i, vec![0u8; 128]);
        }
        assert!(inc.received.is_empty(), "post-decline chunks must not buffer");
        assert_eq!(inc.received_total, 0);
    }

    #[test]
    fn mac_incoming_file_accepted_when_flag_on() {
        // receive_files=true → on_offer(FORMAT_FILE) must NOT decline; it
        // arms reassembly state ready for chunks. Task 7b adds the actual
        // commit path; this test only covers the policy gate.
        let state = ClipboardState::new();
        let mut inc = incoming_with_receive_files(state, true);

        let reply = inc.on_offer(FORMAT_FILE, 4096);
        assert!(reply.is_none(), "accepted offer must not return ClipDecline");
        assert_eq!(inc.expected_len, 4096);
        assert_eq!(inc.expected_format, FORMAT_FILE);
        assert_eq!(inc.incoming_total.load(Ordering::Relaxed), 4096);
        assert_eq!(inc.incoming_progress.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn mac_incoming_file_oversize_offer_dropped_no_decline() {
        // Even with receive_files=true, an offer past MAX_FILE_BYTES + name
        // headroom is dropped silently (no ClipDecline — that's reserved for
        // policy refusals). Reassembly stays un-armed.
        let state = ClipboardState::new();
        let mut inc = incoming_with_receive_files(state, true);

        let huge = (MAX_FILE_PAYLOAD_BYTES + 1) as u32;
        let reply = inc.on_offer(FORMAT_FILE, huge);
        assert!(reply.is_none(), "oversize dropped without ClipDecline");
        assert_eq!(inc.expected_len, 0);
        assert_eq!(inc.expected_format, 0);
    }

    // ------------------------------------------------------------
    // Task 7b: inbound file commit (unpack + sanitize + write)
    // ------------------------------------------------------------

    use wiredesk_protocol::clip_file::pack_first_chunk;

    /// Build an `IncomingClipboard` with `receive_files=true` and a tempdir
    /// override for the cache. Returns (incoming, tempdir) — caller keeps
    /// the tempdir alive for the duration of the test (drop = cleanup).
    fn incoming_with_tempdir() -> (IncomingClipboard, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut inc = IncomingClipboard::new_for_test(ClipboardState::new());
        inc.set_cache_dir_override(dir.path().to_path_buf());
        (inc, dir)
    }

    #[test]
    fn mac_incoming_file_commits_to_cache() {
        // Feed a ClipOffer + chunks for a small file, then verify the file
        // landed in the cache override directory with byte-equal content
        // and the LastSeen.file slot got stamped (brief T5).
        let (mut inc, dir) = incoming_with_tempdir();

        let name = "contract.pdf";
        let content: Vec<u8> = (0..2048).map(|i| (i & 0xFF) as u8).collect();
        let packed = pack_first_chunk(name, &content).expect("pack");

        feed_offer(&mut inc, FORMAT_FILE, &packed);

        let expected = dir.path().join(name);
        assert!(expected.exists(), "file must land in cache dir");
        let on_disk = std::fs::read(&expected).expect("read written file");
        assert_eq!(on_disk, content, "content must round-trip byte-for-byte");

        // LastSeen.file must hold the content hash (loop avoidance — next
        // tick reading the same content from clipboard must short-circuit).
        let s = inc.state.get();
        assert_eq!(s.file, Some(hash_bytes(&content)));

        // CommittedPayload mirror for in-test introspection.
        match inc.last_committed.as_ref().expect("committed") {
            CommittedPayload::File { path, name: n, content: c } => {
                assert_eq!(path, &expected);
                assert_eq!(n, name);
                assert_eq!(c, &content);
            }
            other => panic!("expected File, got {other:?}"),
        }

        // After successful delivery the in-flight stamp is cleared — a
        // subsequent reset() must NOT remove the file the user now owns.
        assert!(
            inc.in_flight_file_path.is_none(),
            "successful commit must clear in_flight_file_path"
        );
        inc.reset();
        assert!(expected.exists(), "delivered file must survive reset()");
    }

    #[test]
    fn mac_incoming_file_sanitizes_traversal() {
        // Path-traversal name "../evil.sh" — sanitize_basename strips path
        // components and "..", leaving "evil.sh". The file must land inside
        // the cache dir, never one directory up (brief T4 + AC6).
        let (mut inc, dir) = incoming_with_tempdir();

        let content = b"#!/bin/sh\necho pwned\n".to_vec();
        let packed = pack_first_chunk("../evil.sh", &content).expect("pack");

        feed_offer(&mut inc, FORMAT_FILE, &packed);

        let safe = dir.path().join("evil.sh");
        assert!(safe.exists(), "sanitized file must land inside cache dir");

        // No "../evil.sh" outside the cache dir.
        let parent_evil = dir.path().parent().unwrap().join("evil.sh");
        assert!(
            !parent_evil.exists(),
            "traversal must not write outside cache: {}",
            parent_evil.display()
        );

        let on_disk = std::fs::read(&safe).expect("read");
        assert_eq!(on_disk, content);
    }

    #[test]
    fn mac_incoming_file_unicode_filename() {
        // "привет 🎉.pdf" must round-trip as the actual on-disk basename
        // (brief T3 + AC5). Modern macOS filesystems are UTF-8 native so
        // the basename is byte-equal to the input.
        let (mut inc, dir) = incoming_with_tempdir();

        let name = "привет 🎉.pdf";
        let content = vec![0xAB_u8; 1024];
        let packed = pack_first_chunk(name, &content).expect("pack");

        feed_offer(&mut inc, FORMAT_FILE, &packed);

        let expected = dir.path().join(name);
        assert!(
            expected.exists(),
            "unicode filename must round-trip: {}",
            expected.display()
        );
        assert_eq!(std::fs::read(&expected).expect("read"), content);
    }

    #[test]
    fn mac_incoming_file_oversize_declined() {
        // total_len > MAX_FILE_PAYLOAD_BYTES → silent drop in on_offer (no
        // ClipDecline — reserved for policy refusals). State stays un-armed,
        // no file is written (AC4).
        let (mut inc, dir) = incoming_with_tempdir();

        let huge = (MAX_FILE_PAYLOAD_BYTES + 1) as u32;
        let reply = inc.on_offer(FORMAT_FILE, huge);
        assert!(reply.is_none(), "silent drop, no ClipDecline");
        assert_eq!(inc.expected_len, 0);
        assert_eq!(inc.expected_format, 0);

        // Even if a peer keeps sending chunks past the reject, they must be
        // dropped by the expected_len==0 guard and no file is created.
        inc.on_chunk(0, vec![0u8; 256]);
        let dir_empty = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .next()
            .is_none();
        assert!(dir_empty, "cache dir must remain empty after oversize reject");
        assert!(inc.last_committed.is_none());
    }

    #[test]
    fn mac_incoming_partial_file_cleaned_on_reset() {
        // Simulate the mid-write abort scenario: commit_file stamps
        // `in_flight_file_path` BEFORE writing; if reset() fires before
        // the slot is cleared (e.g. write panic, or a manual abort
        // injection), the partial file is removed.
        let (mut inc, dir) = incoming_with_tempdir();
        let partial = dir.path().join("partial.bin");
        // Materialize a "partial" file the way commit_file would mid-write:
        // some bytes are on disk but the commit hasn't completed yet.
        std::fs::write(&partial, b"partial").expect("write partial");
        inc.in_flight_file_path = Some(partial.clone());
        assert!(partial.exists(), "fixture: partial file must exist");

        inc.reset();

        assert!(
            !partial.exists(),
            "reset must remove the in-flight partial file: {}",
            partial.display()
        );
        assert!(
            inc.in_flight_file_path.is_none(),
            "reset must clear the in-flight path slot"
        );
    }

    #[test]
    fn mac_incoming_partial_file_missing_no_panic_on_reset() {
        // Defensive: if reset() runs after the partial file has already been
        // deleted (e.g. by an external process or a vacuum tick that beat us
        // to it), the cleanup branch must swallow `NotFound` without logging
        // an error.
        let (mut inc, dir) = incoming_with_tempdir();
        let ghost = dir.path().join("ghost.bin");
        // Stamp but don't actually create the file.
        inc.in_flight_file_path = Some(ghost);

        inc.reset(); // must not panic

        assert!(inc.in_flight_file_path.is_none());
    }

    #[test]
    fn text_and_image_commit_still_work() {
        // Regression: after extending commit() with the FORMAT_FILE branch
        // and changing the reset() lifecycle, the existing text and image
        // paths must still commit normally. AC3 in the brief.
        let (mut inc, _dir) = incoming_with_tempdir();

        // Text path.
        feed_offer(&mut inc, FORMAT_TEXT_UTF8, b"hello, world");
        match inc.last_committed.as_ref().expect("text committed") {
            CommittedPayload::Text(s) => assert_eq!(s, "hello, world"),
            other => panic!("expected Text, got {other:?}"),
        }

        // Image path. Use a synthetic 4×4 RGBA → encoded PNG.
        let original = synthetic_rgba_4x4();
        let png = encode_rgba_to_png(&original).expect("encode");
        feed_offer(&mut inc, FORMAT_PNG_IMAGE, &png);
        match inc.last_committed.as_ref().expect("image committed") {
            CommittedPayload::Image { width, height, bytes } => {
                assert_eq!(*width, original.width);
                assert_eq!(*height, original.height);
                assert_eq!(bytes.as_slice(), &*original.bytes);
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn mac_incoming_file_unpack_failure_leaves_state_clean() {
        // Bogus first-chunk payload (declared name_len > actual buffer)
        // makes unpack_first_chunk return Truncated. commit() must log and
        // skip without panicking or stamping anything. Subsequent offers
        // must proceed normally.
        let (mut inc, dir) = incoming_with_tempdir();

        // Fabricate a payload with name_len=99 but only 4 bytes total.
        let bogus = vec![99, 0, b'A', b'B'];
        feed_offer(&mut inc, FORMAT_FILE, &bogus);

        assert!(inc.last_committed.is_none(), "no commit on unpack failure");
        let dir_empty = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .next()
            .is_none();
        assert!(dir_empty, "cache dir stays empty on unpack failure");

        // Receiver must be ready for a fresh offer.
        assert_eq!(inc.expected_len, 0);
        assert_eq!(inc.expected_format, 0);
        assert!(inc.state.get().file.is_none());

        // Follow-up valid file commit succeeds — no lingering state.
        let content = b"valid".to_vec();
        let packed = pack_first_chunk("good.txt", &content).expect("pack");
        feed_offer(&mut inc, FORMAT_FILE, &packed);
        assert!(dir.path().join("good.txt").exists(), "next file must commit");
    }

    #[test]
    fn mac_incoming_file_reserved_ntfs_name_prefixed() {
        // CON.txt → _CON.txt on disk (sanitize_basename rule). Same content
        // hash applies; the OS-pasteboard URL points at the prefixed name.
        let (mut inc, dir) = incoming_with_tempdir();
        let content = b"CON device test".to_vec();
        let packed = pack_first_chunk("CON.txt", &content).expect("pack");

        feed_offer(&mut inc, FORMAT_FILE, &packed);

        assert!(
            dir.path().join("_CON.txt").exists(),
            "NTFS reserved stem must be prefixed: _CON.txt"
        );
        assert!(
            !dir.path().join("CON.txt").exists(),
            "raw reserved name must not appear"
        );
    }

    #[test]
    fn mac_incoming_file_empty_name_falls_back_to_clipboard_bin() {
        // Empty raw name (or one stripped to "" by sanitize) → fallback
        // "clipboard.bin". Forms an end-to-end test of the
        // FALLBACK_BASENAME path inside commit_file.
        let (mut inc, dir) = incoming_with_tempdir();
        // Use ".." as the name — sanitize_basename collapses to fallback.
        let content = b"data".to_vec();
        let packed = pack_first_chunk("..", &content).expect("pack");

        feed_offer(&mut inc, FORMAT_FILE, &packed);

        assert!(
            dir.path().join("clipboard.bin").exists(),
            "fallback basename must materialize"
        );
    }

    // ---- Task 7d: progress label + decline toast for FORMAT_FILE -------

    #[test]
    fn apply_outgoing_progress_handles_file_format() {
        // Writer-thread dispatch: a ClipOffer with FORMAT_FILE must set
        // total/progress just like text/image, AND the label-aware variant
        // must leave the filename slot alone on Offer (the poll thread set
        // it BEFORE the offer hit the wire). The slot is cleared only on
        // DONE — proven in `apply_outgoing_progress_file_clears_label_on_done`.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let label = Arc::new(Mutex::new("contract.pdf".to_string()));

        let msg = Message::ClipOffer { format: FORMAT_FILE, total_len: 4096 };
        apply_outgoing_progress_with_label(&msg, &progress, &total, &label);

        assert_eq!(total.load(Ordering::Relaxed), 4096);
        assert_eq!(progress.load(Ordering::Relaxed), 0);
        assert_eq!(
            &*label.lock().unwrap(),
            "contract.pdf",
            "label preserved through Offer — UI needs it for the whole transfer"
        );
        // format_label sanity — the log line in apply_outgoing_progress_inner
        // funnels through this helper, so a direct assertion proves the wire-
        // log says "FILE" rather than "2".
        assert_eq!(format_label(FORMAT_FILE), "FILE");
        assert_eq!(format_label(FORMAT_TEXT_UTF8), "TEXT");
        assert_eq!(format_label(FORMAT_PNG_IMAGE), "IMAGE");
    }

    #[test]
    fn apply_outgoing_progress_file_clears_label_on_done() {
        // Status-line label must drop back to empty once the last chunk
        // lands. Without this the next text Cmd+C would still show the
        // previous file's name until DONE for THAT transfer cleared it.
        let progress = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let label = Arc::new(Mutex::new("report.pdf".to_string()));

        let offer = Message::ClipOffer { format: FORMAT_FILE, total_len: 8 };
        apply_outgoing_progress_with_label(&offer, &progress, &total, &label);
        let chunk = Message::ClipChunk { index: 0, data: vec![0u8; 8] };
        apply_outgoing_progress_with_label(&chunk, &progress, &total, &label);

        assert_eq!(total.load(Ordering::Relaxed), 0, "DONE zeroes total");
        assert_eq!(
            &*label.lock().unwrap(),
            "",
            "DONE clears the filename slot"
        );
    }

    #[test]
    fn send_decline_toast_file_format_is_specific() {
        // Verifies the send-side ClipDecline → toast string is FORMAT_FILE-
        // aware. The brief asks for an explicit "Peer declined file (Receive
        // files off)" wording; text/image keep the legacy generic message.
        let file_toast = send_decline_toast(FORMAT_FILE);
        assert!(
            file_toast.contains("declined"),
            "toast must say declined: {file_toast}"
        );
        assert!(
            file_toast.contains("file"),
            "FORMAT_FILE toast must mention 'file': {file_toast}"
        );

        let text_toast = send_decline_toast(FORMAT_TEXT_UTF8);
        assert!(
            !text_toast.contains("file"),
            "text decline must not mention 'file': {text_toast}"
        );
        let image_toast = send_decline_toast(FORMAT_PNG_IMAGE);
        assert!(
            !image_toast.contains("file"),
            "image decline must not mention 'file': {image_toast}"
        );
    }

    #[test]
    fn clip_decline_file_drops_pending_outbox() {
        // Task 7d contract: ClipDecline { FORMAT_FILE } arriving in the
        // reader thread must arm `outgoing_cancel` — that's the signal
        // writer_thread reads to drain any queued ClipOffer/ClipChunk
        // packets out of the outbox instead of pumping them onto the wire.
        // Without the flag flip, a 20 MB declined file would saturate the
        // serial link for ~70s before the writer noticed.
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(!cancel.load(Ordering::Acquire), "precondition: cancel disarmed");

        let _toast = apply_clip_decline(FORMAT_FILE, &cancel);

        assert!(
            cancel.load(Ordering::Acquire),
            "FORMAT_FILE decline must arm outgoing_cancel — writer relies on it to drop queued packets"
        );
    }

    #[test]
    fn clip_decline_file_emits_toast() {
        // Pair to `clip_decline_file_drops_pending_outbox`: the helper
        // also produces the user-facing toast string. Asserts the exact
        // wording the brief calls for so a future refactor doesn't quietly
        // weaken it into a generic message.
        let cancel = Arc::new(AtomicBool::new(false));
        let toast = apply_clip_decline(FORMAT_FILE, &cancel);
        assert_eq!(toast, "Peer declined file (Receive files off)");

        // Non-file formats keep the legacy generic copy — regression guard.
        let cancel2 = Arc::new(AtomicBool::new(false));
        let toast_text = apply_clip_decline(FORMAT_TEXT_UTF8, &cancel2);
        assert_eq!(toast_text, "Host declined the clipboard transfer");
    }

    #[test]
    fn clip_decline_with_label_clears_filename_slot() {
        // Quality-review fix: when the peer declines a FORMAT_FILE offer
        // mid-flight, the writer thread drains queued ClipOffer/ClipChunk
        // packets WITHOUT ever running apply_outgoing_progress_with_label
        // (which is normally responsible for clearing the label on DONE).
        // So the reader-thread decline handler must clear the slot itself
        // — otherwise the status bar sticks at "Sending file 'X.pdf'"
        // until the next outgoing transfer or disconnect.
        let cancel = Arc::new(AtomicBool::new(false));
        let label = Arc::new(Mutex::new(String::from("contract.pdf")));

        let toast = apply_clip_decline_with_label(FORMAT_FILE, &cancel, &label);

        assert_eq!(toast, "Peer declined file (Receive files off)");
        assert!(cancel.load(Ordering::Acquire), "cancel must be armed");
        assert!(
            label.lock().unwrap().is_empty(),
            "filename slot must be cleared on decline so the status bar stops showing 'Sending file ...'"
        );
    }

    #[test]
    fn clip_decline_with_label_clears_slot_even_for_text_format() {
        // Defensive: clearing the label is unconditional. If a future caller
        // accidentally puts a label there for text/image transfers, the
        // decline path still cleans up — easier to debug than a sticky UI.
        let cancel = Arc::new(AtomicBool::new(false));
        let label = Arc::new(Mutex::new(String::from("stale.txt")));

        let _ = apply_clip_decline_with_label(FORMAT_TEXT_UTF8, &cancel, &label);

        assert!(label.lock().unwrap().is_empty(), "slot cleared unconditionally");
    }

    // --- Task 9a: cache vacuum startup hookup -------------------------------

    #[test]
    fn mac_default_cache_dir_ends_in_wiredesk() {
        // Smoke check: the resolution chain (dirs::cache_dir() →
        // env::temp_dir()) always yields *something* on every supported
        // target. We don't care which branch wins — only that the call
        // never returns an empty path that would later fail
        // `create_dir_all`, and that the final segment is "WireDesk" so
        // the path can be told apart from a foreign cache root.
        let p = default_cache_dir();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("WireDesk"));
    }

    #[test]
    fn mac_run_startup_vacuum_handles_missing_dir() {
        // The underlying core helper must return Ok(0) on a missing
        // directory — that's the contract `run_startup_vacuum` relies on
        // for first-run boots when ~/Library/Caches/WireDesk doesn't
        // exist yet. Test against an explicit missing path to pin the
        // behaviour; then call the production helper to assert it
        // doesn't panic against whatever the live resolver yields.
        let missing = std::env::temp_dir()
            .join("wd-mac-cache-vacuum-doesnotexist-9a-startup");
        let _ = std::fs::remove_dir_all(&missing);
        let res = wiredesk_core::cache_vacuum::vacuum_cache_dir(
            &missing,
            Duration::from_secs(24 * 3600),
        );
        assert!(res.is_ok());
        assert_eq!(res.unwrap(), 0);

        // And the helper itself must not panic when called against
        // whatever the production resolver yields — even if the path
        // doesn't exist yet.
        run_startup_vacuum(Duration::from_secs(24 * 3600));
    }

    #[test]
    fn mac_run_startup_vacuum_removes_old_files_via_core() {
        // Drive the underlying core helper directly against a tempdir
        // (the standalone `run_startup_vacuum` resolves
        // dirs::cache_dir() — we can't divert it without an env mutation
        // that would race other tests). Asserts the end-to-end contract
        // production relies on: old files get removed, fresh files
        // survive.
        use filetime::{FileTime, set_file_mtime};
        use std::fs;

        let dir = tempfile::tempdir().expect("tempdir");
        let old_path = dir.path().join("old.bin");
        let fresh_path = dir.path().join("fresh.bin");
        fs::write(&old_path, b"old").expect("write old");
        fs::write(&fresh_path, b"fresh").expect("write fresh");

        let old_ft = FileTime::from_system_time(
            std::time::SystemTime::now() - Duration::from_secs(30 * 3600),
        );
        set_file_mtime(&old_path, old_ft).expect("set old mtime");

        let removed = wiredesk_core::cache_vacuum::vacuum_cache_dir(
            dir.path(),
            Duration::from_secs(24 * 3600),
        )
        .expect("vacuum");

        assert_eq!(removed, 1, "only the >24h file should be removed");
        assert!(!old_path.exists(), "old file should be gone");
        assert!(fresh_path.exists(), "fresh file should survive");
    }

    // ------------------------------------------------------------
    // Task 9b: pre-stamp helper for file URLs at process startup
    // ------------------------------------------------------------

    #[test]
    fn pre_stamp_file_path_stamps_normal_file() {
        // Tempfile with 4 KB of synthetic content → FilePreStampOutcome::Stamped
        // with name = file_name() and hash = hash_bytes(content). Drives the
        // happy path the poll-thread startup block hits when the OS clipboard
        // already carries a file URL at launch.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content: Vec<u8> = (0..4096).map(|i| (i & 0xFF) as u8).collect();
        tmp.write_all(&content).expect("write");
        let path = tmp.path().to_owned();

        let outcome = pre_stamp_file_path(&path, MAX_FILE_BYTES);
        match outcome {
            FilePreStampOutcome::Stamped { name, hash } => {
                assert_eq!(name, path.file_name().unwrap().to_string_lossy());
                assert_eq!(hash, hash_bytes(&content));
            }
            other => panic!("expected Stamped, got {other:?}"),
        }
    }

    #[test]
    fn pre_stamp_file_path_skips_oversize() {
        // Tempfile with 2 KB of content but limit of 1 KB → Oversize. Critical
        // contract: stamp helper must NOT read the content (would defeat the
        // size cap) and must NOT stamp the file hash so the runtime poll path
        // is free to surface the toast on the user's next observation.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content: Vec<u8> = vec![0xAA; 2048];
        tmp.write_all(&content).expect("write");
        let path = tmp.path().to_owned();

        let outcome = pre_stamp_file_path(&path, 1024);
        match outcome {
            FilePreStampOutcome::Oversize { size_bytes } => {
                assert_eq!(size_bytes, 2048);
            }
            other => panic!("expected Oversize, got {other:?}"),
        }
    }

    #[test]
    fn pre_stamp_file_path_skipped_for_missing_file() {
        // Non-existent path → Skipped (stat failure). The startup probe must
        // not panic if the pasteboard points at a file that disappeared (the
        // user deleted it between the previous session and the new launch).
        let path = std::path::PathBuf::from("/nonexistent/wiredesk-test-9b-missing.bin");
        let outcome = pre_stamp_file_path(&path, MAX_FILE_BYTES);
        match outcome {
            FilePreStampOutcome::Skipped(_) => {}
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[test]
    fn pre_stamp_then_runtime_poll_dedups() {
        // End-to-end smoke: simulate the startup flow by calling
        // `pre_stamp_file_path` and stamping `LastSeen.file` exactly as the
        // poll thread does. The next "runtime" poll over the same content
        // (using `matches_file_hash`) must dedup → no resend on the first
        // tick after launch. Regression guard for the "stamp_initial handles
        // pre-existing file" plan checkbox.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content = b"pre-existing file content";
        tmp.write_all(content).expect("write");
        let path = tmp.path().to_owned();

        let state = ClipboardState::new();

        // Startup probe.
        match pre_stamp_file_path(&path, MAX_FILE_BYTES) {
            FilePreStampOutcome::Stamped { hash, .. } => {
                state.set_file(hash);
            }
            other => panic!("expected Stamped, got {other:?}"),
        }

        // Runtime poll-tick path: pack helper computes the same content hash.
        match pack_file_or_warn(&path, MAX_FILE_BYTES) {
            FilePollOutcome::Ready { hash, .. } => {
                assert!(
                    state.get().matches_file_hash(hash),
                    "runtime poll must dedup against pre-stamped hash"
                );
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn pre_stamp_oversize_does_not_pollute_file_slot() {
        // Oversize files at startup must NOT stamp `LastSeen.file` — the user
        // hasn't seen any toast yet, so the runtime poll path needs to fire
        // its oversize branch (and surface the toast) on the first tick.
        use std::io::Write;
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let content: Vec<u8> = vec![0xCC; 4096];
        tmp.write_all(&content).expect("write");
        let path = tmp.path().to_owned();

        let state = ClipboardState::new();

        match pre_stamp_file_path(&path, 1024) {
            FilePreStampOutcome::Oversize { .. } => {
                // intentionally NO state.set_file / set_oversize_file here —
                // mirror of the production startup block.
            }
            other => panic!("expected Oversize, got {other:?}"),
        }

        let s = state.get();
        assert!(s.file.is_none(), "file slot must remain empty after oversize pre-stamp");
        assert!(
            s.oversize_file.is_none(),
            "oversize_file slot must also remain empty — runtime poll owns the toast"
        );
    }
}
