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
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wiredesk_exec_core::{
    ipc::{read_request, write_response, IpcResponse},
    ExecError, ExecEvent, ExecTransport,
};
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;

use crate::exec_bridge::{ExecEventSlot, ExecSlotGuard};

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
pub fn spawn_ipc_acceptor(
    socket_path: PathBuf,
    outgoing_tx: mpsc::Sender<Packet>,
    exec_slot: ExecEventSlot,
    single_inflight: Arc<Mutex<()>>,
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
                    let single_inflight = single_inflight.clone();
                    thread::spawn(move || {
                        handle_connection(stream, outgoing_tx, exec_slot, single_inflight);
                    });
                }
                Err(e) => {
                    log::warn!("IPC accept error: {e}; continuing");
                }
            }
        }
    });
}

/// Per-connection handler. Holds `single_inflight` for the entire run
/// (so concurrent connections queue), installs the `ExecSlotGuard` so
/// `reader_thread` fans shell events into our private mpsc, runs the
/// shared runner, ships the result back over the socket. Both guards
/// are RAII — panic in any branch still releases them.
fn handle_connection(
    mut stream: UnixStream,
    outgoing_tx: mpsc::Sender<Packet>,
    exec_slot: ExecEventSlot,
    single_inflight: Arc<Mutex<()>>,
) {
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("IPC: read_request failed: {e}; dropping connection");
            return;
        }
    };
    log::info!(
        "IPC handler: cmd={:?} ssh={:?} timeout={}s",
        req.cmd,
        req.ssh,
        req.timeout_secs
    );

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use wiredesk_exec_core::ipc::{write_request, IpcRequest};

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
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));

        spawn_ipc_acceptor(socket.clone(), outgoing_tx, exec_slot.clone(), inflight);

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
        write_request(&mut client, &req).unwrap();

        // Pull responses until Exit.
        let mut stdout_collected = Vec::new();
        let exit = loop {
            match wiredesk_exec_core::ipc::read_response(&mut client).unwrap() {
                IpcResponse::Stdout(b) => stdout_collected.extend_from_slice(&b),
                IpcResponse::Exit(c) => break c,
                IpcResponse::Error(m) => panic!("handler error: {m}"),
            }
        };
        stage_thread.join().expect("stage thread");

        assert_eq!(exit, 0);
        let s = String::from_utf8(stdout_collected).unwrap();
        assert!(s.contains("hi\n"), "stdout streamed: {s:?}");
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
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));

        // Must not panic.
        spawn_ipc_acceptor(socket, tx, slot, inflight);
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
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        spawn_ipc_acceptor(socket.clone(), tx, slot, inflight);
        thread::sleep(Duration::from_millis(50));

        // Now it should be a real socket — connect should succeed.
        let res = UnixStream::connect(&socket);
        assert!(res.is_ok(), "stale-unlink + bind should leave a working socket: {res:?}");
    }

    #[test]
    fn ipc_handler_extracts_compress_field() {
        // Smoke: an IpcRequest with compress=true round-trips through
        // bincode and is readable on the handler side. The actual
        // forwarding to run_oneshot is direct field access (req.compress),
        // verified by compilation; this guards the wire-level path.
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
}
