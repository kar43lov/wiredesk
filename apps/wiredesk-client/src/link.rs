//! Link supervisor: owns the reader/writer thread pair and re-spawns them
//! after a disconnect, reopening the transport with exponential backoff.
//!
//! The serial channel can die in two ways the Mac side must recover from
//! on its own:
//!   - **frame-error storm** — one FT232H glitches and both directions
//!     corrupt systematically (`WireDeskError::Protocol` flood). The reader
//!     detects this via [`StormCounter`] and signals a reopen.
//!   - **plain disconnect** — host quit, cable unplug, send/recv fatal. The
//!     reader/writer emit `TransportEvent::Disconnected` and exit.
//!
//! Either way the UI thread answers a `Disconnected` event by pushing a
//! `()` into `reconnect_request_rx`; [`spawn_supervisor`] then tears down the
//! old link, drops both transport handles (releasing the serial fd), and
//! reopens with backoff. The `outgoing_tx`/`outgoing_rx` channel survives the
//! whole cycle — clones of `outgoing_tx` held by the clipboard poll / IPC /
//! keyboard-tap threads stay valid across reconnects because the writer
//! *returns* the receiver when it exits and the supervisor hands it to the
//! next writer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use wiredesk_core::error::WireDeskError;
use wiredesk_core::storm::{DEFAULT_STORM_THRESHOLD, StormCounter};
use wiredesk_protocol::message::{Message, VERSION};
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

use crate::app::TransportEvent;
use crate::{clipboard, exec_bridge};

/// Host identity + geometry learned from the `HelloAck` handshake. The
/// interactive-`wd`-over-IPC relay (Task 6/7) needs these to synthesise an
/// accurate `HelloAck` for a term that connects *after* the GUI already
/// handshook — the term never touches the wire, so the GUI answers its
/// `Hello` from this cache instead of forwarding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInfo {
    pub host_name: String,
    pub screen_w: u32,
    pub screen_h: u32,
}

/// Shared host-info cache. `None` until the first `HelloAck`; set back to
/// `None` on every link-down. Populated by the reader thread (sole writer)
/// and read by the IPC acceptor's interactive relay (Task 7). The acceptor
/// thread has no access to `App`, so this `Arc<Mutex<..>>` is how the
/// handshake result crosses into it.
pub type SharedHostInfo = Arc<std::sync::Mutex<Option<HostInfo>>>;

/// All shared state that outlives a reconnect and is needed by the
/// reader/writer threads. Every field is an `Arc`/`Sender`/`String`, so the
/// container is cheaply `Clone` — each fresh link gets its own clone. This
/// also keeps `spawn_link` from tripping `clippy::too_many_arguments`.
#[derive(Clone)]
pub struct LinkContext {
    pub client_name: String,
    pub clipboard_state: clipboard::ClipboardState,
    pub outgoing_progress: Arc<AtomicU64>,
    pub outgoing_total: Arc<AtomicU64>,
    pub incoming_progress: Arc<AtomicU64>,
    pub incoming_total: Arc<AtomicU64>,
    pub receive_images: Arc<AtomicBool>,
    pub receive_text: Arc<AtomicBool>,
    pub receive_files: Arc<AtomicBool>,
    pub incoming_cancel: Arc<AtomicBool>,
    pub outgoing_cancel: Arc<AtomicBool>,
    pub exec_slot: exec_bridge::ExecEventSlot,
    pub current_outgoing_label: Arc<std::sync::Mutex<String>>,
    /// Reader's clone of `outgoing_tx` — used to send `ClipDecline` back to
    /// the host when an incoming offer is rejected.
    pub reader_outgoing_tx: Sender<Packet>,
    /// True only while the handshake is complete AND the reader is alive —
    /// the IPC gate keys off this (Codex iter4 P2: a freshly opened port is
    /// not yet a usable link; flipping this at spawn would let `wd --exec`
    /// queue onto a host that never sent HelloAck and hang to its timeout).
    /// The reader stores `true` on HelloAck and `false` on every exit path;
    /// the supervisor stores `false` at teardown (belt-and-braces).
    pub link_up: Arc<AtomicBool>,
    /// Host identity/geometry from the last `HelloAck` (see [`HostInfo`]).
    /// The reader populates it on HelloAck alongside `link_up=true` and
    /// clears it (`None`) on every exit path; the supervisor also clears it
    /// at teardown. The interactive relay reads it to synth a `HelloAck`.
    pub host_info: SharedHostInfo,
}

/// Join handles for one reader/writer pair. The writer's handle resolves to
/// the `outgoing_rx` it owned so the supervisor can hand it to the next link.
/// `shutdown` is shared by both threads; the supervisor raises it at teardown
/// so neither thread can outlive the link (see [`spawn_supervisor`]).
pub struct LinkHandles {
    pub writer: JoinHandle<Receiver<Packet>>,
    pub reader: JoinHandle<()>,
    pub shutdown: Arc<AtomicBool>,
}

/// Spawn a reader/writer pair over the given transport handles. `shutdown` is
/// shared by both threads so the supervisor can stop either even when its
/// transport never errors on its own: the reader's `recv()` only times out
/// (silent host quit / unplug), and the writer's `send()` keeps returning
/// `Ok` during a frame-error storm (the fd is alive, the chip just corrupts
/// bytes on the wire). Without it `join()` would hang forever.
pub fn spawn_link(
    reader_t: Box<dyn Transport>,
    writer_t: Box<dyn Transport>,
    outgoing_rx: Receiver<Packet>,
    events_tx: Sender<TransportEvent>,
    shutdown: Arc<AtomicBool>,
    ctx: LinkContext,
) -> LinkHandles {
    let writer = {
        let events_tx = events_tx.clone();
        let ctx = ctx.clone();
        let shutdown = shutdown.clone();
        thread::spawn(move || writer_thread(writer_t, outgoing_rx, events_tx, shutdown, ctx))
    };
    let reader = {
        let shutdown = shutdown.clone();
        thread::spawn(move || reader_thread(reader_t, events_tx, shutdown, ctx))
    };
    LinkHandles {
        writer,
        reader,
        shutdown,
    }
}

/// Exponential backoff capped at 30s: attempt 1→1s, 2→2s, 3→4s, 4→8s,
/// 5→16s, ≥6→30s. Pure helper so the schedule is unit-testable; the
/// supervisor takes a `backoff_fn` so tests can inject a near-zero delay.
pub fn backoff_delay(attempt: u32) -> Duration {
    let secs = match attempt {
        0 | 1 => 1,
        2 => 2,
        3 => 4,
        4 => 8,
        5 => 16,
        _ => 30,
    };
    Duration::from_secs(secs)
}

/// Spawn the supervisor thread. It blocks on `reconnect_request_rx`; on each
/// request it tears down the current link (if any), drops the transport
/// handles, and reopens via `open_fn` with `backoff_fn` delays, then spawns a
/// fresh link. `link_up` reflects whether a link is currently spawned.
///
/// The first request — sent by `main` at startup — drives the initial open
/// through the same path, so there's no duplicate open code.
pub fn spawn_supervisor(
    mut open_fn: impl FnMut() -> Result<Box<dyn Transport>, WireDeskError> + Send + 'static,
    mut backoff_fn: impl FnMut(u32) -> Duration + Send + 'static,
    outgoing_rx: Receiver<Packet>,
    events_tx: Sender<TransportEvent>,
    reconnect_request_rx: Receiver<()>,
    ctx: LinkContext,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut outgoing_rx = Some(outgoing_rx);
        let mut handles: Option<LinkHandles> = None;

        loop {
            // Wait for a (re)connect request. All senders dropped → app is
            // shutting down, so stop the supervisor.
            if reconnect_request_rx.recv().is_err() {
                return;
            }

            // Tear down the current link, if one is up. The reader also
            // clears the flag on its own exit paths; this store covers the
            // teardown-of-a-live-link case (storm: reader already gone, but
            // we must gate IPC before joining the still-alive writer).
            ctx.link_up.store(false, Ordering::Release);
            if let Ok(mut hi) = ctx.host_info.lock() {
                *hi = None;
            }
            if let Some(h) = handles.take() {
                // Raise the shared shutdown flag so BOTH threads exit even if
                // their transport never errors on its own — the writer's
                // `send()` keeps returning `Ok` during a frame-error storm, so
                // without this flag `h.writer.join()` would block forever. The
                // writer returns the receiver we need for the next link.
                h.shutdown.store(true, Ordering::Release);
                if let Ok(rx) = h.writer.join() {
                    outgoing_rx = Some(rx);
                }
                let _ = h.reader.join();
            }

            let rx = match outgoing_rx.take() {
                Some(rx) => rx,
                None => {
                    // Writer panicked without returning the receiver — we
                    // can't respawn a link without it. Bail out rather than
                    // spin.
                    log::error!("supervisor: lost outgoing receiver; stopping");
                    return;
                }
            };

            // Reopen with backoff. Loops until the transport opens — the
            // channel is the user's only link, so we keep trying.
            let mut attempt: u32 = 0;
            let reader_t = loop {
                attempt = attempt.saturating_add(1);
                let _ = events_tx.send(TransportEvent::Reconnecting { attempt });
                log::info!("reopening transport attempt={attempt}");
                match open_fn() {
                    Ok(t) => break t,
                    Err(e) => {
                        log::warn!("reopen attempt={attempt} failed: {e}");
                        thread::sleep(backoff_fn(attempt));
                    }
                }
            };

            let writer_t = match reader_t.try_clone() {
                Ok(w) => w,
                Err(e) => {
                    log::error!("try_clone failed after reopen: {e}");
                    let _ =
                        events_tx.send(TransportEvent::Disconnected(format!("try_clone: {e}")));
                    // Keep the receiver for the next request and wait.
                    outgoing_rx = Some(rx);
                    continue;
                }
            };

            // Drain duplicate requests BEFORE spawning the fresh link. Stale
            // duplicates (reader + writer of the dead link both emitting
            // Disconnected) have piled up during teardown + backoff — eat them
            // now. Draining AFTER the spawn instead would swallow a legitimate
            // request from the fresh link itself (e.g. its HELLO send fails
            // right away on a still-broken port) and auto-reconnect would
            // stall on a dead transport (Codex P2). A stale duplicate arriving
            // after this point merely causes one extra teardown/reopen blip —
            // a safe failure mode, unlike a permanently dead link.
            while reconnect_request_rx.try_recv().is_ok() {}

            let shutdown = Arc::new(AtomicBool::new(false));
            handles = Some(spawn_link(
                reader_t,
                writer_t,
                rx,
                events_tx.clone(),
                shutdown,
                ctx.clone(),
            ));
            // NOTE: `link_up` is NOT raised here — an open port is not yet a
            // usable link. The reader flips it true on HelloAck (handshake
            // complete) and back to false on every exit path; see LinkContext.
        }
    })
}

/// Sole writer to the serial port. Any UI-driven packet hits the wire within
/// one channel hop (~µs) — no waiting on a recv timeout.
///
/// Returns `outgoing_rx` on exit so the supervisor can hand the same channel
/// to the next link — the `outgoing_tx` clones held by the poll / IPC /
/// keyboard threads stay valid across reconnects.
///
/// M3 fix: this thread is the SOLE updater of `outgoing_progress` /
/// `outgoing_total`. Counters are bumped via `apply_outgoing_progress_with_label`
/// AFTER each successful `transport.send`, so the UI sees real wire-state
/// progress rather than instant jumps to 100% as packets queue.
fn writer_thread(
    mut transport: Box<dyn Transport>,
    outgoing_rx: Receiver<Packet>,
    events_tx: Sender<TransportEvent>,
    shutdown: Arc<AtomicBool>,
    ctx: LinkContext,
) -> Receiver<Packet> {
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
    let outgoing_cancel = &ctx.outgoing_cancel;

    if let Err(e) = transport.send(&Packet::new(
        Message::Hello {
            version: VERSION,
            client_name: ctx.client_name.clone(),
        },
        0,
    )) {
        log::error!("failed to send HELLO: {e}");
        ctx.link_up.store(false, Ordering::Release);
        let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
        return outgoing_rx;
    }

    let mut last_heartbeat = Instant::now();
    // Per-cancel-batch counter. We log a single INFO at the start of a
    // cancel sweep and another summary INFO when it ends — emitting one
    // line per dropped chunk floods the log.
    let mut cancel_drop_count: u32 = 0;
    let mut cancel_active = false;

    loop {
        // Supervisor raised the teardown flag — return the receiver so the
        // next link can reuse the channel. Checked here because `send()` keeps
        // returning `Ok` during a frame-error storm, so the error-exit paths
        // below never fire and `recv_timeout` would otherwise loop forever.
        if shutdown.load(Ordering::Acquire) {
            return outgoing_rx;
        }

        let timeout = HEARTBEAT_INTERVAL
            .saturating_sub(last_heartbeat.elapsed())
            .max(Duration::from_millis(1));

        match outgoing_rx.recv_timeout(timeout) {
            Ok(packet) => {
                let is_clip = matches!(
                    packet.message,
                    Message::ClipOffer { .. } | Message::ClipChunk { .. }
                );
                let cancelling = outgoing_cancel.load(Ordering::Acquire);
                if is_clip && cancelling {
                    // User pressed Cancel mid-transfer. Drop the queued
                    // clip packet without writing it to the wire so Host
                    // never sees the rest of the offer.
                    if !cancel_active {
                        log::info!("clipboard.send CANCELLED — dropping queued packets");
                        cancel_active = true;
                        cancel_drop_count = 0;
                    }
                    cancel_drop_count = cancel_drop_count.saturating_add(1);
                    continue;
                }
                if cancelling && !is_clip {
                    // Drained the cancelled batch. Re-arm for next transfer.
                    outgoing_cancel.store(false, Ordering::Release);
                    if cancel_active {
                        log::info!(
                            "clipboard.send cancel complete ({cancel_drop_count} packets dropped)"
                        );
                        cancel_active = false;
                        cancel_drop_count = 0;
                    }
                }
                if let Err(e) = transport.send(&packet) {
                    log::error!("send error: {e}");
                    // Close the IPC gate immediately (Codex iter5 P2): the
                    // reader may still be alive, and waiting for the UI →
                    // supervisor teardown to clear the flag leaves a window
                    // where wd --exec queues onto a dead writer's channel.
                    ctx.link_up.store(false, Ordering::Release);
                    let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                    return outgoing_rx;
                }
                // Update progress AFTER send returns — atomic reflects bytes
                // actually written to the UART, not bytes queued in mpsc.
                // The label-aware variant clears `current_outgoing_label`
                // when the transfer reaches DONE (Task 7d).
                clipboard::apply_outgoing_progress_with_label(
                    &packet.message,
                    &ctx.outgoing_progress,
                    &ctx.outgoing_total,
                    &ctx.current_outgoing_label,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Queue empty. If a cancel was pending, the only way to
                // be here is that we've dropped every queued clip packet —
                // safe to clear the flag.
                if outgoing_cancel.load(Ordering::Acquire) {
                    outgoing_cancel.store(false, Ordering::Release);
                }
                if cancel_active {
                    log::info!(
                        "clipboard.send cancel complete ({cancel_drop_count} packets dropped)"
                    );
                    cancel_active = false;
                    cancel_drop_count = 0;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return outgoing_rx,
        }

        if last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            // Treat a failed heartbeat write like any other send error. On an
            // idle link the periodic heartbeat is the ONLY write, so swallowing
            // its error would hide a dead local fd (cable yanked on our side)
            // until the reader happens to notice. Emit Disconnected and hand the
            // receiver back so the supervisor can reopen.
            if let Err(e) = transport.send(&Packet::new(Message::Heartbeat, 0)) {
                log::error!("heartbeat send error: {e}");
                ctx.link_up.store(false, Ordering::Release);
                let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                return outgoing_rx;
            }
            last_heartbeat = Instant::now();
        }
    }
}

/// Receive-side liveness budget while the link is idle. The host emits a
/// heartbeat every 2 s; three missed in a row means the peer is gone — host
/// quit / crash / cable yanked on the *remote* side leaves our local fd open,
/// so `recv()` only ever times out (never a fatal error). Mirrors the host's
/// `HEARTBEAT_TIMEOUT_IDLE` so neither side is more trigger-happy.
const RECV_TIMEOUT_IDLE: Duration = Duration::from_secs(6);
/// Receive-side budget while a transfer is in flight. A large clipboard push
/// (Mac→Host) saturates the wire and the host's heartbeats queue behind our
/// chunks, so the strict idle window would falsely fire. Mirrors the host's
/// `HEARTBEAT_TIMEOUT_BUSY`.
const RECV_TIMEOUT_BUSY: Duration = Duration::from_secs(30);

/// True while a clipboard transfer is in flight (either direction) or a shell
/// session is open — the wire is busy and the peer's heartbeats may be delayed,
/// so the reader picks the looser [`RECV_TIMEOUT_BUSY`] budget.
fn transfer_in_flight(ctx: &LinkContext) -> bool {
    let outgoing =
        ctx.outgoing_total.load(Ordering::Relaxed) > ctx.outgoing_progress.load(Ordering::Relaxed);
    let incoming =
        ctx.incoming_total.load(Ordering::Relaxed) > ctx.incoming_progress.load(Ordering::Relaxed);
    let shell = ctx.exec_slot.lock().map(|g| g.is_some()).unwrap_or(false);
    outgoing || incoming || shell
}

/// Sole reader of the serial port. Translates incoming packets to UI events.
///
/// Storm detection: a run of `DEFAULT_STORM_THRESHOLD` consecutive
/// `Protocol` recv errors means the channel is corrupting systematically —
/// we emit `Disconnected("frame-error storm — reopening port")` and exit so
/// the supervisor reopens the port. Any successfully decoded packet resets
/// the run; recv timeouts touch neither (see [`StormCounter`]).
///
/// `shutdown` is checked every iteration so the supervisor can stop us even
/// on a silent transport (recv only ever timing out).
fn reader_thread(
    transport: Box<dyn Transport>,
    events_tx: Sender<TransportEvent>,
    shutdown: Arc<AtomicBool>,
    ctx: LinkContext,
) {
    let link_up = ctx.link_up.clone();
    let host_info = ctx.host_info.clone();
    reader_loop(
        transport,
        events_tx,
        shutdown,
        ctx,
        RECV_TIMEOUT_IDLE,
        RECV_TIMEOUT_BUSY,
    );
    // Whatever path the loop exited through (storm, fatal, idle budget,
    // host Disconnect, shutdown flag) — the link is no longer usable.
    // Clear the host-info cache in lock-step with link_up so the relay never
    // synths a `HelloAck` for a host we're no longer connected to. Cleared
    // before the final `link_up` store so the lock temporary drops well
    // before `host_info` does (borrow-checker: keeps it off the fn tail).
    if let Ok(mut hi) = host_info.lock() {
        *hi = None;
    }
    link_up.store(false, Ordering::Release);
}

/// Reader body with injectable liveness budgets so tests can drive the
/// idle-disconnect path without real-time waits. Production passes
/// [`RECV_TIMEOUT_IDLE`]/[`RECV_TIMEOUT_BUSY`].
fn reader_loop(
    mut transport: Box<dyn Transport>,
    events_tx: Sender<TransportEvent>,
    shutdown: Arc<AtomicBool>,
    ctx: LinkContext,
    idle_timeout: Duration,
    busy_timeout: Duration,
) {
    let outgoing_tx = ctx.reader_outgoing_tx.clone();
    let exec_slot = ctx.exec_slot.clone();
    let clipboard_state = ctx.clipboard_state.clone();
    let outgoing_progress = ctx.outgoing_progress.clone();
    let outgoing_total = ctx.outgoing_total.clone();
    let incoming_cancel = ctx.incoming_cancel.clone();
    let outgoing_cancel = ctx.outgoing_cancel.clone();
    let current_outgoing_label = ctx.current_outgoing_label.clone();

    // Helper closure — keeps the three reset sites identical and prevents
    // future drift. `IncomingClipboard::reset()` already zeroes incoming_*;
    // we also zero outgoing_* (sole owner is writer_thread, which only ever
    // increments — safe to clobber from here at session boundaries).
    let reset_session_state = |incoming_clip: &mut clipboard::IncomingClipboard| {
        incoming_clip.reset();
        clipboard_state.reset();
        outgoing_progress.store(0, Ordering::Relaxed);
        outgoing_total.store(0, Ordering::Relaxed);
    };

    // Keep our own handle to the sender-side state so we can clear its dedup
    // hash on disconnect (Codex iter4 F1 — without this, a mid-transfer abort
    // leaves `LastKind` stamped and the next poll after reconnect dedups
    // → silent lost-update). `IncomingClipboard` gets a clone for its receive
    // path; both refer to the same `Arc<Mutex<LastKind>>`.
    let mut incoming_clip = clipboard::IncomingClipboard::new(
        ctx.clipboard_state.clone(),
        ctx.incoming_progress.clone(),
        ctx.incoming_total.clone(),
        ctx.receive_images.clone(),
        ctx.receive_text.clone(),
        ctx.receive_files.clone(),
    );

    // Frame-error storm detector — see module + StormCounter docs.
    let mut storm = StormCounter::new(DEFAULT_STORM_THRESHOLD);

    // Receive-liveness clock. Reset on every decoded packet (the host's 2 s
    // heartbeat keeps it fresh on a live idle link). If it runs past the
    // budget while `recv()` only times out, the peer is gone — see the
    // timeout arm below.
    let mut last_recv = Instant::now();

    // Cancel-batch state — same role as the writer-side counters: log a
    // single START and a single END line per cancel sweep instead of one
    // per dropped chunk.
    let mut cancel_seen = false;
    let mut cancel_drop_count: u32 = 0;
    loop {
        if shutdown.load(Ordering::Acquire) {
            return;
        }
        match transport.recv() {
            Ok(p) => {
                // A real packet decoded → the channel is alive; clear the
                // storm run and refresh the liveness clock.
                storm.on_valid_packet();
                last_recv = Instant::now();
                match p.message {
                    Message::HelloAck {
                        host_name,
                        screen_w,
                        screen_h,
                        ..
                    } => {
                        log::info!("connected to '{host_name}' ({screen_w}x{screen_h})");
                        reset_session_state(&mut incoming_clip);
                        // Cache host identity/geometry for the interactive
                        // relay's synth `HelloAck` (see LinkContext::host_info).
                        // Populated BEFORE link_up flips true so a relay that
                        // observes link_up==true always sees a matching cache.
                        if let Ok(mut hi) = ctx.host_info.lock() {
                            *hi = Some(HostInfo {
                                host_name: host_name.clone(),
                                screen_w: screen_w.into(),
                                screen_h: screen_h.into(),
                            });
                        }
                        // Handshake complete — the link is now usable; open
                        // the IPC gate (see LinkContext::link_up).
                        ctx.link_up.store(true, Ordering::Release);
                        let _ = events_tx.send(TransportEvent::Connected {
                            host_name,
                            screen_w,
                            screen_h,
                        });
                    }
                    Message::Heartbeat => {
                        let _ = events_tx.send(TransportEvent::Heartbeat);
                    }
                    Message::ClipOffer { format, total_len } => {
                        if incoming_cancel.swap(false, Ordering::AcqRel) && cancel_seen {
                            log::info!(
                                "clipboard.recv cancel complete ({cancel_drop_count} chunks dropped)"
                            );
                        }
                        cancel_seen = false;
                        cancel_drop_count = 0;
                        if let Some(decline) = incoming_clip.on_offer(format, total_len) {
                            // Tell host to drop its outbox — without this it
                            // would keep streaming chunks we're going to
                            // discard, saturating RX and starving TX.
                            let _ = outgoing_tx.send(Packet::new(decline, 0));
                        }
                    }
                    Message::ClipDecline { format } => {
                        let toast = clipboard::apply_clip_decline_with_label(
                            format,
                            &outgoing_cancel,
                            &current_outgoing_label,
                        );
                        let _ = events_tx.send(TransportEvent::Toast(toast));
                    }
                    Message::ClipChunk { index, data } => {
                        if incoming_cancel.load(Ordering::Acquire) {
                            if !cancel_seen {
                                log::info!(
                                    "clipboard.recv CANCELLED — dropping chunks (first idx {index})"
                                );
                                incoming_clip.reset();
                                cancel_seen = true;
                                cancel_drop_count = 0;
                            }
                            cancel_drop_count = cancel_drop_count.saturating_add(1);
                            continue;
                        }
                        incoming_clip.on_chunk(index, data);
                    }
                    // Shell output/exit/errors are consumed only by the exec &
                    // interactive-IPC paths via the `exec_slot` fan-out. The GUI
                    // shell-panel was removed (interactive `wd` runs over the
                    // socket relay), so there is no `TransportEvent::Shell*`
                    // consumer left — we broadcast to the slot and stop there.
                    Message::ShellOutput { data } => {
                        exec_bridge::broadcast_exec_event(
                            &exec_slot,
                            wiredesk_exec_core::ExecEvent::ShellOutput(data),
                        );
                    }
                    Message::ShellExit { code } => {
                        exec_bridge::broadcast_exec_event(
                            &exec_slot,
                            wiredesk_exec_core::ExecEvent::ShellExit(code),
                        );
                    }
                    Message::Error { code, msg } => {
                        log::warn!("error from host: code={code} msg={msg}");
                        if msg.contains("shell") {
                            exec_bridge::broadcast_exec_event(
                                &exec_slot,
                                wiredesk_exec_core::ExecEvent::HostError(msg),
                            );
                        }
                    }
                    Message::Disconnect => {
                        log::info!("host disconnected");
                        reset_session_state(&mut incoming_clip);
                        let _ = events_tx
                            .send(TransportEvent::Disconnected("host disconnected".into()));
                        return;
                    }
                    other => {
                        log::debug!("ignored message: {other:?}");
                    }
                }
            }
            Err(ref e) if e.to_string().contains("timeout") => {
                // Silent disconnect: host quit / crash / cable yanked on the
                // remote side leaves our local fd open, so `recv()` only ever
                // times out (no fatal error) and the writer's heartbeats sink
                // into a dead wire. Without this check the reader would loop
                // forever, `link_up` would stay true, and the supervisor would
                // never reopen. If no packet (incl. the host's 2 s heartbeat)
                // arrived within the liveness budget, treat the link as gone.
                let limit = if transfer_in_flight(&ctx) {
                    busy_timeout
                } else {
                    idle_timeout
                };
                if last_recv.elapsed() >= limit {
                    log::warn!(
                        "host link lost — no packet for {:?} — reopening port",
                        last_recv.elapsed()
                    );
                    reset_session_state(&mut incoming_clip);
                    let _ = events_tx.send(TransportEvent::Disconnected(
                        "host link lost — no heartbeat".into(),
                    ));
                    return;
                }
                continue;
            }
            Err(WireDeskError::Protocol(ref msg)) => {
                if storm.on_protocol_error() {
                    log::error!(
                        "frame-error storm detected ({} consecutive) — reopening port",
                        storm.count()
                    );
                    reset_session_state(&mut incoming_clip);
                    let _ = events_tx.send(TransportEvent::Disconnected(
                        "frame-error storm — reopening port".into(),
                    ));
                    return;
                }
                log::warn!("dropping bad frame: {msg}");
                continue;
            }
            Err(e) => {
                log::error!("transport error: {e}");
                reset_session_state(&mut incoming_clip);
                let _ = events_tx.send(TransportEvent::Disconnected(e.to_string()));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use wiredesk_core::error::Result;

    /// What a scripted `recv()` should do next.
    enum Step {
        Protocol,
        Valid(Packet),
        Fatal,
    }

    /// Test transport that replays a scripted series of `recv()` outcomes.
    /// When the script is exhausted it returns a recv timeout forever (with a
    /// short sleep) — modelling a silent/idle link.
    struct ScriptedTransport {
        steps: Arc<Mutex<VecDeque<Step>>>,
        send_ok: bool,
    }

    impl ScriptedTransport {
        fn new(steps: Vec<Step>, send_ok: bool) -> Self {
            Self {
                steps: Arc::new(Mutex::new(steps.into_iter().collect())),
                send_ok,
            }
        }
    }

    impl Transport for ScriptedTransport {
        fn send(&mut self, _packet: &Packet) -> Result<()> {
            if self.send_ok {
                Ok(())
            } else {
                Err(WireDeskError::Transport("scripted send failure".into()))
            }
        }
        fn recv(&mut self) -> Result<Packet> {
            let step = self.steps.lock().unwrap().pop_front();
            match step {
                Some(Step::Protocol) => Err(WireDeskError::Protocol("scripted bad frame".into())),
                Some(Step::Valid(p)) => Ok(p),
                Some(Step::Fatal) => Err(WireDeskError::Transport("scripted fatal".into())),
                None => {
                    // Idle link — keep timing out so the reader loops until
                    // told to shut down.
                    thread::sleep(Duration::from_millis(2));
                    Err(WireDeskError::Transport("recv timeout".into()))
                }
            }
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "scripted"
        }
        fn try_clone(&self) -> Result<Box<dyn Transport>> {
            Ok(Box::new(ScriptedTransport {
                steps: self.steps.clone(),
                send_ok: self.send_ok,
            }))
        }
    }

    fn test_ctx() -> (LinkContext, Receiver<Packet>) {
        let (tx, rx) = mpsc::channel();
        let ctx = LinkContext {
            client_name: "test".into(),
            clipboard_state: clipboard::ClipboardState::new(),
            outgoing_progress: Arc::new(AtomicU64::new(0)),
            outgoing_total: Arc::new(AtomicU64::new(0)),
            incoming_progress: Arc::new(AtomicU64::new(0)),
            incoming_total: Arc::new(AtomicU64::new(0)),
            receive_images: Arc::new(AtomicBool::new(true)),
            receive_text: Arc::new(AtomicBool::new(true)),
            receive_files: Arc::new(AtomicBool::new(true)),
            incoming_cancel: Arc::new(AtomicBool::new(false)),
            outgoing_cancel: Arc::new(AtomicBool::new(false)),
            exec_slot: Arc::new(std::sync::Mutex::new(None)),
            current_outgoing_label: Arc::new(std::sync::Mutex::new(String::new())),
            reader_outgoing_tx: tx,
            link_up: Arc::new(AtomicBool::new(false)),
            host_info: Arc::new(Mutex::new(None)),
        };
        (ctx, rx)
    }

    /// A scripted HelloAck — drives the reader's handshake-complete path so
    /// supervisor tests can observe `link_up` flipping true.
    fn hello_ack() -> Packet {
        Packet::new(
            Message::HelloAck {
                version: VERSION,
                host_name: "test-host".into(),
                screen_w: 100,
                screen_h: 100,
            },
            0,
        )
    }

    #[test]
    fn reader_storm_emits_disconnect_after_threshold() {
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Exactly threshold Protocol errors → reader should detect storm and
        // emit a Disconnected event, then exit.
        let steps: Vec<Step> = (0..DEFAULT_STORM_THRESHOLD).map(|_| Step::Protocol).collect();
        let transport = Box::new(ScriptedTransport::new(steps, true));

        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown, ctx));

        let evt = events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected a transport event");
        match evt {
            TransportEvent::Disconnected(reason) => {
                assert!(reason.contains("frame-error storm"), "reason: {reason}");
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn reader_below_threshold_does_not_disconnect() {
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        // One short of threshold, then a valid Heartbeat. The reader must
        // process every error WITHOUT firing a storm and then surface the
        // Heartbeat — a storm would have returned before reaching it. The
        // positive Heartbeat assertion proves the errors were drained (not
        // merely "no event yet"), so the test can't pass on a slow runner
        // that simply hasn't processed the errors in time.
        let mut steps: Vec<Step> = (0..DEFAULT_STORM_THRESHOLD - 1)
            .map(|_| Step::Protocol)
            .collect();
        steps.push(Step::Valid(Packet::new(Message::Heartbeat, 0)));
        let transport = Box::new(ScriptedTransport::new(steps, true));
        let shutdown_c = shutdown.clone();
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown_c, ctx));

        let evt = events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected the heartbeat after threshold-1 errors");
        assert!(
            matches!(evt, TransportEvent::Heartbeat),
            "expected Heartbeat (no storm), got {evt:?}"
        );
        // No Disconnected should follow.
        assert!(events_rx.try_recv().is_err(), "unexpected event after heartbeat");
        shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    #[test]
    fn reader_fatal_error_emits_disconnect() {
        // Plain-disconnect path (host quit / cable yank / send-recv fatal):
        // a non-timeout, non-Protocol recv error must emit Disconnected and
        // exit so the supervisor reopens. This is the second of the two
        // recovery triggers named in the module docs.
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let transport = Box::new(ScriptedTransport::new(vec![Step::Fatal], true));
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown, ctx));

        let evt = events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected a transport event");
        match evt {
            TransportEvent::Disconnected(reason) => {
                assert!(reason.contains("scripted fatal"), "reason: {reason}");
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn reader_valid_packet_resets_storm() {
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        // threshold-1 errors, then a valid Heartbeat (resets the run), then
        // threshold-1 errors again — never a full run, so no storm.
        let mut steps: Vec<Step> = (0..DEFAULT_STORM_THRESHOLD - 1)
            .map(|_| Step::Protocol)
            .collect();
        steps.push(Step::Valid(Packet::new(Message::Heartbeat, 0)));
        steps.extend((0..DEFAULT_STORM_THRESHOLD - 1).map(|_| Step::Protocol));
        let transport = Box::new(ScriptedTransport::new(steps, true));
        let shutdown_c = shutdown.clone();
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown_c, ctx));

        // The valid Heartbeat must surface as an event...
        let evt = events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected the heartbeat event");
        assert!(matches!(evt, TransportEvent::Heartbeat), "got {evt:?}");
        // ...and no Disconnected should follow (storm never reached threshold).
        thread::sleep(Duration::from_millis(100));
        assert!(events_rx.try_recv().is_err(), "unexpected event after reset");

        shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    #[test]
    fn transfer_in_flight_reflects_counters() {
        let (ctx, _rx) = test_ctx();
        // Fresh ctx, no shell → idle.
        assert!(!transfer_in_flight(&ctx));
        // Outgoing transfer started (total set, progress behind) → busy.
        ctx.outgoing_total.store(100, Ordering::Relaxed);
        ctx.outgoing_progress.store(10, Ordering::Relaxed);
        assert!(transfer_in_flight(&ctx));
        // Outgoing finished (progress caught up) → idle again.
        ctx.outgoing_progress.store(100, Ordering::Relaxed);
        assert!(!transfer_in_flight(&ctx));
        // Incoming transfer in flight → busy.
        ctx.incoming_total.store(50, Ordering::Relaxed);
        ctx.incoming_progress.store(0, Ordering::Relaxed);
        assert!(transfer_in_flight(&ctx));
    }

    #[test]
    fn reader_idle_timeout_emits_disconnect() {
        // Silent remote disconnect (host quit / crash / remote-side unplug):
        // our local fd stays open so `recv()` only times out. The reader must
        // notice the dead link via the receive-liveness budget and emit
        // Disconnected so the supervisor reopens — otherwise link_up stays true
        // forever (the AC1 gap this fix closes).
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Empty script → idle transport that only ever times out.
        let transport = Box::new(ScriptedTransport::new(vec![], true));
        let handle = thread::spawn(move || {
            reader_loop(
                transport,
                events_tx,
                shutdown,
                ctx,
                Duration::from_millis(40), // tiny idle budget for the test
                Duration::from_secs(30),
            )
        });

        let evt = events_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("expected a Disconnected after the idle budget elapsed");
        match evt {
            TransportEvent::Disconnected(reason) => {
                assert!(reason.contains("host link lost"), "reason: {reason}");
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
        handle.join().unwrap();
    }

    #[test]
    fn reader_busy_budget_suppresses_idle_disconnect() {
        // With a transfer in flight the reader uses the looser busy budget, so
        // the strict idle window must NOT fire mid-transfer (false-positive
        // guard). Idle budget tiny, busy budget large → no Disconnected while
        // the outgoing counter says a push is in progress.
        let (ctx, _reader_outgoing_rx) = test_ctx();
        ctx.outgoing_total.store(1000, Ordering::Relaxed);
        ctx.outgoing_progress.store(1, Ordering::Relaxed);
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let transport = Box::new(ScriptedTransport::new(vec![], true));
        let shutdown_c = shutdown.clone();
        let handle = thread::spawn(move || {
            reader_loop(
                transport,
                events_tx,
                shutdown_c,
                ctx,
                Duration::from_millis(20),  // idle budget would fire fast...
                Duration::from_secs(30),    // ...but busy budget keeps us alive
            )
        });

        // Well past the idle budget but far under the busy budget → silence.
        assert!(
            events_rx
                .recv_timeout(Duration::from_millis(200))
                .is_err(),
            "unexpected Disconnected while a transfer was in flight"
        );
        shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    #[test]
    fn writer_returns_receiver_on_exit() {
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, _events_rx) = mpsc::channel();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let shutdown = Arc::new(AtomicBool::new(false));

        // send_ok=false → the Hello send fails immediately and the writer
        // returns the receiver.
        let transport = Box::new(ScriptedTransport::new(vec![], false));
        let handle =
            thread::spawn(move || writer_thread(transport, outgoing_rx, events_tx, shutdown, ctx));
        let returned_rx = handle.join().unwrap();

        // The outgoing_tx clone is still valid; a packet sent now must be
        // readable from the returned receiver (channel survived the writer).
        outgoing_tx
            .send(Packet::new(Message::Heartbeat, 0))
            .expect("send on surviving channel");
        let got = returned_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("packet available on returned receiver");
        assert!(matches!(got.message, Message::Heartbeat));
    }

    #[test]
    fn writer_exits_on_shutdown_flag_even_when_send_succeeds() {
        // Regression: during a frame-error storm the serial fd stays open, so
        // `send()` keeps returning Ok and the writer never reaches an
        // error-exit. The supervisor must be able to stop it via the shutdown
        // flag — otherwise `writer.join()` at teardown deadlocks and the port
        // is never reopened (the exact scenario this feature exists to fix).
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, _events_rx) = mpsc::channel();
        let (_outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let shutdown = Arc::new(AtomicBool::new(false));

        // send_ok=true + empty script → Hello sends OK, then the writer loops
        // on recv_timeout + periodic heartbeats, never erroring.
        let transport = Box::new(ScriptedTransport::new(vec![], true));
        let shutdown_c = shutdown.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let rx = writer_thread(transport, outgoing_rx, events_tx, shutdown_c, ctx);
            let _ = done_tx.send(());
            rx
        });

        thread::sleep(Duration::from_millis(50));
        shutdown.store(true, Ordering::Release);
        // Must exit within a heartbeat interval + slack once shutdown is set.
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("writer never exited on shutdown — would deadlock supervisor teardown");
        handle.join().unwrap();
    }

    #[test]
    fn reader_exits_on_shutdown_flag() {
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, _events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Empty script → ScriptedTransport idles with recv timeouts forever.
        let transport = Box::new(ScriptedTransport::new(vec![], true));
        let shutdown_c = shutdown.clone();
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown_c, ctx));

        thread::sleep(Duration::from_millis(50));
        shutdown.store(true, Ordering::Release);
        // Must terminate promptly once shutdown is raised.
        let start = Instant::now();
        handle.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn supervisor_retries_then_links_up() {
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let link_up = ctx.link_up.clone();
        let (events_tx, events_rx) = mpsc::channel();
        let (_outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let (request_tx, request_rx) = mpsc::channel::<()>();

        // open_fn fails twice, succeeds on the 3rd call with a transport that
        // immediately hands the reader a HelloAck (handshake completes →
        // link_up flips true; an open port alone must NOT raise the flag).
        let mut calls = 0u32;
        let open_fn = move || -> Result<Box<dyn Transport>> {
            calls += 1;
            if calls < 3 {
                Err(WireDeskError::Transport(format!("open fail {calls}")))
            } else {
                Ok(Box::new(ScriptedTransport::new(vec![Step::Valid(hello_ack())], true))
                    as Box<dyn Transport>)
            }
        };

        let _sup = spawn_supervisor(
            open_fn,
            |_| Duration::from_millis(5), // near-zero backoff for the test
            outgoing_rx,
            events_tx,
            request_rx,
            ctx,
        );

        // Kick off the initial open.
        request_tx.send(()).unwrap();

        // Expect Reconnecting{1}, Reconnecting{2}, Reconnecting{3}.
        let mut attempts = Vec::new();
        for _ in 0..3 {
            match events_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(TransportEvent::Reconnecting { attempt }) => attempts.push(attempt),
                other => panic!("expected Reconnecting, got {other:?}"),
            }
        }
        assert_eq!(attempts, vec![1, 2, 3]);

        // After the 3rd attempt the link is spawned → link_up flips true.
        let start = Instant::now();
        while !link_up.load(Ordering::Acquire) {
            assert!(start.elapsed() < Duration::from_secs(2), "link never came up");
            thread::sleep(Duration::from_millis(10));
        }
        assert!(link_up.load(Ordering::Acquire));

        // Drop the request sender so the supervisor's next recv() returns Err
        // and the thread can wind down with the test.
        drop(request_tx);
    }

    #[test]
    fn supervisor_reconnects_live_link_without_deadlock() {
        // Regression for the storm path: tearing down a LIVE link — one whose
        // writer `send()` still returns Ok (fd open, chip corrupting) — must
        // not block on `writer.join()`. Drives a full up → teardown → up cycle.
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let link_up = ctx.link_up.clone();
        let (events_tx, events_rx) = mpsc::channel();
        let (_outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let (request_tx, request_rx) = mpsc::channel::<()>();

        // Every open succeeds with a send-OK transport that completes the
        // handshake (HelloAck) — models a fd that stays open across the storm.
        let open_fn = move || -> Result<Box<dyn Transport>> {
            Ok(Box::new(ScriptedTransport::new(vec![Step::Valid(hello_ack())], true))
                as Box<dyn Transport>)
        };

        let _sup = spawn_supervisor(
            open_fn,
            |_| Duration::from_millis(5),
            outgoing_rx,
            events_tx,
            request_rx,
            ctx,
        );

        let wait_up = || {
            let start = Instant::now();
            while !link_up.load(Ordering::Acquire) {
                assert!(start.elapsed() < Duration::from_secs(3), "link never came up");
                thread::sleep(Duration::from_millis(5));
            }
        };

        // First open → link up.
        request_tx.send(()).unwrap();
        match events_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(TransportEvent::Reconnecting { attempt }) => assert_eq!(attempt, 1),
            other => panic!("expected Reconnecting{{1}}, got {other:?}"),
        }
        wait_up();
        // Small settle so the supervisor is back in recv() before we ask for
        // a teardown (the drain runs pre-spawn, so our request can't be
        // swallowed — this sleep just de-flakes the event ordering).
        thread::sleep(Duration::from_millis(20));

        // Tear down the live link and reopen. The second Reconnecting event is
        // emitted only AFTER `writer.join()` returns — if the writer can't be
        // stopped, this recv times out (the regression). Skip interleaved
        // events (the first link's Connected from its HelloAck).
        request_tx.send(()).unwrap();
        let start = Instant::now();
        loop {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "never saw the second Reconnecting after teardown"
            );
            match events_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(TransportEvent::Reconnecting { attempt }) => {
                    assert_eq!(attempt, 1);
                    break;
                }
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("events channel died: {e}"),
            }
        }
        wait_up();

        drop(request_tx);
    }

    #[test]
    fn supervisor_honors_request_from_instantly_dead_link() {
        // Codex P2 regression: a fresh link whose HELLO send fails right away
        // emits Disconnected immediately after spawn. The UI answers with a
        // reconnect request — which the old post-spawn drain would swallow as
        // a "stale duplicate", leaving the supervisor parked in recv() with a
        // dead transport forever. With the drain moved BEFORE the spawn, that
        // request must drive a second reopen cycle.
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, events_rx) = mpsc::channel();
        let (_outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let (request_tx, request_rx) = mpsc::channel::<()>();

        // 1st open → send-broken transport (HELLO fails instantly, writer
        // exits with Disconnected). 2nd open → healthy idle transport.
        let mut calls = 0u32;
        let open_fn = move || -> Result<Box<dyn Transport>> {
            calls += 1;
            Ok(Box::new(ScriptedTransport::new(vec![], calls > 1)) as Box<dyn Transport>)
        };

        let _sup = spawn_supervisor(
            open_fn,
            |_| Duration::from_millis(5),
            outgoing_rx,
            events_tx,
            request_rx,
            ctx,
        );

        // Kick the initial open → broken link spawns and dies immediately.
        request_tx.send(()).unwrap();
        let mut saw_disconnect = false;
        // Pump events until the writer's Disconnected surfaces.
        let start = Instant::now();
        while !saw_disconnect {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "never saw the instant-death Disconnected"
            );
            if let Ok(TransportEvent::Disconnected(_)) =
                events_rx.recv_timeout(Duration::from_millis(200))
            {
                saw_disconnect = true;
            }
        }

        // Play the UI's role: answer the Disconnected with a request. With the
        // old post-spawn drain this could be eaten; now it must start a second
        // reopen cycle. NOTE: `link_up` can't be the success signal here — it
        // is still true from the first (instantly-dead) link — so we assert on
        // the second cycle's Reconnecting event instead.
        request_tx.send(()).unwrap();

        let start = Instant::now();
        loop {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "supervisor swallowed the fresh link's reconnect request"
            );
            match events_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(TransportEvent::Reconnecting { .. }) => break, // second cycle started
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(e) => panic!("events channel died: {e}"),
            }
        }

        drop(request_tx);
    }

    #[test]
    fn link_up_requires_handshake_not_just_spawn() {
        // Codex iter4 P2 regression: an open port alone must NOT open the IPC
        // gate. link_up may flip true only on HelloAck, and must drop back to
        // false when the reader exits.
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let link_up = ctx.link_up.clone();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        // Reader over an idle transport (no HelloAck): the flag must stay
        // false no matter how long the link has been "spawned".
        let transport = Box::new(ScriptedTransport::new(vec![], true));
        let shutdown_c = shutdown.clone();
        let ctx_c = ctx.clone();
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown_c, ctx_c));
        thread::sleep(Duration::from_millis(50));
        assert!(
            !link_up.load(Ordering::Acquire),
            "link_up must not raise before the handshake completes"
        );
        shutdown.store(true, Ordering::Release);
        handle.join().unwrap();

        // Reader that completes the handshake: flag raises on HelloAck...
        let (events_tx, events_rx2) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let transport = Box::new(ScriptedTransport::new(vec![Step::Valid(hello_ack())], true));
        let shutdown_c = shutdown.clone();
        let ctx_c = ctx.clone();
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown_c, ctx_c));
        match events_rx2.recv_timeout(Duration::from_secs(2)) {
            Ok(TransportEvent::Connected { .. }) => {}
            other => panic!("expected Connected after HelloAck, got {other:?}"),
        }
        assert!(
            link_up.load(Ordering::Acquire),
            "link_up must raise on HelloAck"
        );
        // ...and drops back to false once the reader exits.
        shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
        assert!(
            !link_up.load(Ordering::Acquire),
            "link_up must clear when the reader exits"
        );
        drop(events_rx);
    }

    #[test]
    fn reader_caches_host_info_on_hello_ack() {
        // The interactive relay synths its `HelloAck` from this cache, so the
        // reader must populate it (host_name/geometry from the handshake) the
        // moment HelloAck arrives — and while the link stays up.
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let host_info = ctx.host_info.clone();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        // HelloAck then an idle link (keeps the reader alive so we can observe
        // the populated cache before the exit path would clear it).
        let transport = Box::new(ScriptedTransport::new(vec![Step::Valid(hello_ack())], true));
        let shutdown_c = shutdown.clone();
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown_c, ctx));

        // Wait for the Connected event so we know HelloAck was processed.
        match events_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(TransportEvent::Connected { .. }) => {}
            other => panic!("expected Connected after HelloAck, got {other:?}"),
        }
        assert_eq!(
            *host_info.lock().unwrap(),
            Some(HostInfo {
                host_name: "test-host".into(),
                screen_w: 100,
                screen_h: 100,
            }),
            "host_info must be cached from the HelloAck"
        );

        shutdown.store(true, Ordering::Release);
        handle.join().unwrap();
    }

    #[test]
    fn reader_clears_host_info_on_disconnect() {
        // On any link-down the cache must be cleared so the relay never synths
        // a `HelloAck` for a host we're no longer connected to. Drive HelloAck
        // (populate) → host Disconnect (reader exits, must clear).
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let host_info = ctx.host_info.clone();
        let (events_tx, events_rx) = mpsc::channel();
        let shutdown = Arc::new(AtomicBool::new(false));

        let transport = Box::new(ScriptedTransport::new(
            vec![
                Step::Valid(hello_ack()),
                Step::Valid(Packet::new(Message::Disconnect, 0)),
            ],
            true,
        ));
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown, ctx));

        // Reader exits on the Disconnect packet; join, then assert cleared.
        handle.join().unwrap();
        assert_eq!(
            *host_info.lock().unwrap(),
            None,
            "host_info must be cleared when the reader exits on disconnect"
        );
        // Drain the events so the channel isn't dropped mid-send in the reader.
        while events_rx.try_recv().is_ok() {}
    }
}
