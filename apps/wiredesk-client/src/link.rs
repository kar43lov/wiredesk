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
}

/// Join handles for one reader/writer pair. The writer's handle resolves to
/// the `outgoing_rx` it owned so the supervisor can hand it to the next link.
pub struct LinkHandles {
    pub writer: JoinHandle<Receiver<Packet>>,
    pub reader: JoinHandle<()>,
}

/// Spawn a reader/writer pair over the given transport handles. `shutdown`
/// lets the supervisor stop the reader even when its `recv()` only ever times
/// out (silent host quit / unplug) — without it `join()` would hang forever.
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
        thread::spawn(move || writer_thread(writer_t, outgoing_rx, events_tx, ctx))
    };
    let reader = thread::spawn(move || reader_thread(reader_t, events_tx, shutdown, ctx));
    LinkHandles { writer, reader }
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
#[allow(clippy::too_many_arguments)]
pub fn spawn_supervisor(
    mut open_fn: impl FnMut() -> Result<Box<dyn Transport>, WireDeskError> + Send + 'static,
    mut backoff_fn: impl FnMut(u32) -> Duration + Send + 'static,
    outgoing_rx: Receiver<Packet>,
    events_tx: Sender<TransportEvent>,
    reconnect_request_rx: Receiver<()>,
    link_up: Arc<AtomicBool>,
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

            // Tear down the current link, if one is up.
            link_up.store(false, Ordering::Release);
            if let Some(h) = handles.take() {
                // Reader exits on the shutdown flag we raise here; writer
                // exits when its transport handle fails (the old fd is dead)
                // and returns the receiver we need for the next link.
                // The shutdown flag belonged to the now-departing link; the
                // next link gets a fresh one below.
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

            let shutdown = Arc::new(AtomicBool::new(false));
            handles = Some(spawn_link(
                reader_t,
                writer_t,
                rx,
                events_tx.clone(),
                shutdown,
                ctx.clone(),
            ));
            link_up.store(true, Ordering::Release);

            // Drain duplicate requests that piled up while we were
            // reconnecting so we don't immediately tear down the fresh link.
            while reconnect_request_rx.try_recv().is_ok() {}
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
            let _ = transport.send(&Packet::new(Message::Heartbeat, 0));
            last_heartbeat = Instant::now();
        }
    }
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
    mut transport: Box<dyn Transport>,
    events_tx: Sender<TransportEvent>,
    shutdown: Arc<AtomicBool>,
    ctx: LinkContext,
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
                // storm run.
                storm.on_valid_packet();
                match p.message {
                    Message::HelloAck {
                        host_name,
                        screen_w,
                        screen_h,
                        ..
                    } => {
                        log::info!("connected to '{host_name}' ({screen_w}x{screen_h})");
                        reset_session_state(&mut incoming_clip);
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
                    Message::ShellOutput { data } => {
                        exec_bridge::broadcast_exec_event(
                            &exec_slot,
                            wiredesk_exec_core::ExecEvent::ShellOutput(data.clone()),
                        );
                        let _ = events_tx.send(TransportEvent::ShellOutput(data));
                    }
                    Message::ShellExit { code } => {
                        exec_bridge::broadcast_exec_event(
                            &exec_slot,
                            wiredesk_exec_core::ExecEvent::ShellExit(code),
                        );
                        let _ = events_tx.send(TransportEvent::ShellExit(code));
                    }
                    Message::Error { code, msg } => {
                        log::warn!("error from host: code={code} msg={msg}");
                        if msg.contains("shell") {
                            exec_bridge::broadcast_exec_event(
                                &exec_slot,
                                wiredesk_exec_core::ExecEvent::HostError(msg.clone()),
                            );
                            let _ = events_tx.send(TransportEvent::ShellError(msg));
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
            Err(ref e) if e.to_string().contains("timeout") => continue,
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
        #[allow(dead_code)]
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
        };
        (ctx, rx)
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

        // One short of threshold, then the script idles (timeouts). No
        // Disconnected should arrive; we then shut the reader down.
        let steps: Vec<Step> = (0..DEFAULT_STORM_THRESHOLD - 1)
            .map(|_| Step::Protocol)
            .collect();
        let transport = Box::new(ScriptedTransport::new(steps, true));
        let shutdown_c = shutdown.clone();
        let handle = thread::spawn(move || reader_thread(transport, events_tx, shutdown_c, ctx));

        // Give it time to chew through the errors and start idling.
        thread::sleep(Duration::from_millis(100));
        // No event should be queued.
        assert!(events_rx.try_recv().is_err());
        shutdown.store(true, Ordering::Release);
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
    fn writer_returns_receiver_on_exit() {
        let (ctx, _reader_outgoing_rx) = test_ctx();
        let (events_tx, _events_rx) = mpsc::channel();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();

        // send_ok=false → the Hello send fails immediately and the writer
        // returns the receiver.
        let transport = Box::new(ScriptedTransport::new(vec![], false));
        let handle = thread::spawn(move || writer_thread(transport, outgoing_rx, events_tx, ctx));
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
        let (events_tx, events_rx) = mpsc::channel();
        let (_outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let (request_tx, request_rx) = mpsc::channel::<()>();
        let link_up = Arc::new(AtomicBool::new(false));

        // open_fn fails twice, succeeds on the 3rd call with an idle
        // transport.
        let mut calls = 0u32;
        let open_fn = move || -> Result<Box<dyn Transport>> {
            calls += 1;
            if calls < 3 {
                Err(WireDeskError::Transport(format!("open fail {calls}")))
            } else {
                Ok(Box::new(ScriptedTransport::new(vec![], true)) as Box<dyn Transport>)
            }
        };

        let link_up_c = link_up.clone();
        let _sup = spawn_supervisor(
            open_fn,
            |_| Duration::from_millis(5), // near-zero backoff for the test
            outgoing_rx,
            events_tx,
            request_rx,
            link_up_c,
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
}
