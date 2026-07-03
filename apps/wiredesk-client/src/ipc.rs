//! Mac-only IPC server: accepts connections from `wd --exec` and runs
//! the shared sentinel-driven runner against the GUI's already-open
//! serial port. Without this, GUI and `wd --exec` are mutually
//! exclusive (both want `open()` on the same port). With this, GUI
//! holds the port and `wd --exec` connects to a Unix socket; if the
//! socket isn't there, the term's `try_socket_first` falls back to
//! direct serial — backward-compatible.
//!
//! Lifecycle:
//! 1. `spawn_ipc_acceptor` runs once at GUI startup. It tries to bind
//!    `~/Library/Application Support/WireDesk/wd-exec.sock` (unlinking
//!    any stale socket from a prior crash first), `chmod 0600`, then
//!    spawns a thread that loops over `incoming()`.
//! 2. Per connection: `single_inflight` mutex serialises concurrent
//!    `wd --exec` runs (rare — typically one Claude in chat). The
//!    handler installs an `ExecSlotGuard` so `reader_thread` fans
//!    shell-events into our private mpsc, then drives the runner with
//!    a callback that writes `IpcResponse::Stdout(...)` onto the socket.
//!    Final `IpcResponse::Exit(code)` (or `Error(...)`) closes the round.
//!
//! All guards (`MutexGuard<()>` for inflight, `ExecSlotGuard` for the
//! reader broadcast) are RAII so a panicking handler can't strand
//! state — the next connection finds a clean slate.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wiredesk_exec_core::{
    ipc::{
        read_connect, read_packet_frame, write_packet_frame, write_response, IpcConnect,
        IpcInteractiveOpen, IpcRequest, IpcResponse,
    },
    ExecError, ExecEvent, ExecTransport,
};
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;

use crate::exec_bridge::{ExecEventSlot, ExecSlotGuard};
use crate::link::SharedHostInfo;
use crate::shell_channel::{try_acquire, SharedShellOwner, ShellOwner};

/// `ExecTransport` impl that bridges the runner to the GUI's existing
/// outgoing-packet channel and the IPC handler's mpsc. `send_input`
/// pushes a `ShellInput` packet into `outgoing_tx` (writer thread
/// picks it up); `recv_event` blocks on the per-handler `rx` for at
/// most `timeout`, returning `Idle` on tick so the runner can re-check
/// its overall budget.
struct IpcExecTransport {
    outgoing_tx: mpsc::Sender<Packet>,
    rx: mpsc::Receiver<ExecEvent>,
}

impl ExecTransport for IpcExecTransport {
    fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
        self.outgoing_tx
            .send(Packet::new(
                Message::ShellInput { data: data.to_vec() },
                0,
            ))
            .map_err(|_| ExecError::Closed)
    }

    fn recv_event(&mut self, timeout: Duration) -> Result<ExecEvent, ExecError> {
        match self.rx.recv_timeout(timeout) {
            Ok(ev) => Ok(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(ExecEvent::Idle),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ExecError::Closed),
        }
    }
}

/// Bind the Unix socket and spawn an acceptor thread. Failure to bind
/// (missing parent dir, EADDRINUSE race, permission denied) logs a
/// warning and returns — GUI continues without IPC, term's
/// `try_socket_first` will fall back to direct serial.
#[allow(clippy::too_many_arguments)]
pub fn spawn_ipc_acceptor(
    socket_path: PathBuf,
    outgoing_tx: mpsc::Sender<Packet>,
    exec_slot: ExecEventSlot,
    shell_owner: SharedShellOwner,
    single_inflight: Arc<Mutex<()>>,
    host_info: SharedHostInfo,
    link_up: Arc<AtomicBool>,
) {
    // Stale socket from prior crash — `bind` fails with EADDRINUSE
    // unless we unlink first. Ignore not-found.
    let _ = std::fs::remove_file(&socket_path);

    // Ensure parent dir exists. If it doesn't, create it (with default
    // perms — config.toml save creates it on first run, but a fresh
    // install hitting IPC before settings save would see ENOENT).
    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!(
                "IPC acceptor: failed to create parent dir {}: {e}; wd --exec will use direct serial fallback",
                parent.display()
            );
            return;
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            log::warn!(
                "IPC bind failed at {}: {e}; wd --exec will use direct serial fallback",
                socket_path.display()
            );
            return;
        }
    };

    // 0600 — owner read/write only. Single-user Mac doesn't strictly
    // need this (FS perms on the dir would also work) but defense in
    // depth is cheap. ⚠ unwrap on metadata: if the socket vanished
    // between bind and metadata, we're in deep trouble anyway — log
    // and continue.
    if let Ok(meta) = listener.local_addr().and_then(|_| std::fs::metadata(&socket_path)) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        if let Err(e) = std::fs::set_permissions(&socket_path, perms) {
            log::warn!("IPC chmod 0600 failed: {e}");
        }
    }

    log::info!("IPC acceptor listening at {}", socket_path.display());

    thread::spawn(move || {
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    log::info!("IPC connection accepted");
                    let outgoing_tx = outgoing_tx.clone();
                    let exec_slot = exec_slot.clone();
                    let shell_owner = shell_owner.clone();
                    let single_inflight = single_inflight.clone();
                    let host_info = host_info.clone();
                    let link_up = link_up.clone();
                    thread::spawn(move || {
                        dispatch_connection(
                            stream,
                            outgoing_tx,
                            exec_slot,
                            shell_owner,
                            single_inflight,
                            host_info,
                            link_up,
                        );
                    });
                }
                Err(e) => {
                    log::warn!("IPC accept error: {e}; continuing");
                }
            }
        }
    });
}

/// Read the `IpcConnect` dispatch frame (first frame of every connection)
/// and route to the matching handler. `Exec` → the one-shot exec handler
/// (`handle_connection`); `Interactive` → the streaming PTY relay
/// (`handle_interactive_connection`). A malformed / legacy-bare-request
/// first frame fails to decode here and the connection is dropped — the
/// intended fail-closed behaviour of the lock-step cutover (Task 7).
#[allow(clippy::too_many_arguments)]
fn dispatch_connection(
    mut stream: UnixStream,
    outgoing_tx: mpsc::Sender<Packet>,
    exec_slot: ExecEventSlot,
    shell_owner: SharedShellOwner,
    single_inflight: Arc<Mutex<()>>,
    host_info: SharedHostInfo,
    link_up: Arc<AtomicBool>,
) {
    let conn = match read_connect(&mut stream) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("IPC: read_connect failed: {e}; dropping connection");
            return;
        }
    };
    match conn {
        IpcConnect::Exec(req) => handle_connection(
            stream,
            req,
            outgoing_tx,
            exec_slot,
            shell_owner,
            single_inflight,
            link_up,
        ),
        IpcConnect::Interactive(open) => handle_interactive_connection(
            stream,
            open,
            outgoing_tx,
            exec_slot,
            shell_owner,
            host_info,
            link_up,
        ),
    }
}

/// Per-connection handler for one-shot `wd --exec`. The `IpcConnect::Exec(req)`
/// frame was already decoded by `dispatch_connection`, so we take `req` by
/// value. Holds `single_inflight` for the entire run (so concurrent exec
/// connections queue FIFO), claims the shell channel as `Exec` (fail-fast if
/// an interactive session holds it), installs the `ExecSlotGuard` so
/// `reader_thread` fans shell events into our private mpsc, runs the shared
/// runner, ships the result back over the socket. All guards are RAII — panic
/// in any branch still releases them.
#[allow(clippy::too_many_arguments)]
fn handle_connection(
    mut stream: UnixStream,
    req: IpcRequest,
    outgoing_tx: mpsc::Sender<Packet>,
    exec_slot: ExecEventSlot,
    shell_owner: SharedShellOwner,
    single_inflight: Arc<Mutex<()>>,
    link_up: Arc<AtomicBool>,
) {
    log::info!(
        "IPC handler: cmd={:?} ssh={:?} timeout={}s",
        req.cmd,
        req.ssh,
        req.timeout_secs
    );

    // Serial link is mid-reconnect (supervisor cleared `link_up`): the
    // writer thread is gone, so any ShellOpen/ShellInput we'd queue
    // would block in the outgoing channel until a new link comes up,
    // and the run would just time out on a dead wire. Bail out
    // immediately with a distinct terminal frame so the term side can
    // map it to exit 125 (transport class) instead of waiting. This
    // check is BEFORE the keepalive and single_inflight acquire — no
    // point queuing against a link that isn't there.
    if !link_up.load(Ordering::Relaxed) {
        log::info!("IPC handler: link down (reconnecting) — refusing run");
        let _ = write_response(
            &mut stream,
            &IpcResponse::TransportUnavailable("transport reconnecting — retry shortly".into()),
        );
        return;
    }

    // Keepalive BEFORE acquiring single_inflight. If a prior handler
    // is stuck in run_oneshot (e.g. `--ssh dev "exit 42"` exits the
    // remote bash, ssh tunnel closes, host PS stays alive, sentinel
    // never arrives → runner waits to its full timeout), acquiring
    // the mutex would block here for up to 90 s. Without this early
    // keepalive, term's 2 s read-timeout-on-first-frame fires, term
    // falls back to direct serial, which then errors with "port busy"
    // because the GUI is still holding it. With keepalive emitted
    // first, term knows the handler is alive and queued, and just
    // waits — user can Ctrl+C if it's too long.
    if let Err(e) = write_response(&mut stream, &IpcResponse::Stdout(Vec::new())) {
        log::warn!("IPC: keepalive write failed: {e}; aborting handler");
        return;
    }

    // Serialise concurrent `wd --exec` calls. The single serial writer
    // already serialises packets, but if two callers raced into the
    // runner with overlapping ShellOpen+ShellInput sequences they'd
    // step on each other's sentinels. RAII guard keeps lock held until
    // function return / panic — `_inflight_guard` is intentional.
    let lock_started = std::time::Instant::now();
    let _inflight_guard = match single_inflight.lock() {
        Ok(g) => g,
        Err(p) => {
            log::warn!("IPC single_inflight mutex poisoned; recovering: {p:?}");
            p.into_inner()
        }
    };
    let waited = lock_started.elapsed();
    if waited > Duration::from_secs(1) {
        log::warn!(
            "IPC handler: waited {:?} for single_inflight (prior wd --exec held it long — likely an --ssh path that exited the remote shell without a matching sentinel; that handler will release on its own timeout)",
            waited
        );
    }

    // Recheck the link AFTER acquiring the slot (Codex P2 race): a request
    // that passed the early gate while the link was up may have waited here
    // (up to ~90 s behind a stuck run) through a disconnect. Without this
    // recheck it would queue ShellOpen into a reconnecting transport and
    // time out instead of failing fast with exit 125.
    if !link_up.load(Ordering::Relaxed) {
        log::info!("IPC handler: link went down while waiting for slot — refusing run");
        let _ = write_response(
            &mut stream,
            &IpcResponse::TransportUnavailable("transport reconnecting — retry shortly".into()),
        );
        return;
    }

    // Claim the shell channel as `Exec`. `single_inflight` (held above) already
    // serialises exec-vs-exec FIFO, so by the time we reach here the owner is
    // `Idle` UNLESS an interactive `wd` session holds the channel — in which
    // case we fail fast with a transport-class frame (term maps → exit 125)
    // instead of queuing behind a minutes-long PTY session. Declared *after*
    // `_inflight_guard` so on return the owner resets to `Idle` before the
    // inflight mutex releases, keeping the next queued exec's `try_acquire`
    // clean.
    let _owner_guard = match try_acquire(&shell_owner, ShellOwner::Exec) {
        Some(g) => g,
        None => {
            log::info!("IPC handler: shell channel held by interactive session — refusing exec");
            let _ = write_response(
                &mut stream,
                &IpcResponse::TransportUnavailable(
                    "shell busy — interactive wd session active".into(),
                ),
            );
            return;
        }
    };

    // Private mpsc for the duration of this run. Reader thread fans
    // ShellOutput / ShellExit / shell-Error into here via the slot
    // guard; runner pulls them as `ExecEvent`s.
    let (event_tx, event_rx) = mpsc::channel::<ExecEvent>();
    let _slot_guard = ExecSlotGuard::install(&exec_slot, event_tx);

    // Open a fresh pipe-mode shell on the host. Standalone term does
    // this in `run()` before calling run_oneshot; in IPC mode the
    // handler owns the lifecycle so we send ShellOpen here and a
    // matching ShellClose after the run. Without this, host shell
    // slot is empty and our `ShellInput` packets get ignored — what
    // the user saw as "GUI IPC unresponsive (no first frame in 2s)".
    if let Err(e) = outgoing_tx.send(Packet::new(
        Message::ShellOpen { shell: String::new() },
        0,
    )) {
        log::warn!("IPC: failed to send ShellOpen: {e}; aborting handler");
        let _ = write_response(
            &mut stream,
            &IpcResponse::Error(format!("ShellOpen send: {e}")),
        );
        return;
    }

    // Drain the PS startup banner so it doesn't pollute the runner's
    // stdout. Win11 host emits the banner + initial prompt within
    // ~100–300 ms of ShellOpen; we drain for 500 ms to be safe. Each
    // drained event is silently discarded — the runner's phase tracker
    // would have to filter them anyway, and the PS-only path now
    // streams from the first event so any leftover noise would leak.
    let drain_until = std::time::Instant::now() + Duration::from_millis(500);
    while let Some(remaining) = drain_until.checked_duration_since(std::time::Instant::now()) {
        match event_rx.recv_timeout(remaining) {
            Ok(_ev) => continue,
            Err(_) => break,
        }
    }

    let mut transport = IpcExecTransport {
        outgoing_tx: outgoing_tx.clone(),
        rx: event_rx,
    };

    // Streaming callback: each chunk = one IpcResponse::Stdout frame.
    // Failure to write means the client side disconnected; we abort
    // the closure but not the runner — the runner already committed
    // a ShellInput to the host, can't rewind. Better to let it run
    // to sentinel/timeout cleanly so single_inflight unlocks at a
    // predictable point (see plan's Cancellation section).
    //
    // We pass the closure a separate UnixStream clone (try_clone gives
    // us a second fd referencing the same kernel-side socket; writes
    // through either fd hit the same byte-stream). That avoids the
    // Arc<Mutex<UnixStream>> dance and the borrow-checker pain of
    // sharing the original stream between the closure and the
    // post-run final-frame write.
    let mut chunk_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::warn!("IPC: stream try_clone failed: {e}; aborting handler");
            return;
        }
    };

    let result = wiredesk_exec_core::run_oneshot(
        &mut transport,
        &req.cmd,
        req.ssh.as_deref(),
        req.timeout_secs,
        req.compress,
        move |chunk| {
            // Once a write fails, future calls return immediately to
            // avoid log spam. The runner has already committed work to
            // the host, so we let it run to completion — the final
            // frame attempt below will harmlessly fail too.
            if let Err(e) =
                write_response(&mut chunk_stream, &IpcResponse::Stdout(chunk.to_vec()))
            {
                log::debug!("IPC: client write failed mid-stream: {e}");
            }
        },
    );

    // Final terminal frame on the original stream. Client may have
    // already disconnected — that's fine, we still complete cleanly
    // so the inflight guard unlocks.
    let final_frame = match result {
        Ok(code) => {
            log::info!("IPC handler: exit code {code}");
            IpcResponse::Exit(code)
        }
        Err(ExecError::Timeout(_buf)) => {
            log::warn!(
                "IPC handler: timeout after {}s (no sentinel from host)",
                req.timeout_secs
            );
            IpcResponse::Exit(124)
        }
        Err(ExecError::Transport(m)) => {
            log::warn!("IPC handler: transport error: {m}");
            IpcResponse::Error(m)
        }
        Err(ExecError::Closed) => {
            log::warn!("IPC handler: transport closed (reader thread gone?)");
            IpcResponse::Error("transport closed".into())
        }
        Err(ExecError::CompressionFailed(m)) => {
            log::warn!("IPC handler: --compress decode failed: {m}");
            IpcResponse::Error(format!("compression failed: {m}"))
        }
    };

    let _ = write_response(&mut stream, &final_frame);

    // Close the shell on the host so the next IPC handler can ShellOpen
    // again. Without this, the second `wd --exec` lands in a host with
    // a shell slot still occupied by the previous run — host returns
    // "shell already open" Error and the run hangs to timeout.
    if let Err(e) = outgoing_tx.send(Packet::new(Message::ShellClose, 0)) {
        log::warn!("IPC: failed to send ShellClose: {e}");
    }

    // Post-run drain: hold `single_inflight` until the wire goes idle (or
    // host confirms ShellExit, whichever comes first). Without this the
    // next IPC handler's ShellOpen lands on a wire still saturated with
    // the prior cmd's in-flight ShellOutput chunks — host receives our
    // ShellOpen but its session loop is blocked shipping the leftover
    // output, the new ShellInput never reaches a fresh shell, and the
    // run hangs to timeout. Live-test 2026-05-06: a single ES
    // `_search?size=1` query produced 407 KB of output that kept
    // streaming for ~30 s after wd-term had already returned 124 — every
    // subsequent `wd --exec` failed timeout until we manually waited
    // through that window.
    //
    // Strategy: poll for events with a 2 s idle deadline. Each event
    // received resets the deadline; ShellExit short-circuits. If no event
    // for 2 s straight, we treat the wire as quiet and return. Hard cap
    // at SHELL_KILL_GRACE_MAX so a host that never emits ShellExit can't
    // hold the next client hostage indefinitely.
    const POST_RUN_IDLE: Duration = Duration::from_secs(2);
    const SHELL_KILL_GRACE_MAX: Duration = Duration::from_secs(30);
    let drain_started = std::time::Instant::now();
    let mut drained_events: u32 = 0;
    let mut got_exit = false;
    loop {
        if drain_started.elapsed() >= SHELL_KILL_GRACE_MAX {
            log::warn!(
                "IPC: post-cleanup drain hit max grace ({:?}); releasing single_inflight anyway",
                SHELL_KILL_GRACE_MAX
            );
            break;
        }
        match transport.rx.recv_timeout(POST_RUN_IDLE) {
            Ok(wiredesk_exec_core::ExecEvent::ShellExit(_)) => {
                got_exit = true;
                break;
            }
            Ok(_) => {
                drained_events = drained_events.saturating_add(1);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if drained_events > 0 || got_exit {
        log::info!(
            "IPC: post-cleanup drain: events_drained={} shell_exit={} elapsed={:?}",
            drained_events,
            got_exit,
            drain_started.elapsed()
        );
    }
}

/// Protocol version echoed in the synthesised `HelloAck`. The term ignores
/// the version field on receive (`link.rs` HelloAck arm binds `..`), so this
/// is informational — kept at 1 to match the host's real handshake.
const SYNTH_HELLO_ACK_VERSION: u8 = 1;

/// Error code carried by the terminal frames the interactive relay writes when
/// it refuses a connection (channel busy / link not ready). The term maps the
/// closed socket to a transport-class exit; the message is for the user's eyes.
const RELAY_REFUSE_CODE: u16 = 125;

/// Build the `Error` packet the relay writes to the socket when it refuses an
/// interactive connect (owner already held, or link/host not ready).
fn relay_error_packet(msg: &str) -> Packet {
    Packet::new(
        Message::Error {
            code: RELAY_REFUSE_CODE,
            msg: msg.to_string(),
        },
        0,
    )
}

/// Synthesize a `HelloAck` from the cached host-info. The term connects to the
/// socket *after* the GUI already handshook with the host, so its `Hello` is
/// answered from this cache instead of being forwarded to the wire. Falls back
/// to empty host_name / zero geometry if the cache somehow drained between the
/// caller's readiness check and this call (the term tolerates it — geometry is
/// re-derived from its own `terminal::size()`).
fn synth_hello_ack(host_info: &SharedHostInfo) -> Packet {
    let (host_name, screen_w, screen_h) = host_info
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|hi| (hi.host_name, hi.screen_w as u16, hi.screen_h as u16))
        .unwrap_or_else(|| (String::new(), 0, 0));
    Packet::new(
        Message::HelloAck {
            version: SYNTH_HELLO_ACK_VERSION,
            host_name,
            screen_w,
            screen_h,
        },
        0,
    )
}

/// Per-connection handler for an **interactive** `wd` session (streaming PTY
/// over the socket). Unlike the one-shot exec handler, both directions carry
/// raw `Packet`s after the `IpcConnect::Interactive` frame the acceptor already
/// consumed. This function:
///
///   1. claims the shell channel as `Interactive` (cross-kind fail-fast — a
///      busy channel gets a terminal "shell busy" frame and the socket closes);
///   2. refuses if the serial link is down or the host-info cache is empty
///      (not yet handshook — AC6), writing a terminal frame + closing;
///   3. installs an `ExecSlotGuard` so `reader_thread` fans host shell-events
///      into our private mpsc;
///   4. **originates the single `ShellOpenPty { shell, cols, rows }`** — the
///      term does NOT send its own (plan-review Important #2);
///   5. runs two pumps until socket EOF / `ShellExit` / term `ShellClose` /
///      link-down:
///        * socket → wire (reader thread): `Hello` → synth `HelloAck` from the
///          cache (NOT forwarded); `Heartbeat` → dropped; `ShellInput` /
///          `PtyResize` → `outgoing_tx`; `ShellClose` / `Disconnect` → stop
///          (teardown sends the single host-side `ShellClose`);
///        * slot → socket (this thread): `ShellOutput` / `ShellExit` /
///          `HostError` → `Packet` → socket. Polls `link_up` each cycle; on
///          `false` writes a synth `Disconnect` and closes the socket so the
///          term's reader sees EOF and exits cleanly (AC6).
///
/// On teardown: send `ShellClose` to the host, drop the owner + slot guards
/// (channel → `Idle`, slot → `None`), close the socket. All guards are RAII so
/// a panic in either pump still releases the channel.
///
/// Dispatched from `dispatch_connection` on an `IpcConnect::Interactive` frame.
#[allow(clippy::too_many_arguments)]
fn handle_interactive_connection(
    stream: UnixStream,
    open: IpcInteractiveOpen,
    outgoing_tx: mpsc::Sender<Packet>,
    exec_slot: ExecEventSlot,
    shell_owner: SharedShellOwner,
    host_info: SharedHostInfo,
    link_up: Arc<AtomicBool>,
) {
    // 1. Claim the host's single shell slot exclusively. Busy (exec or another
    //    interactive session in flight) → fail-fast terminal frame + close. No
    //    queuing: a minutes-long interactive session must never block Claude.
    let _owner_guard = match try_acquire(&shell_owner, ShellOwner::Interactive) {
        Some(g) => g,
        None => {
            log::info!("IPC interactive: shell channel busy — refusing");
            let mut s = stream;
            let _ = write_packet_frame(&mut s, &relay_error_packet("shell busy"));
            return;
        }
    };

    // 2. Refuse if the link is mid-reconnect or we never handshook (empty
    //    host-info cache): we can't synth an accurate HelloAck and any
    //    ShellOpenPty we'd queue would block against a dead wire.
    let host_info_ready = host_info.lock().map(|g| g.is_some()).unwrap_or(false);
    if !link_up.load(Ordering::Relaxed) || !host_info_ready {
        log::info!("IPC interactive: link down or host-info empty — refusing");
        let mut s = stream;
        let _ = write_packet_frame(&mut s, &relay_error_packet("host link not ready"));
        return;
    }

    // 3. Private mpsc for the duration of the session; reader_thread fans host
    //    ShellOutput / ShellExit / HostError into it via the slot guard.
    let (event_tx, event_rx) = mpsc::channel::<ExecEvent>();
    let _slot_guard = ExecSlotGuard::install(&exec_slot, event_tx);

    // 4. Originate the single ShellOpenPty (the term does NOT send its own on
    //    the IPC path — plan-review Important #2). cols/rows come from the
    //    term's terminal::size() at connect time.
    if let Err(e) = outgoing_tx.send(Packet::new(
        Message::ShellOpenPty {
            shell: open.shell.clone(),
            cols: open.cols,
            rows: open.rows,
        },
        0,
    )) {
        log::warn!("IPC interactive: ShellOpenPty send failed: {e}; aborting");
        return;
    }

    // 5. Split the socket: the original fd reads (blocking); a clone behind a
    //    mutex serialises writes from both pumps (reader-thread synth HelloAck
    //    vs this thread's ShellOutput frames — concurrent write_all would
    //    interleave frame bytes otherwise).
    let read_stream = stream;
    let write_stream = match read_stream.try_clone() {
        Ok(s) => {
            // Bound socket writes so a wedged term (SIGSTOP'd `wd`, hung
            // terminal that stops draining) can't fill the kernel send buffer
            // and block the pump forever mid-`write_all` while holding the
            // owner guard — that would poison the shell channel (every later
            // `wd` refused "shell busy") and grow `event_rx` unbounded. On a
            // write timeout the pump returns Err, breaks, and teardown frees
            // the channel. 15s is far above any healthy local-socket write.
            let _ = s.set_write_timeout(Some(Duration::from_secs(15)));
            Arc::new(Mutex::new(s))
        }
        Err(e) => {
            log::warn!("IPC interactive: stream try_clone failed: {e}; aborting");
            return;
        }
    };

    let stop = Arc::new(AtomicBool::new(false));

    // Reader thread: socket → wire. Blocks on read_packet_frame; the teardown
    // below shuts the socket down to unblock it (avoids a mid-frame read-timeout
    // desync). It NEVER forwards ShellClose/Disconnect to the wire — teardown
    // emits the single host-side ShellClose.
    let reader = {
        let r_stop = stop.clone();
        let r_write = write_stream.clone();
        let r_outgoing = outgoing_tx.clone();
        let r_host_info = host_info.clone();
        let mut rs = read_stream;
        thread::spawn(move || {
            while !r_stop.load(Ordering::Relaxed) {
                match read_packet_frame(&mut rs) {
                    Ok(pkt) => match pkt.message {
                        Message::Hello { .. } => {
                            // Answer from the cache; never forward to the wire —
                            // the GUI already handshook with the host.
                            let ack = synth_hello_ack(&r_host_info);
                            if let Ok(mut w) = r_write.lock() {
                                let _ = write_packet_frame(&mut *w, &ack);
                            }
                        }
                        // GUI writer owns heartbeat on the real wire; drop the
                        // term's so we don't double it.
                        Message::Heartbeat => {}
                        Message::ShellClose | Message::Disconnect => {
                            r_stop.store(true, Ordering::Relaxed);
                            break;
                        }
                        Message::ShellInput { .. } | Message::PtyResize { .. } => {
                            if r_outgoing.send(pkt).is_err() {
                                r_stop.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                        // Any other message type from the term is unexpected on
                        // the interactive path — ignore rather than forward.
                        _ => {}
                    },
                    // EOF / socket shutdown / decode error — term is gone.
                    Err(_) => {
                        r_stop.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        })
    };

    // Main pump: slot → socket, plus link_up watchdog.
    loop {
        if !link_up.load(Ordering::Relaxed) {
            // Link went down mid-session (supervisor cleared it). Tell the term
            // with a synth Disconnect so its reader exits cleanly instead of
            // hanging on a wire that will never answer (AC6).
            log::info!("IPC interactive: link down mid-session — sending synth Disconnect");
            if let Ok(mut w) = write_stream.lock() {
                let _ = write_packet_frame(&mut *w, &Packet::new(Message::Disconnect, 0));
            }
            break;
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ExecEvent::ShellOutput(data)) => {
                if let Ok(mut w) = write_stream.lock() {
                    if write_packet_frame(&mut *w, &Packet::new(Message::ShellOutput { data }, 0))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Ok(ExecEvent::ShellExit(code)) => {
                if let Ok(mut w) = write_stream.lock() {
                    let _ = write_packet_frame(&mut *w, &Packet::new(Message::ShellExit { code }, 0));
                }
                break;
            }
            Ok(ExecEvent::HostError(msg)) => {
                if let Ok(mut w) = write_stream.lock() {
                    let _ = write_packet_frame(
                        &mut *w,
                        &Packet::new(Message::Error { code: 0, msg }, 0),
                    );
                }
            }
            Ok(ExecEvent::Idle) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Teardown. Signal + shut the socket down to unblock the blocking reader,
    // join it, then close the host shell so the next session can ShellOpen.
    stop.store(true, Ordering::Relaxed);
    if let Ok(w) = write_stream.lock() {
        let _ = w.shutdown(std::net::Shutdown::Both);
    }
    let _ = reader.join();
    if let Err(e) = outgoing_tx.send(Packet::new(Message::ShellClose, 0)) {
        log::warn!("IPC interactive: ShellClose send failed on teardown: {e}");
    }
    // `_owner_guard` (→ Idle) and `_slot_guard` (→ None) drop here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use wiredesk_exec_core::ipc::{read_request, write_connect, write_request, IpcRequest};

    #[test]
    fn ipc_exec_transport_send_input_pushes_packet() {
        let (tx, rx) = mpsc::channel::<Packet>();
        let (_event_tx, event_rx) = mpsc::channel::<ExecEvent>();
        let mut t = IpcExecTransport {
            outgoing_tx: tx,
            rx: event_rx,
        };
        t.send_input(b"hello").unwrap();
        let pkt = rx.recv().unwrap();
        match pkt.message {
            Message::ShellInput { data } => assert_eq!(data, b"hello"),
            other => panic!("expected ShellInput, got {other:?}"),
        }
    }

    #[test]
    fn ipc_exec_transport_recv_event_idle_on_timeout() {
        let (tx, _rx) = mpsc::channel::<Packet>();
        let (_event_tx, event_rx) = mpsc::channel::<ExecEvent>();
        let mut t = IpcExecTransport {
            outgoing_tx: tx,
            rx: event_rx,
        };
        // No event posted — recv_event returns Idle after the timeout.
        let ev = t.recv_event(Duration::from_millis(20)).unwrap();
        assert!(matches!(ev, ExecEvent::Idle));
    }

    #[test]
    fn ipc_exec_transport_recv_event_disconnect_to_closed() {
        let (tx, _rx) = mpsc::channel::<Packet>();
        let (event_tx, event_rx) = mpsc::channel::<ExecEvent>();
        drop(event_tx);
        let mut t = IpcExecTransport {
            outgoing_tx: tx,
            rx: event_rx,
        };
        let res = t.recv_event(Duration::from_millis(20));
        assert!(matches!(res, Err(ExecError::Closed)));
    }

    /// Bind succeeds, accept loop is alive, client connects + writes
    /// a Request + reads back a Stdout + Exit. We can't drive the
    /// runner end-to-end (it'd want a real serial transport), so we
    /// stage events directly into `exec_slot` and let the handler's
    /// runner consume them.
    #[test]
    fn handler_round_trip_via_unix_socket() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        spawn_ipc_acceptor(
            socket.clone(),
            outgoing_tx,
            exec_slot.clone(),
            owner,
            inflight,
            host_info,
            link_up,
        );

        // Give the acceptor a moment to bind before we connect.
        thread::sleep(Duration::from_millis(50));

        // Stage host-side events on a separate thread that fires after
        // the runner has sent its payload (we observe the outgoing_rx
        // channel for the ShellInput packet, then push events).
        let stage_slot = exec_slot.clone();
        let stage_thread = thread::spawn(move || {
            // Handler now sends ShellOpen first, then payload (a
            // ShellInput) — drain ShellOpen and keep reading until
            // we land on the ShellInput carrying the sentinel marker.
            let payload = loop {
                let pkt = outgoing_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("runner should send packets");
                if let Message::ShellInput { data } = pkt.message {
                    let s = String::from_utf8_lossy(&data).to_string();
                    if s.contains("__WD_DONE_") {
                        break s;
                    }
                }
                // ShellOpen / heartbeats / others — keep draining.
            };
            let marker = "__WD_DONE_";
            let start = payload.find(marker).expect("uuid") + marker.len();
            let after = &payload[start..];
            let end = after.find("__").unwrap();
            let uuid = &after[..end];

            // Stage: prompt → output → sentinel.
            let stage = |slot: &ExecEventSlot, ev: ExecEvent| {
                if let Some(tx) = slot.lock().unwrap().as_ref() {
                    let _ = tx.send(ev);
                }
            };
            stage(&stage_slot, ExecEvent::ShellOutput(b"PS C:\\>\n".to_vec()));
            stage(&stage_slot, ExecEvent::ShellOutput(b"hi\n".to_vec()));
            stage(
                &stage_slot,
                ExecEvent::ShellOutput(format!("__WD_DONE_{uuid}__0\n").into_bytes()),
            );
        });

        // Client side: connect, send request, read responses.
        let mut client = UnixStream::connect(&socket).expect("connect");
        let req = IpcRequest {
            cmd: "echo hi".into(),
            ssh: None,
            timeout_secs: 5,
            compress: false,
        };
        write_connect(&mut client, &IpcConnect::Exec(req)).unwrap();

        // Pull responses until Exit.
        let mut stdout_collected = Vec::new();
        let exit = loop {
            match wiredesk_exec_core::ipc::read_response(&mut client).unwrap() {
                IpcResponse::Stdout(b) => stdout_collected.extend_from_slice(&b),
                IpcResponse::Exit(c) => break c,
                IpcResponse::Error(m) => panic!("handler error: {m}"),
                IpcResponse::TransportUnavailable(m) => panic!("unexpected unavailable: {m}"),
            }
        };
        stage_thread.join().expect("stage thread");

        assert_eq!(exit, 0);
        let s = String::from_utf8(stdout_collected).unwrap();
        assert!(s.contains("hi\n"), "stdout streamed: {s:?}");
    }

    /// When the serial link is mid-reconnect (`link_up == false`), the
    /// handler must answer `TransportUnavailable` immediately and never
    /// touch `outgoing_tx` (no ShellOpen queued against a dead wire).
    #[test]
    fn handler_link_down_returns_transport_unavailable() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(false)); // link DOWN

        // Hold single_inflight for the whole test. The link-down refusal must
        // fire BEFORE the handler tries to acquire this lock — otherwise it
        // would block here forever. Proves the ordering the handler documents,
        // not just the "no packet queued" symptom.
        let _held = inflight.lock().unwrap();

        spawn_ipc_acceptor(
            socket.clone(),
            outgoing_tx,
            exec_slot,
            owner,
            inflight.clone(),
            host_info,
            link_up,
        );
        thread::sleep(Duration::from_millis(50));

        let mut client = UnixStream::connect(&socket).expect("connect");
        // Bound the read so a regression (lock acquired before the link check)
        // fails the test instead of hanging it.
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let req = IpcRequest {
            cmd: "echo hi".into(),
            ssh: None,
            timeout_secs: 5,
            compress: false,
        };
        write_connect(&mut client, &IpcConnect::Exec(req)).unwrap();

        match wiredesk_exec_core::ipc::read_response(&mut client)
            .expect("link-down refusal must arrive without acquiring single_inflight")
        {
            IpcResponse::TransportUnavailable(msg) => {
                assert!(msg.contains("reconnecting"), "msg: {msg}");
            }
            other => panic!("expected TransportUnavailable, got {other:?}"),
        }

        // Handler must NOT have queued any packet (no ShellOpen against
        // a dead wire).
        assert!(
            outgoing_rx.try_recv().is_err(),
            "handler must not send any outgoing packet when link is down"
        );
    }

    #[test]
    fn bind_failure_does_not_panic() {
        // Pass a path inside a non-existent root that we can't create
        // (use a regular file as the parent directory — mkdir will
        // fail with ENOTDIR). spawn_ipc_acceptor should log warn and
        // return without panicking.
        use tempfile::TempDir;
        let tmp = TempDir::new().expect("tempdir");
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"file, not a dir").unwrap();
        let socket = blocker.join("wd-exec.sock");

        let (tx, _rx) = mpsc::channel::<Packet>();
        let slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info: SharedHostInfo = Arc::new(Mutex::new(None));
        let link_up = Arc::new(AtomicBool::new(true));

        // Must not panic.
        spawn_ipc_acceptor(socket, tx, slot, owner, inflight, host_info, link_up);
    }

    #[test]
    fn stale_socket_unlinked_before_bind() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");
        std::fs::write(&socket, b"stale leftover").unwrap();
        assert!(socket.exists(), "stale file present");

        let (tx, _rx) = mpsc::channel::<Packet>();
        let slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info: SharedHostInfo = Arc::new(Mutex::new(None));
        let link_up = Arc::new(AtomicBool::new(true));
        spawn_ipc_acceptor(socket.clone(), tx, slot, owner, inflight, host_info, link_up);
        thread::sleep(Duration::from_millis(50));

        // Now it should be a real socket — connect should succeed.
        let res = UnixStream::connect(&socket);
        assert!(res.is_ok(), "stale-unlink + bind should leave a working socket: {res:?}");
    }

    #[test]
    fn ipc_handler_extracts_compress_field() {
        // Codec smoke only: an IpcRequest with compress=true survives a
        // bincode write/read round-trip. This does NOT drive the handler or
        // assert the flag reaches run_oneshot — that forwarding is direct
        // field access (req.compress), enforced by compilation.
        use std::io::Cursor;

        let req = IpcRequest {
            cmd: "echo hi".into(),
            ssh: None,
            timeout_secs: 5,
            compress: true,
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let mut r = Cursor::new(buf);
        let decoded = read_request(&mut r).unwrap();
        assert!(decoded.compress, "handler-side decode preserves compress flag");
        assert_eq!(decoded.cmd, "echo hi");
    }

    // ---- Interactive relay (Task 6) -------------------------------------

    use crate::link::{HostInfo, SharedHostInfo};
    use crate::shell_channel::new_shared_owner;

    fn populated_host_info() -> SharedHostInfo {
        Arc::new(Mutex::new(Some(HostInfo {
            host_name: "win-host".into(),
            screen_w: 2560,
            screen_h: 1440,
        })))
    }

    /// Spin until the interactive handler has installed its exec slot (it does
    /// so before originating ShellOpenPty, but the handler runs on its own
    /// thread so we poll to avoid a race in the staging tests).
    fn wait_slot_installed(slot: &ExecEventSlot) {
        for _ in 0..400 {
            if slot.lock().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("exec slot never installed by interactive handler");
    }

    fn stage_event(slot: &ExecEventSlot, ev: ExecEvent) {
        let guard = slot.lock().unwrap();
        let tx = guard.as_ref().expect("slot must be installed before staging");
        tx.send(ev).expect("stage into installed slot");
    }

    #[test]
    fn interactive_hello_synth_ack_and_forwards_input() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        let open = IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 100,
            rows: 30,
        };
        let (h_slot, h_owner, h_hi, h_link) =
            (exec_slot.clone(), owner.clone(), host_info.clone(), link_up.clone());
        let handler = thread::spawn(move || {
            handle_interactive_connection(server, open, outgoing_tx, h_slot, h_owner, h_hi, h_link);
        });

        // (4) The relay originates the single ShellOpenPty — the term sends none.
        let first = outgoing_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("relay must originate ShellOpenPty");
        match first.message {
            Message::ShellOpenPty { shell, cols, rows } => {
                assert_eq!(shell, "pwsh");
                assert_eq!(cols, 100);
                assert_eq!(rows, 30);
            }
            other => panic!("expected ShellOpenPty first, got {other:?}"),
        }
        assert_eq!(*owner.lock().unwrap(), ShellOwner::Interactive);

        // Hello → synth HelloAck from the cache, NOT forwarded to the wire.
        write_packet_frame(
            &mut client,
            &Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "mac-term".into(),
                },
                0,
            ),
        )
        .unwrap();
        match read_packet_frame(&mut client).expect("HelloAck").message {
            Message::HelloAck {
                host_name,
                screen_w,
                screen_h,
                ..
            } => {
                assert_eq!(host_name, "win-host");
                assert_eq!(screen_w, 2560);
                assert_eq!(screen_h, 1440);
            }
            other => panic!("expected synth HelloAck, got {other:?}"),
        }

        // Heartbeat dropped; ShellInput + PtyResize forwarded to the wire.
        write_packet_frame(&mut client, &Packet::new(Message::Heartbeat, 0)).unwrap();
        write_packet_frame(
            &mut client,
            &Packet::new(Message::ShellInput { data: b"ls\r".to_vec() }, 0),
        )
        .unwrap();
        write_packet_frame(
            &mut client,
            &Packet::new(Message::PtyResize { cols: 80, rows: 24 }, 0),
        )
        .unwrap();

        // Next wire packet is ShellInput — Hello + Heartbeat were NOT forwarded.
        match outgoing_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("forwarded ShellInput")
            .message
        {
            Message::ShellInput { data } => assert_eq!(data, b"ls\r"),
            other => panic!("expected forwarded ShellInput, got {other:?}"),
        }
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("forwarded PtyResize")
                .message,
            Message::PtyResize { cols: 80, rows: 24 }
        ));

        // Staged host ShellOutput / ShellExit reach the socket.
        wait_slot_installed(&exec_slot);
        stage_event(&exec_slot, ExecEvent::ShellOutput(b"hi\n".to_vec()));
        match read_packet_frame(&mut client).expect("ShellOutput").message {
            Message::ShellOutput { data } => assert_eq!(data, b"hi\n"),
            other => panic!("expected ShellOutput, got {other:?}"),
        }
        stage_event(&exec_slot, ExecEvent::ShellExit(0));
        assert!(matches!(
            read_packet_frame(&mut client).expect("ShellExit").message,
            Message::ShellExit { code: 0 }
        ));

        handler.join().expect("handler thread");
        // Teardown: single host-side ShellClose + owner released.
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("teardown ShellClose")
                .message,
            Message::ShellClose
        ));
        assert_eq!(*owner.lock().unwrap(), ShellOwner::Idle);
    }

    #[test]
    fn interactive_refused_when_link_down() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(false)); // link DOWN

        let open = IpcInteractiveOpen {
            shell: String::new(),
            cols: 80,
            rows: 24,
        };
        let owner_probe = owner.clone();
        let handler = thread::spawn(move || {
            handle_interactive_connection(
                server, open, outgoing_tx, exec_slot, owner, host_info, link_up,
            );
        });

        match read_packet_frame(&mut client).expect("refuse frame").message {
            Message::Error { code, .. } => assert_eq!(code, RELAY_REFUSE_CODE),
            other => panic!("expected Error refuse frame, got {other:?}"),
        }
        handler.join().unwrap();
        assert!(
            outgoing_rx.try_recv().is_err(),
            "no ShellOpenPty may be queued when the link is down"
        );
        assert_eq!(
            *owner_probe.lock().unwrap(),
            ShellOwner::Idle,
            "refused connect must release the channel"
        );
    }

    #[test]
    fn interactive_refused_when_host_info_empty() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let host_info: SharedHostInfo = Arc::new(Mutex::new(None)); // never handshook
        let link_up = Arc::new(AtomicBool::new(true));

        let open = IpcInteractiveOpen {
            shell: String::new(),
            cols: 80,
            rows: 24,
        };
        let owner_probe = owner.clone();
        let handler = thread::spawn(move || {
            handle_interactive_connection(
                server, open, outgoing_tx, exec_slot, owner, host_info, link_up,
            );
        });

        assert!(matches!(
            read_packet_frame(&mut client).expect("refuse frame").message,
            Message::Error { .. }
        ));
        handler.join().unwrap();
        assert!(
            outgoing_rx.try_recv().is_err(),
            "no ShellOpenPty may be queued before the first HelloAck"
        );
        // This branch acquires the owner guard *before* the host-info check, so
        // a guard leak here would strand the channel — assert it released.
        assert_eq!(
            *owner_probe.lock().unwrap(),
            ShellOwner::Idle,
            "refused connect must release the channel"
        );
    }

    #[test]
    fn interactive_refused_when_channel_busy() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        // Pre-claim as Exec — a competing interactive connect must fail fast.
        let _held = try_acquire(&owner, ShellOwner::Exec).expect("pre-claim Exec");
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        let open = IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 80,
            rows: 24,
        };
        let handler = thread::spawn(move || {
            handle_interactive_connection(
                server,
                open,
                outgoing_tx,
                exec_slot,
                owner.clone(),
                host_info,
                link_up,
            );
        });

        match read_packet_frame(&mut client).expect("busy frame").message {
            Message::Error { msg, .. } => assert!(msg.contains("busy"), "msg: {msg}"),
            other => panic!("expected 'shell busy' Error, got {other:?}"),
        }
        handler.join().unwrap();
        assert!(
            outgoing_rx.try_recv().is_err(),
            "no ShellOpenPty may be queued when the channel is busy"
        );
    }

    #[test]
    fn interactive_link_down_midsession_sends_disconnect() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        let open = IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 80,
            rows: 24,
        };
        let link_probe = link_up.clone();
        let handler = thread::spawn(move || {
            handle_interactive_connection(
                server, open, outgoing_tx, exec_slot, owner, host_info, link_up,
            );
        });

        // Session established: the relay originated ShellOpenPty.
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("ShellOpenPty")
                .message,
            Message::ShellOpenPty { .. }
        ));

        // Link drops mid-session → the relay must synth a Disconnect so the
        // term's reader sees a clean end instead of hanging (AC6).
        link_probe.store(false, Ordering::Relaxed);
        assert!(matches!(
            read_packet_frame(&mut client)
                .expect("synth Disconnect on link-down")
                .message,
            Message::Disconnect
        ));
        handler.join().unwrap();
    }

    // ---- Atomic IpcConnect cutover (Task 7) -----------------------------

    /// While an interactive `wd` session holds the shell channel, an incoming
    /// `wd --exec` must fail fast with a transport-class frame (term → exit 125)
    /// and never queue a `ShellOpen` behind the minutes-long PTY session.
    #[test]
    fn exec_refused_when_interactive_holds_channel() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        // Pre-claim the channel as Interactive — mirrors a live PTY session.
        let _held = try_acquire(&owner, ShellOwner::Interactive).expect("pre-claim Interactive");
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        spawn_ipc_acceptor(
            socket.clone(),
            outgoing_tx,
            exec_slot,
            owner.clone(),
            inflight,
            host_info,
            link_up,
        );
        thread::sleep(Duration::from_millis(50));

        let mut client = UnixStream::connect(&socket).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let req = IpcRequest {
            cmd: "echo hi".into(),
            ssh: None,
            timeout_secs: 5,
            compress: false,
        };
        write_connect(&mut client, &IpcConnect::Exec(req)).unwrap();

        // The handler emits an empty keepalive Stdout before acquiring the slot;
        // then the owner-held refusal arrives as TransportUnavailable.
        let terminal = loop {
            match wiredesk_exec_core::ipc::read_response(&mut client)
                .expect("owner-held refusal must arrive without hanging")
            {
                IpcResponse::Stdout(_) => continue, // keepalive
                other => break other,
            }
        };
        match terminal {
            IpcResponse::TransportUnavailable(msg) => {
                assert!(msg.contains("busy"), "msg: {msg}");
            }
            other => panic!("expected TransportUnavailable (shell busy), got {other:?}"),
        }

        // No ShellOpen may be queued against a channel the interactive session owns.
        assert!(
            outgoing_rx.try_recv().is_err(),
            "no outgoing packet may be queued when interactive holds the channel"
        );
        assert_eq!(
            *owner.lock().unwrap(),
            ShellOwner::Interactive,
            "the refused exec must NOT disturb the interactive owner state"
        );
    }

    /// Two `wd --exec` calls racing into one acceptor must both complete (exit 0)
    /// — the `Exec` owner claim is nested UNDER `single_inflight`, so the second
    /// exec blocks on the FIFO mutex rather than seeing a false "shell busy". A
    /// regression that acquired the owner *before* `single_inflight` would make
    /// the second concurrent exec fail fast; this guards that ordering.
    #[test]
    fn concurrent_exec_fifo_no_false_busy() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        spawn_ipc_acceptor(
            socket.clone(),
            outgoing_tx,
            exec_slot.clone(),
            owner.clone(),
            inflight,
            host_info,
            link_up,
        );
        thread::sleep(Duration::from_millis(50));

        // Staging thread: handlers serialise via single_inflight, so sentinels
        // appear one run at a time. For each of the two runs, drain outgoing
        // until the ShellInput carrying the sentinel, extract its uuid, then
        // stage prompt → output → sentinel into whichever slot is installed.
        let stage_slot = exec_slot.clone();
        let stage_thread = thread::spawn(move || {
            for _ in 0..2 {
                let payload = loop {
                    let pkt = match outgoing_rx.recv_timeout(Duration::from_secs(10)) {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    if let Message::ShellInput { data } = pkt.message {
                        let s = String::from_utf8_lossy(&data).to_string();
                        if s.contains("__WD_DONE_") {
                            break s;
                        }
                    }
                };
                let marker = "__WD_DONE_";
                let start = payload.find(marker).expect("uuid") + marker.len();
                let after = &payload[start..];
                let end = after.find("__").unwrap();
                let uuid = after[..end].to_string();

                // The slot is reinstalled by each handler; wait for it.
                let mut installed = false;
                for _ in 0..400 {
                    if stage_slot.lock().unwrap().is_some() {
                        installed = true;
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                if !installed {
                    return;
                }
                let stage = |ev: ExecEvent| {
                    if let Some(tx) = stage_slot.lock().unwrap().as_ref() {
                        let _ = tx.send(ev);
                    }
                };
                stage(ExecEvent::ShellOutput(b"PS C:\\>\n".to_vec()));
                stage(ExecEvent::ShellOutput(b"hi\n".to_vec()));
                stage(ExecEvent::ShellOutput(
                    format!("__WD_DONE_{uuid}__0\n").into_bytes(),
                ));
            }
        });

        // Two concurrent exec clients.
        let run_client = |socket: PathBuf| {
            let mut client = UnixStream::connect(&socket).expect("connect");
            client
                .set_read_timeout(Some(Duration::from_secs(15)))
                .unwrap();
            let req = IpcRequest {
                cmd: "echo hi".into(),
                ssh: None,
                timeout_secs: 10,
                compress: false,
            };
            write_connect(&mut client, &IpcConnect::Exec(req)).unwrap();
            loop {
                match wiredesk_exec_core::ipc::read_response(&mut client).unwrap() {
                    IpcResponse::Stdout(_) => {}
                    IpcResponse::Exit(c) => break c,
                    IpcResponse::TransportUnavailable(m) => {
                        panic!("concurrent exec falsely reported busy: {m}")
                    }
                    IpcResponse::Error(m) => panic!("handler error: {m}"),
                }
            }
        };
        let s1 = socket.clone();
        let s2 = socket.clone();
        let c1 = thread::spawn(move || run_client(s1));
        let c2 = thread::spawn(move || run_client(s2));

        let e1 = c1.join().expect("client 1");
        let e2 = c2.join().expect("client 2");
        stage_thread.join().expect("stage thread");
        assert_eq!(e1, 0, "first exec exit code");
        assert_eq!(e2, 0, "second exec exit code");

        // The handler holds the owner guard through its post-run drain (a couple
        // seconds after the client already saw Exit), so poll for the reset
        // rather than asserting immediately.
        let mut released = false;
        for _ in 0..600 {
            if *owner.lock().unwrap() == ShellOwner::Idle {
                released = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            released,
            "channel must return to Idle after both exec runs (owner still {:?})",
            *owner.lock().unwrap()
        );
    }
}
