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
use wiredesk_protocol::message::{pty_slot_supported, Message};
use wiredesk_protocol::packet::Packet;

use crate::exec_bridge::{ExecSlotGuard, ShellSlots};
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
        // Wire-sized pieces: a single oversize packet is refused by the
        // writer and the run hangs to timeout (see `shell_input_packets`).
        for packet in wiredesk_exec_core::transport::shell_input_packets(data) {
            self.outgoing_tx
                .send(packet)
                .map_err(|_| ExecError::Closed)?;
        }
        Ok(())
    }

    fn recv_event(&mut self, timeout: Duration) -> Result<ExecEvent, ExecError> {
        match self.rx.recv_timeout(timeout) {
            Ok(ev) => Ok(ev),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(ExecEvent::Idle),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ExecError::Closed),
        }
    }
}

/// How much of a `wd --exec` command line the INFO log keeps. Agent sessions
/// routinely pass 4–7 KB base64 blobs — logging them whole bloated the file
/// and copied every DB host, user and query into it. The full text stays in
/// the caller's own shell history.
const CMD_LOG_CHARS: usize = 160;

/// Truncate `cmd` for logging on a char boundary (commands carry Cyrillic),
/// appending the omitted length so a long one is still recognisable.
fn abbreviate_cmd(cmd: &str) -> String {
    let total = cmd.chars().count();
    if total <= CMD_LOG_CHARS {
        return format!("{cmd:?}");
    }
    let head: String = cmd.chars().take(CMD_LOG_CHARS).collect();
    format!("{head:?}… (+{} chars)", total - CMD_LOG_CHARS)
}

/// Narrow the socket's directory to 0700. Best-effort: a failure is not
/// fatal, the 0600 on the socket itself still applies a moment later.
///
/// Split out from `spawn_ipc_acceptor` so it can be tested without starting
/// an acceptor thread — that thread outlives the test, and once the test's
/// temp dir is removed its listener would return errors forever.
fn narrow_socket_dir(dir: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(dir) else {
        return;
    };
    let mut perms = meta.permissions();
    if perms.mode() & 0o077 == 0 {
        return;
    }
    perms.set_mode(0o700);
    if let Err(e) = std::fs::set_permissions(dir, perms) {
        log::warn!("IPC acceptor: chmod 0700 on {} failed: {e}", dir.display());
    }
}

/// How long to wait after a failed `accept` before trying again, and how
/// many consecutive failures to tolerate before concluding the socket is
/// gone for good. 10 × 200 ms ≈ 2 s of a genuinely broken listener.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(200);
const MAX_ACCEPT_ERRORS: u32 = 10;

/// Bind the Unix socket and spawn an acceptor thread. Failure to bind
/// (missing parent dir, EADDRINUSE race, permission denied) logs a
/// warning and returns — GUI continues without IPC, term's
/// `try_socket_first` will fall back to direct serial.
#[allow(clippy::too_many_arguments)]
pub fn spawn_ipc_acceptor(
    socket_path: PathBuf,
    outgoing_tx: mpsc::Sender<Packet>,
    slots: ShellSlots,
    shell_owner: SharedShellOwner,
    single_inflight: Arc<Mutex<()>>,
    host_info: SharedHostInfo,
    link_up: Arc<AtomicBool>,
) {
    // Stale socket from prior crash — `bind` fails with EADDRINUSE
    // unless we unlink first. Ignore not-found.
    let _ = std::fs::remove_file(&socket_path);

    // Ensure parent dir exists. If it doesn't, create it (config.toml
    // save creates it on first run, but a fresh install hitting IPC
    // before settings save would see ENOENT).
    //
    // The dir is then narrowed to 0700. This is what actually closes the
    // window between `bind` below and the `chmod 0600` after it: for the
    // few microseconds the socket exists with umask-derived perms, the
    // directory above it already denies traversal to everyone else. Doing
    // it with umask instead would need a libc dependency and would race
    // against other threads, since umask is process-global.
    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            log::warn!(
                "IPC acceptor: failed to create parent dir {}: {e}; wd --exec will use direct serial fallback",
                parent.display()
            );
            return;
        }
        narrow_socket_dir(parent);
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
    if let Ok(meta) = listener
        .local_addr()
        .and_then(|_| std::fs::metadata(&socket_path))
    {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        if let Err(e) = std::fs::set_permissions(&socket_path, perms) {
            log::warn!("IPC chmod 0600 failed: {e}");
        }
    }

    log::info!("IPC acceptor listening at {}", socket_path.display());

    thread::spawn(move || {
        let mut consecutive_accept_errors: u32 = 0;
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    consecutive_accept_errors = 0;
                    log::info!("IPC connection accepted");
                    let outgoing_tx = outgoing_tx.clone();
                    let slots = slots.clone();
                    let shell_owner = shell_owner.clone();
                    let single_inflight = single_inflight.clone();
                    let host_info = host_info.clone();
                    let link_up = link_up.clone();
                    thread::spawn(move || {
                        dispatch_connection(
                            stream,
                            outgoing_tx,
                            slots,
                            shell_owner,
                            single_inflight,
                            host_info,
                            link_up,
                        );
                    });
                }
                Err(e) => {
                    // A transient accept error (EINTR, a client that hung up
                    // between connect and accept) is worth retrying. A
                    // permanent one is not: if the socket is gone — its
                    // directory removed, the fd exhausted — `incoming()`
                    // returns Err immediately, forever, and an unpaced loop
                    // here burns a core and floods the log. Back off, and
                    // give up once it is clearly not coming back; without a
                    // socket the acceptor has nothing left to do, and `wd`
                    // falls back to direct serial on its own.
                    consecutive_accept_errors += 1;
                    log::warn!(
                        "IPC accept error ({consecutive_accept_errors}/{MAX_ACCEPT_ERRORS}): {e}"
                    );
                    if consecutive_accept_errors >= MAX_ACCEPT_ERRORS {
                        log::error!(
                            "IPC acceptor giving up after {MAX_ACCEPT_ERRORS} consecutive \
                             accept errors; wd will use direct serial fallback"
                        );
                        return;
                    }
                    thread::sleep(ACCEPT_ERROR_BACKOFF);
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
    slots: ShellSlots,
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
            slots,
            shell_owner,
            single_inflight,
            host_info,
            link_up,
        ),
        IpcConnect::Interactive(open) => handle_interactive_connection(
            stream,
            open,
            outgoing_tx,
            slots,
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
    slots: ShellSlots,
    shell_owner: SharedShellOwner,
    single_inflight: Arc<Mutex<()>>,
    host_info: SharedHostInfo,
    link_up: Arc<AtomicBool>,
) {
    log::info!(
        "IPC handler: cmd={} ssh={:?} timeout={}s",
        abbreviate_cmd(&req.cmd),
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

    // Claim the shell channel as `Exec` BEFORE queuing on `single_inflight`
    // (Codex P2). Exec claims stack (ref-counted), so a queued exec is counted
    // as "exec present" for the whole time it waits — an interactive session
    // can't slip into the A→B handoff window and force a false "shell busy" on
    // the already-queued exec. Against a host with the dedicated pty slot a
    // live console doesn't block this at all; against an older one it fails
    // fast (term maps the transport-class frame → exit 125).
    // Declared BEFORE `_inflight_guard` so on return the inflight mutex releases
    // first (waking the next queued exec, which already holds its own Exec ref)
    // and only then does this exec's ref drop — the count never dips to 0 across
    // the handoff.
    let dual = host_supports_pty_slot(&host_info);
    let _owner_guard = match try_acquire(&shell_owner, ShellOwner::Exec, dual) {
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
        // Ordinary queueing: the host has one shell slot, and several
        // agent sessions routinely fire `wd --exec` at once. Three weeks of
        // logs held ~950 of these, nearly all plain back-to-back runs — so
        // INFO, not WARN. A genuinely stuck predecessor shows up as its own
        // "timeout after Ns" line.
        log::info!("IPC handler: queued {waited:?} behind another wd --exec (one host shell slot)");
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

    // Private mpsc for the duration of this run. Reader thread fans
    // ShellOutput / ShellExit / shell-Error into here via the slot
    // guard; runner pulls them as `ExecEvent`s.
    let (event_tx, event_rx) = mpsc::channel::<ExecEvent>();
    let _slot_guard = ExecSlotGuard::install(&slots.exec, event_tx);

    // Open a fresh pipe-mode shell on the host. Standalone term does
    // this in `run()` before calling run_oneshot; in IPC mode the
    // handler owns the lifecycle so we send ShellOpen here and a
    // matching ShellClose after the run. Without this, host shell
    // slot is empty and our `ShellInput` packets get ignored — what
    // the user saw as "GUI IPC unresponsive (no first frame in 2s)".
    if let Err(e) = outgoing_tx.send(Packet::new(
        Message::ShellOpen {
            shell: String::new(),
        },
        0,
    )) {
        log::warn!("IPC: failed to send ShellOpen: {e}; aborting handler");
        let _ = write_response(
            &mut stream,
            &IpcResponse::Error(format!("ShellOpen send: {e}")),
        );
        return;
    }

    // Throw away anything already queued from before this handler existed,
    // then send the command immediately. Each drained event is silently
    // discarded.
    //
    // This step used to *wait*: a flat 500 ms on every `wd --exec`, later a
    // 120 ms quiet window, and that wait was most of what a command cost -
    // measured live 2026-09-11, the shell plus the command itself came to
    // ~170 ms, so the pause was the largest single item in the budget.
    //
    // Waiting is unnecessary because the payload marks its own beginning.
    // Every wrapper `format_command` builds now opens with the READY
    // marker, and the runner drops every line up to it, so PowerShell
    // startup noise and a stray prompt are discarded by construction
    // rather than by having been timed out first. Ordering is safe too:
    // `ShellOpen` and `ShellInput` travel the same ordered stream and the
    // host spawns the shell inside the `ShellOpen` arm, so the input can
    // never arrive at a host that has no shell yet.
    while let Ok(_ev) = event_rx.try_recv() {}

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
            if let Err(e) = write_response(&mut chunk_stream, &IpcResponse::Stdout(chunk.to_vec()))
            {
                log::debug!("IPC: client write failed mid-stream: {e}");
            }
        },
    );

    // Final terminal frame on the original stream. Client may have
    // already disconnected — that's fine, we still complete cleanly
    // so the inflight guard unlocks.
    let sentinel_seen = result.is_ok();
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
    // Strategy: poll for events with an idle deadline. Each event received
    // resets the deadline; ShellExit short-circuits. Once nothing has
    // arrived for a whole deadline we treat the wire as quiet and return.
    // Hard cap at SHELL_KILL_GRACE_MAX so a host that never emits ShellExit
    // can't hold the next client hostage indefinitely.
    //
    // The deadline depends on how the run ended, because that is what says
    // whether leftovers are plausible. A run that reached its sentinel is
    // done by construction - the sentinel is the last thing the payload
    // prints - so the only thing that can still arrive is a prompt
    // fragment, and a short wait settles it. A run that timed out or died
    // on a transport error is the dangerous one (the 407 KB case above),
    // and keeps the full budget.
    //
    // This is pure latency for anything scripted, so it is worth keeping
    // short: a successful run used to pay the whole 2 s and the next command
    // queued behind it (`queued 2.0s behind another wd --exec` in the client
    // log). Since 2026-09-11 the host answers `ShellClose` with a
    // `ShellExit`, so the usual outcome is one round trip and the idle
    // budget below is only the fallback for a host that predates that.
    let post_run_idle = post_run_idle(sentinel_seen);
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
        match transport.rx.recv_timeout(post_run_idle) {
            // The acknowledgement we are actually waiting for. A real
            // `ShellExit` also ends the wait: the shell died on its own and
            // there is nothing left to come.
            Ok(wiredesk_exec_core::ExecEvent::ShellClosed)
            | Ok(wiredesk_exec_core::ExecEvent::ShellExit(_)) => {
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
            "IPC: post-cleanup drain: events_drained={} shell_exit={} idle_budget={:?} elapsed={:?}",
            drained_events,
            got_exit,
            post_run_idle,
            drain_started.elapsed()
        );
    }
}

/// Idle budget the post-run drain waits out before declaring the wire
/// quiet. See the call site for why the two cases differ.
fn post_run_idle(sentinel_seen: bool) -> Duration {
    if sentinel_seen {
        // Only reached against a host that doesn't answer `ShellClose`;
        // a current one short-circuits this on its `ShellClosed`. Long enough
        // to swallow a trailing prompt fragment on a link whose worst
        // measured packet latency is ~85 ms (Bluetooth Classic, 2026-09-11),
        // short enough not to be felt between commands. Late leftovers are
        // not lost either way: the next command drains its own quiet window
        // before it starts.
        Duration::from_millis(150)
    } else {
        Duration::from_secs(2)
    }
}

/// Whether the connected host has the dedicated pty slot, i.e. whether the
/// `Pty*` opcodes may go on the wire and the two shell kinds may run at once.
///
/// Read **once per connection**, after the link/host-info gate, and held for
/// the whole session: a reconnect to a differently-built host mid-session would
/// otherwise flip the opcodes under a live PTY. No host info (not handshook
/// yet) reads as `false` — the strict, always-safe answer.
fn host_supports_pty_slot(host_info: &SharedHostInfo) -> bool {
    host_info
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|hi| pty_slot_supported(hi.proto_version)))
        .unwrap_or(false)
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
///      into our private mpsc (before the PTY opens, so no output is missed);
///   4. **handshakes synchronously**: reads the term's `Hello` and answers with a
///      synth `HelloAck` from the cached host-info (NOT forwarded to the wire —
///      the GUI already handshook), under a bounded read timeout;
///   5. **only then originates the single `ShellOpenPty { shell, cols, rows }`**
///      (the term sends none — plan-review Important #2). Opening after the
///      HelloAck guarantees host startup output can't race ahead of it and be
///      dropped by the term's handshake loop (Codex P2);
///   6. runs two pumps until socket EOF / `ShellExit` / term `ShellClose` /
///      link-down:
///        * socket → wire (reader thread): `Hello` (stray) / `Heartbeat` →
///          dropped; `ShellInput` / `PtyResize` → `outgoing_tx`; `ShellClose` /
///          `Disconnect` → stop (teardown sends the single host-side `ShellClose`);
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
    mut stream: UnixStream,
    open: IpcInteractiveOpen,
    outgoing_tx: mpsc::Sender<Packet>,
    slots: ShellSlots,
    shell_owner: SharedShellOwner,
    host_info: SharedHostInfo,
    link_up: Arc<AtomicBool>,
) {
    // 1. Refuse if the link is mid-reconnect or we never handshook (empty
    //    host-info cache): we can't synth an accurate HelloAck and any open
    //    we'd queue would block against a dead wire.
    //
    //    This runs before the channel claim, because the claim's own answer
    //    now depends on the host generation — which is exactly what the
    //    host-info cache holds. Claiming first would mean claiming under a
    //    guess and releasing it a line later anyway.
    let host_info_ready = host_info.lock().map(|g| g.is_some()).unwrap_or(false);
    if !link_up.load(Ordering::Relaxed) || !host_info_ready {
        log::info!("IPC interactive: link down or host-info empty — refusing");
        let mut s = stream;
        let _ = write_packet_frame(&mut s, &relay_error_packet("host link not ready"));
        return;
    }

    // 2. Which dialect this host speaks, fixed for the whole session.
    let dual = host_supports_pty_slot(&host_info);

    // 3. Claim the console. A second interactive session always loses — there
    //    is one pty slot on the host either way. Against a legacy host a
    //    running `wd --exec` also blocks it, since there is only the one slot
    //    over there. Fail-fast terminal frame + close; no queuing, because a
    //    minutes-long console must never sit behind anything.
    let _owner_guard = match try_acquire(&shell_owner, ShellOwner::Interactive, dual) {
        Some(g) => g,
        None => {
            log::info!("IPC interactive: shell channel busy — refusing");
            let mut s = stream;
            let _ = write_packet_frame(&mut s, &relay_error_packet("shell busy"));
            return;
        }
    };

    // 4. Private mpsc for the duration of the session; the reader fans this
    //    console's output into it via the slot guard. Installed before the PTY
    //    opens so no host startup output is missed. Which slot depends on the
    //    dialect: a dual host answers on `Pty*`, a legacy one on `Shell*`.
    let (event_tx, event_rx) = mpsc::channel::<ExecEvent>();
    let _slot_guard = ExecSlotGuard::install(if dual { &slots.pty } else { &slots.exec }, event_tx);

    // 4. Synchronous handshake BEFORE opening the PTY (Codex P2): read the term's
    //    Hello and answer with a synth HelloAck from the cached host-info. Opening
    //    the PTY only *after* the HelloAck is on the wire guarantees host startup
    //    output (PowerShell banner/prompt) can never race ahead of the HelloAck
    //    and get discarded by the term's handshake loop (which ignores
    //    non-HelloAck frames). A bounded read timeout keeps a silent/crashed
    //    client from stranding the owner guard; on any failure we bail and
    //    teardown frees the channel.
    if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
        log::warn!("IPC interactive: handshake set_read_timeout failed: {e}; aborting");
        return;
    }
    match read_packet_frame(&mut stream) {
        Ok(pkt) if matches!(pkt.message, Message::Hello { .. }) => {
            let ack = synth_hello_ack(&host_info);
            if let Err(e) = write_packet_frame(&mut stream, &ack) {
                log::warn!("IPC interactive: HelloAck write failed: {e}; aborting");
                return;
            }
        }
        Ok(other) => {
            log::warn!(
                "IPC interactive: expected Hello first, got {:?}; aborting",
                other.message
            );
            return;
        }
        Err(e) => {
            log::info!("IPC interactive: no Hello within handshake window: {e}; aborting");
            return;
        }
    }
    // Restore blocking reads for the async reader thread below (teardown unblocks
    // it via socket shutdown, not a timeout).
    let _ = stream.set_read_timeout(None);

    // 6. NOW originate the single open — any host output arrives strictly
    //    after the HelloAck (the term does NOT send its own; plan-review
    //    Important #2). cols/rows come from the term's terminal::size().
    //
    //    `PtyOpen` takes the host's dedicated slot and leaves `wd --exec`
    //    alone; `ShellOpenPty` is the legacy open a pre-two-slot host
    //    understands, and it takes the whole shell side over there.
    let open_msg = if dual {
        Message::PtyOpen {
            shell: open.shell.clone(),
            cols: open.cols,
            rows: open.rows,
        }
    } else {
        Message::ShellOpenPty {
            shell: open.shell.clone(),
            cols: open.cols,
            rows: open.rows,
        }
    };
    if let Err(e) = outgoing_tx.send(Packet::new(open_msg, 0)) {
        log::warn!("IPC interactive: open send failed: {e}; aborting");
        return;
    }

    // 6. Split the socket: the original fd is read (blocking) by the reader
    //    thread; a write clone is used by this thread's main pump. The Hello
    //    handshake above already ran synchronously on `stream`, so the reader no
    //    longer writes to the socket — only the main pump does; the Mutex wrapper
    //    is retained as a simple owned write handle.
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
        let r_outgoing = outgoing_tx.clone();
        let r_dual = dual;
        let mut rs = read_stream;
        thread::spawn(move || {
            while !r_stop.load(Ordering::Relaxed) {
                match read_packet_frame(&mut rs) {
                    // The term speaks one dialect — the pre-two-slot one — and
                    // never learns otherwise (it is unchanged by this feature).
                    // Against a dual host its stdin is re-addressed here, so
                    // the console's keystrokes ride `PtyInput` and can never be
                    // confused with an exec run's `ShellInput`. `PtyResize` is
                    // shared by both generations and goes as-is.
                    Ok(pkt) => {
                        let pkt = if r_dual {
                            let seq = pkt.seq;
                            match pkt.message {
                                Message::ShellInput { data } => {
                                    Packet::new(Message::PtyInput { data }, seq)
                                }
                                // Rebuilt rather than forwarded, but with the
                                // same seq — nothing downstream reads it, and
                                // an altered one would only confuse a log.
                                message => Packet::new(message, seq),
                            }
                        } else {
                            // Legacy path untouched: the term's packet goes to
                            // the wire exactly as it arrived, as it always did.
                            pkt
                        };
                        match pkt.message {
                            // Hello was already answered synchronously before the PTY
                            // opened; GUI owns heartbeat — drop both.
                            Message::Hello { .. } | Message::Heartbeat => {}
                            Message::ShellClose | Message::Disconnect => {
                                r_stop.store(true, Ordering::Relaxed);
                                break;
                            }
                            // clippy 1.98 wants this folded into a match guard, but
                            // the guard would have to move `pkt` into `send`, which
                            // match guards cannot do (E0382). The lint's own
                            // suggestion does not compile — verified 2026-09-04.
                            #[allow(clippy::collapsible_match)]
                            Message::ShellInput { .. }
                            | Message::PtyInput { .. }
                            | Message::PtyResize { .. } => {
                                if r_outgoing.send(pkt).is_err() {
                                    r_stop.store(true, Ordering::Relaxed);
                                    break;
                                }
                            }
                            // Any other message type from the term is unexpected on
                            // the interactive path — ignore rather than forward.
                            _ => {}
                        }
                    }
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
                    let _ =
                        write_packet_frame(&mut *w, &Packet::new(Message::ShellExit { code }, 0));
                }
                break;
            }
            // Acknowledgement of somebody's `ShellClose` - possibly this
            // session's own teardown, possibly a leftover from an earlier
            // one. Either way it says nothing about the shell this relay is
            // streaming, so it is not forwarded and does not end the loop.
            Ok(ExecEvent::ShellClosed) => {}
            Ok(ExecEvent::HostError(msg)) => {
                // A host shell error on the interactive path is terminal: the
                // only `Message::Error`s the reader fans in here are shell-open
                // failures ("shell already open" from a stale host slot after a
                // crashed prior session, or a spawn error), and no `ShellExit`
                // follows them. Forward the error to the term, then break so
                // teardown runs — otherwise the owner guard stays held and the
                // channel is stuck "shell busy" until the user manually Ctrl+]s.
                if let Ok(mut w) = write_stream.lock() {
                    let _ = write_packet_frame(
                        &mut *w,
                        &Packet::new(Message::Error { code: 0, msg }, 0),
                    );
                }
                break;
            }
            Ok(ExecEvent::Idle) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Teardown. Signal + shut the socket down to unblock the blocking reader,
    // join it, then close the host shell so the next session can ShellOpen.
    stop.store(true, Ordering::Relaxed);
    // Recover on poison: if a pump panicked while holding the write lock the
    // guard is poisoned, but we still MUST shut the socket down — it's the only
    // thing that unblocks the timeout-less reader below (see the reader comment
    // above). Silently skipping the shutdown on `Err` would hang reader.join()
    // forever, so `_owner_guard` never drops and every later `wd` is refused
    // "shell busy" until the GUI restarts. Matches the poison recovery used for
    // `single_inflight` in handle_connection.
    {
        let w = write_stream.lock().unwrap_or_else(|p| p.into_inner());
        let _ = w.shutdown(std::net::Shutdown::Both);
    }
    let _ = reader.join();
    // Close the slot this session actually holds. `PtyClose` is unacknowledged
    // by design — nothing below waits for one, and an ack would only land in
    // the middle of whatever `wd --exec` is streaming.
    let close_msg = if dual {
        Message::PtyClose
    } else {
        Message::ShellClose
    };
    if let Err(e) = outgoing_tx.send(Packet::new(close_msg, 0)) {
        log::warn!("IPC interactive: close send failed on teardown: {e}");
    }
    // `_owner_guard` (→ Idle) and `_slot_guard` (→ None) drop here.
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_finished_run_does_not_hold_the_next_command_for_two_seconds() {
        // The host never answers ShellClose, so this budget is paid in
        // full on every successful run and the next wd --exec queues
        // behind it. A run that reached its sentinel has nothing left to
        // emit; only a failed one might still be streaming.
        assert!(post_run_idle(true) <= Duration::from_millis(300));
        assert!(post_run_idle(true) >= Duration::from_millis(100));
        assert_eq!(post_run_idle(false), Duration::from_secs(2));
    }

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
    /// The socket dir is narrowed to 0700 before binding, so the brief
    /// window where the socket itself still carries umask-derived perms
    /// is not reachable by another local user.
    #[test]
    fn narrow_socket_dir_makes_dir_owner_only() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("WireDesk");
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod 755");

        narrow_socket_dir(&dir);

        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "expected owner-only, got {:o}",
            mode & 0o777
        );
    }

    /// Already-narrow dirs are left exactly as they are — no needless chmod,
    /// and a stricter mode (0500) is not widened back to 0700.
    #[test]
    fn narrow_socket_dir_leaves_already_private_dir_alone() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("WireDesk");
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).expect("chmod 500");

        narrow_socket_dir(&dir);

        let mode = std::fs::metadata(&dir).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o500);
    }

    /// A missing dir is a no-op, not a panic.
    #[test]
    fn narrow_socket_dir_tolerates_missing_dir() {
        use tempfile::TempDir;
        let tmp = TempDir::new().expect("tempdir");
        narrow_socket_dir(&tmp.path().join("does-not-exist"));
    }

    /// stage events directly into the exec slot and let the handler's
    /// runner consume them.
    #[test]
    fn handler_round_trip_via_unix_socket() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        spawn_ipc_acceptor(
            socket.clone(),
            outgoing_tx,
            slots.clone(),
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
        let stage_slot = slots.exec.clone();
        let stage_thread = thread::spawn(move || {
            // Handler now sends ShellOpen first, then payload (a
            // ShellInput) — drain ShellOpen and keep reading until
            // we land on the ShellInput carrying the sentinel marker.
            let payload = loop {
                let pkt = outgoing_rx
                    .recv_timeout(Duration::from_secs(10))
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

            // Stage: prompt → READY → output → sentinel. The prompt is
            // there to be dropped: the runner stays muted until READY.
            let stage = |slot: &ExecEventSlot, ev: ExecEvent| {
                if let Some(tx) = slot.lock().unwrap().as_ref() {
                    let _ = tx.send(ev);
                }
            };
            stage(&stage_slot, ExecEvent::ShellOutput(b"PS C:\\>\n".to_vec()));
            stage(
                &stage_slot,
                ExecEvent::ShellOutput(format!("__WD_READY_{uuid}__\n").into_bytes()),
            );
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
        let slots = ShellSlots::new();
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
            slots,
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
            .set_read_timeout(Some(Duration::from_secs(10)))
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
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info: SharedHostInfo = Arc::new(Mutex::new(None));
        let link_up = Arc::new(AtomicBool::new(true));

        // Must not panic.
        spawn_ipc_acceptor(socket, tx, slots, owner, inflight, host_info, link_up);
    }

    #[test]
    fn stale_socket_unlinked_before_bind() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");
        std::fs::write(&socket, b"stale leftover").unwrap();
        assert!(socket.exists(), "stale file present");

        let (tx, _rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info: SharedHostInfo = Arc::new(Mutex::new(None));
        let link_up = Arc::new(AtomicBool::new(true));
        spawn_ipc_acceptor(
            socket.clone(),
            tx,
            slots,
            owner,
            inflight,
            host_info,
            link_up,
        );
        thread::sleep(Duration::from_millis(50));

        // Now it should be a real socket — connect should succeed.
        let res = UnixStream::connect(&socket);
        assert!(
            res.is_ok(),
            "stale-unlink + bind should leave a working socket: {res:?}"
        );
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
        assert!(
            decoded.compress,
            "handler-side decode preserves compress flag"
        );
        assert_eq!(decoded.cmd, "echo hi");
    }

    // ---- Interactive relay (Task 6) -------------------------------------

    use crate::exec_bridge::ExecEventSlot;
    use crate::link::{HostInfo, SharedHostInfo};
    use crate::shell_channel::{current_owner, new_shared_owner};
    use wiredesk_protocol::message::HOST_PROTO_VERSION;

    /// Host-info cache as the reader would leave it, for a host announcing
    /// `proto_version`.
    fn host_info_v(proto_version: u8) -> SharedHostInfo {
        Arc::new(Mutex::new(Some(HostInfo {
            host_name: "win-host".into(),
            screen_w: 2560,
            screen_h: 1440,
            proto_version,
        })))
    }

    /// The pre-two-slot host. Default for the tests that predate the split, so
    /// they keep pinning the old contract exactly as they did.
    fn populated_host_info() -> SharedHostInfo {
        host_info_v(1)
    }

    /// A host with the dedicated pty slot.
    fn dual_host_info() -> SharedHostInfo {
        host_info_v(HOST_PROTO_VERSION)
    }

    /// Spin until the interactive handler has installed its exec slot (it does
    /// so before originating ShellOpenPty, but the handler runs on its own
    /// thread so we poll to avoid a race in the staging tests).
    fn wait_slot_installed(slot: &ExecEventSlot) {
        // 10 s of 5 ms steps. The handler installs the slot in microseconds
        // on an idle machine; the budget is for a loaded CI runner, where a
        // thread can simply not be scheduled for a second or two.
        for _ in 0..2000 {
            if slot.lock().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("exec slot never installed by interactive handler");
    }

    fn stage_event(slot: &ExecEventSlot, ev: ExecEvent) {
        let guard = slot.lock().unwrap();
        let tx = guard
            .as_ref()
            .expect("slot must be installed before staging");
        tx.send(ev).expect("stage into installed slot");
    }

    /// Client-side interactive handshake: send `Hello`, read + return the relay's
    /// synth `HelloAck`. The relay answers `Hello` BEFORE originating
    /// `ShellOpenPty` (Codex P2 ordering), so tests must handshake before
    /// expecting the PTY-open on the wire.
    fn client_handshake(client: &mut UnixStream) -> Message {
        write_packet_frame(
            client,
            &Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "mac-term".into(),
                },
                0,
            ),
        )
        .unwrap();
        read_packet_frame(client).expect("HelloAck").message
    }

    #[test]
    fn interactive_hello_synth_ack_and_forwards_input() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        let open = IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 100,
            rows: 30,
        };
        let (h_slot, h_owner, h_hi, h_link) = (
            slots.clone(),
            owner.clone(),
            host_info.clone(),
            link_up.clone(),
        );
        let handler = thread::spawn(move || {
            handle_interactive_connection(server, open, outgoing_tx, h_slot, h_owner, h_hi, h_link);
        });

        // New ordering (Codex P2): the relay reads Hello and answers the synth
        // HelloAck (from the cache, NOT forwarded to the wire) BEFORE originating
        // ShellOpenPty. Handshake first.
        match client_handshake(&mut client) {
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

        // Only after the HelloAck does the relay originate the single ShellOpenPty
        // — the term sends none.
        let first = outgoing_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("relay must originate ShellOpenPty after handshake");
        match first.message {
            Message::ShellOpenPty { shell, cols, rows } => {
                assert_eq!(shell, "pwsh");
                assert_eq!(cols, 100);
                assert_eq!(rows, 30);
            }
            other => panic!("expected ShellOpenPty after handshake, got {other:?}"),
        }
        assert_eq!(current_owner(&owner), ShellOwner::Interactive);

        // Heartbeat dropped; ShellInput + PtyResize forwarded to the wire.
        write_packet_frame(&mut client, &Packet::new(Message::Heartbeat, 0)).unwrap();
        write_packet_frame(
            &mut client,
            &Packet::new(
                Message::ShellInput {
                    data: b"ls\r".to_vec(),
                },
                0,
            ),
        )
        .unwrap();
        write_packet_frame(
            &mut client,
            &Packet::new(Message::PtyResize { cols: 80, rows: 24 }, 0),
        )
        .unwrap();

        // Next wire packet is ShellInput — Hello + Heartbeat were NOT forwarded.
        match outgoing_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("forwarded ShellInput")
            .message
        {
            Message::ShellInput { data } => assert_eq!(data, b"ls\r"),
            other => panic!("expected forwarded ShellInput, got {other:?}"),
        }
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("forwarded PtyResize")
                .message,
            Message::PtyResize { cols: 80, rows: 24 }
        ));

        // Staged host ShellOutput / ShellExit reach the socket.
        wait_slot_installed(&slots.exec);
        stage_event(&slots.exec, ExecEvent::ShellOutput(b"hi\n".to_vec()));
        match read_packet_frame(&mut client).expect("ShellOutput").message {
            Message::ShellOutput { data } => assert_eq!(data, b"hi\n"),
            other => panic!("expected ShellOutput, got {other:?}"),
        }
        stage_event(&slots.exec, ExecEvent::ShellExit(0));
        assert!(matches!(
            read_packet_frame(&mut client).expect("ShellExit").message,
            Message::ShellExit { code: 0 }
        ));

        handler.join().expect("handler thread");
        // Teardown: single host-side ShellClose + owner released.
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("teardown ShellClose")
                .message,
            Message::ShellClose
        ));
        assert_eq!(current_owner(&owner), ShellOwner::Idle);
    }

    #[test]
    fn interactive_refused_when_link_down() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
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
                server,
                open,
                outgoing_tx,
                slots,
                owner,
                host_info,
                link_up,
            );
        });

        match read_packet_frame(&mut client)
            .expect("refuse frame")
            .message
        {
            Message::Error { code, .. } => assert_eq!(code, RELAY_REFUSE_CODE),
            other => panic!("expected Error refuse frame, got {other:?}"),
        }
        handler.join().unwrap();
        assert!(
            outgoing_rx.try_recv().is_err(),
            "no ShellOpenPty may be queued when the link is down"
        );
        assert_eq!(
            current_owner(&owner_probe),
            ShellOwner::Idle,
            "refused connect must release the channel"
        );
    }

    #[test]
    fn interactive_refused_when_host_info_empty() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
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
                server,
                open,
                outgoing_tx,
                slots,
                owner,
                host_info,
                link_up,
            );
        });

        assert!(matches!(
            read_packet_frame(&mut client)
                .expect("refuse frame")
                .message,
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
            current_owner(&owner_probe),
            ShellOwner::Idle,
            "refused connect must release the channel"
        );
    }

    #[test]
    fn interactive_refused_when_channel_busy() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        // Pre-claim as Exec — against this (legacy) host a competing
        // interactive connect must fail fast.
        let _held = try_acquire(&owner, ShellOwner::Exec, false).expect("pre-claim Exec");
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
                slots,
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
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
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
                server,
                open,
                outgoing_tx,
                slots,
                owner,
                host_info,
                link_up,
            );
        });

        // Handshake first, then the relay originates ShellOpenPty.
        let _ = client_handshake(&mut client);
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(10))
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

    #[test]
    fn interactive_host_shell_error_tears_down_and_releases_owner() {
        // A host shell-open error (e.g. "shell already open" from a stale host
        // slot after a crashed prior session) arrives as ExecEvent::HostError
        // with no ShellExit to follow. The relay must forward it to the term,
        // then break so teardown runs — otherwise the owner guard stays held and
        // every later `wd` is refused "shell busy" indefinitely.
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        let open = IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 80,
            rows: 24,
        };
        let (h_slot, h_owner, h_hi, h_link) = (
            slots.clone(),
            owner.clone(),
            host_info.clone(),
            link_up.clone(),
        );
        let handler = thread::spawn(move || {
            handle_interactive_connection(server, open, outgoing_tx, h_slot, h_owner, h_hi, h_link);
        });

        // Handshake first, then the relay originates ShellOpenPty and holds the
        // channel.
        let _ = client_handshake(&mut client);
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("ShellOpenPty")
                .message,
            Message::ShellOpenPty { .. }
        ));
        assert_eq!(current_owner(&owner), ShellOwner::Interactive);

        // Host rejects the open — HostError with no ShellExit to follow.
        wait_slot_installed(&slots.exec);
        stage_event(
            &slots.exec,
            ExecEvent::HostError("shell already open".into()),
        );

        // The error is forwarded to the term...
        match read_packet_frame(&mut client)
            .expect("forwarded host error")
            .message
        {
            Message::Error { msg, .. } => assert!(msg.contains("shell"), "msg: {msg}"),
            other => panic!("expected forwarded Message::Error, got {other:?}"),
        }
        // ...and the relay tears down: single host-side ShellClose + owner freed.
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("teardown ShellClose after host error")
                .message,
            Message::ShellClose
        ));
        handler
            .join()
            .expect("handler thread must return after host error");
        assert_eq!(
            current_owner(&owner),
            ShellOwner::Idle,
            "host shell error must release the channel"
        );
    }

    // ---- Atomic IpcConnect cutover (Task 7) -----------------------------

    /// The same situation against a host with the dedicated pty slot: the exec
    /// run must go through, because it lands in a slot the console never
    /// touches. This is the whole point of the split, at the handler level.
    #[test]
    fn exec_proceeds_while_interactive_holds_channel_on_a_dual_host() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        // A live console, claimed the way the relay claims it on a dual host.
        let _held =
            try_acquire(&owner, ShellOwner::Interactive, true).expect("pre-claim Interactive");
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let link_up = Arc::new(AtomicBool::new(true));

        spawn_ipc_acceptor(
            socket.clone(),
            outgoing_tx,
            slots,
            owner.clone(),
            inflight,
            dual_host_info(),
            link_up,
        );
        thread::sleep(Duration::from_millis(50));

        let mut client = UnixStream::connect(&socket).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write_connect(
            &mut client,
            &IpcConnect::Exec(IpcRequest {
                cmd: "echo hi".into(),
                ssh: None,
                timeout_secs: 5,
                compress: false,
            }),
        )
        .unwrap();

        // Not refused: the run opens its own shell on the wire. (It then waits
        // for a sentinel that never comes and times out on its own; all this
        // test needs is that it started at all.)
        match outgoing_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("exec must reach the wire beside a live console")
            .message
        {
            Message::ShellOpen { .. } => {}
            other => panic!("expected ShellOpen, got {other:?}"),
        }
    }

    /// Against a dual host the relay must speak the pty dialect end to end:
    /// open with `PtyOpen`, re-address the term's stdin to `PtyInput`, and
    /// close with `PtyClose`. The term itself is unchanged and knows none of
    /// this.
    #[test]
    fn interactive_relay_speaks_the_pty_dialect_on_a_dual_host() {
        let (mut client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        let host_info = dual_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        let open = IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 100,
            rows: 30,
        };
        let (h_slots, h_owner, h_hi, h_link) = (
            slots.clone(),
            owner.clone(),
            host_info.clone(),
            link_up.clone(),
        );
        let handler = thread::spawn(move || {
            handle_interactive_connection(
                server,
                open,
                outgoing_tx,
                h_slots,
                h_owner,
                h_hi,
                h_link,
            );
        });

        let _ack = client_handshake(&mut client);
        match outgoing_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .message
        {
            Message::PtyOpen { shell, cols, rows } => {
                assert_eq!((shell.as_str(), cols, rows), ("pwsh", 100, 30));
            }
            other => panic!("expected PtyOpen on a dual host, got {other:?}"),
        }

        // The term sends `ShellInput`; the relay re-addresses it.
        write_packet_frame(
            &mut client,
            &Packet::new(
                Message::ShellInput {
                    data: b"echo hi\r".to_vec(),
                },
                0,
            ),
        )
        .unwrap();
        match outgoing_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .message
        {
            Message::PtyInput { data } => assert_eq!(data, b"echo hi\r"),
            other => panic!("expected PtyInput, got {other:?}"),
        }

        // Output comes back on the pty slot and reaches the term as the plain
        // `ShellOutput` it understands.
        wait_slot_installed(&slots.pty);
        stage_event(&slots.pty, ExecEvent::ShellOutput(b"hi\r\n".to_vec()));
        match read_packet_frame(&mut client)
            .expect("console output")
            .message
        {
            Message::ShellOutput { data } => assert_eq!(data, b"hi\r\n"),
            other => panic!("expected ShellOutput to the term, got {other:?}"),
        }

        // Teardown closes the pty slot only.
        write_packet_frame(&mut client, &Packet::new(Message::Disconnect, 0)).unwrap();
        assert!(matches!(
            outgoing_rx
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .message,
            Message::PtyClose
        ));
        handler.join().expect("relay thread");
        assert_eq!(current_owner(&owner), ShellOwner::Idle);
    }

    /// While an interactive `wd` session holds the shell channel, an incoming
    /// `wd --exec` must fail fast with a transport-class frame (term → exit 125)
    /// and never queue a `ShellOpen` behind the minutes-long PTY session.
    #[test]
    fn exec_refused_when_interactive_holds_channel() {
        use tempfile::TempDir;

        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        // Pre-claim the channel as Interactive — mirrors a live PTY session.
        let _held =
            try_acquire(&owner, ShellOwner::Interactive, false).expect("pre-claim Interactive");
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        spawn_ipc_acceptor(
            socket.clone(),
            outgoing_tx,
            slots,
            owner.clone(),
            inflight,
            host_info,
            link_up,
        );
        thread::sleep(Duration::from_millis(50));

        let mut client = UnixStream::connect(&socket).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
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
            current_owner(&owner),
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
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info = populated_host_info();
        let link_up = Arc::new(AtomicBool::new(true));

        spawn_ipc_acceptor(
            socket.clone(),
            outgoing_tx,
            slots.clone(),
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
        let stage_slot = slots.exec.clone();
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
                // READY opens every wrapper the runner builds, and the
                // sentinel is only honoured after it.
                stage(ExecEvent::ShellOutput(
                    format!("__WD_READY_{uuid}__\n").into_bytes(),
                ));
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
            if current_owner(&owner) == ShellOwner::Idle {
                released = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            released,
            "channel must return to Idle after both exec runs (owner still {:?})",
            current_owner(&owner)
        );
    }

    #[test]
    fn abbreviate_cmd_keeps_short_commands_whole() {
        assert_eq!(abbreviate_cmd("Get-ChildItem"), "\"Get-ChildItem\"");
    }

    #[test]
    fn abbreviate_cmd_truncates_long_commands_with_remainder() {
        let cmd = "x".repeat(CMD_LOG_CHARS + 40);
        let out = abbreviate_cmd(&cmd);
        assert!(out.starts_with(&format!("\"{}\"", "x".repeat(CMD_LOG_CHARS))));
        assert!(out.ends_with("… (+40 chars)"), "got {out}");
    }

    #[test]
    fn abbreviate_cmd_cuts_on_char_boundary() {
        // Cyrillic is two bytes per char; a byte-indexed slice would panic.
        let cmd = "ж".repeat(CMD_LOG_CHARS + 3);
        let out = abbreviate_cmd(&cmd);
        assert!(out.ends_with("… (+3 chars)"), "got {out}");
        assert_eq!(out.matches('ж').count(), CMD_LOG_CHARS);
    }
}
