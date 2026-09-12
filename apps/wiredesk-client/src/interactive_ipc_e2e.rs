//! End-to-end interactive-IPC round-trip test (Task 9).
//!
//! Unlike the per-function unit tests in `ipc.rs` (which call
//! `handle_interactive_connection` directly with a `UnixStream::pair`), this
//! exercises the *full* socket path: a fake-GUI binds a temp socket via the
//! real `spawn_ipc_acceptor`, and an in-process client connects, sends an
//! `IpcConnect::Interactive` dispatch frame, and drives the whole PTY session
//! over the wire — so the acceptor's `dispatch_connection` routing and the
//! `IpcConnect` framing are covered too.
//!
//! `wiredesk-client` is a binary-only crate (no lib target), so a real
//! `tests/` integration crate can't reach `spawn_ipc_acceptor`; the plan
//! (Task 9) explicitly allows a `#[cfg(test)]` integ module instead. This file
//! is that module — gated `#[cfg(all(test, target_os = "macos"))]` in `main.rs`
//! (the whole IPC subsystem is Mac-only).

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use tempfile::TempDir;
use wiredesk_exec_core::ipc::{
    read_packet_frame, read_response, write_connect, write_packet_frame, IpcConnect,
    IpcInteractiveOpen, IpcRequest, IpcResponse,
};
use wiredesk_exec_core::ExecEvent;
use wiredesk_protocol::message::{Message, HOST_PROTO_VERSION};
use wiredesk_protocol::packet::Packet;

use crate::exec_bridge::{ExecEventSlot, ShellSlots};
use crate::ipc::spawn_ipc_acceptor;
use crate::link::{HostInfo, SharedHostInfo};
use crate::shell_channel::{
    current_owner, new_shared_owner, try_acquire, SharedShellOwner, ShellOwner,
};

/// Shared wiring for a fake-GUI: the acceptor's dependencies plus the mock
/// `outgoing_rx` (captures packets the relay forwards to the "wire") and the
/// installed shell slots (drivable host shell-event sources).
struct FakeGui {
    _tmp: TempDir,
    socket: PathBuf,
    outgoing_rx: mpsc::Receiver<Packet>,
    slots: ShellSlots,
    owner: SharedShellOwner,
}

impl FakeGui {
    /// A fake GUI talking to a host that predates the dedicated pty slot —
    /// the contract every test here was originally written against.
    fn spawn() -> Self {
        Self::spawn_with_proto(1)
    }

    /// A fake GUI talking to a host that has both shell slots.
    fn spawn_dual() -> Self {
        Self::spawn_with_proto(HOST_PROTO_VERSION)
    }

    /// Bind a temp socket and spawn the real acceptor against fresh mocks,
    /// with a populated host-info cache (win-host, 2560x1440, announcing the
    /// given protocol generation) and `link_up`.
    fn spawn_with_proto(proto_version: u8) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let slots = ShellSlots::new();
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info: SharedHostInfo = Arc::new(Mutex::new(Some(HostInfo {
            host_name: "win-host".into(),
            screen_w: 2560,
            screen_h: 1440,
            proto_version,
        })));
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
        // Let the acceptor bind before the first connect.
        thread::sleep(Duration::from_millis(50));

        Self {
            _tmp: tmp,
            socket,
            outgoing_rx,
            slots,
            owner,
        }
    }

    /// Open an interactive session and drive it through the handshake and the
    /// host-side open, returning the client socket. Asserts that the open went
    /// out on the opcode this host generation expects.
    fn open_interactive(&self, expect_dual: bool) -> UnixStream {
        let mut c = self.connect();
        write_connect(
            &mut c,
            &IpcConnect::Interactive(IpcInteractiveOpen {
                shell: "pwsh".into(),
                cols: 80,
                rows: 24,
            }),
        )
        .unwrap();
        let _ = client_handshake(&mut c);
        match (expect_dual, self.recv_wire()) {
            (true, Message::PtyOpen { .. }) | (false, Message::ShellOpenPty { .. }) => {}
            (dual, other) => panic!("wrong open opcode for dual={dual}: {other:?}"),
        }
        c
    }

    /// The slot the interactive relay installs against this host generation.
    fn console_slot(&self, dual: bool) -> &ExecEventSlot {
        if dual {
            &self.slots.pty
        } else {
            &self.slots.exec
        }
    }

    fn connect(&self) -> UnixStream {
        let c = UnixStream::connect(&self.socket).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        c
    }

    fn recv_wire(&self) -> Message {
        self.outgoing_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("relay should forward a packet to the wire")
            .message
    }
}

/// Wait until the interactive handler installs its exec slot (it does so
/// before it can receive staged host events; the handler runs on its own
/// acceptor-spawned thread, so we poll).
fn wait_slot_installed(slot: &ExecEventSlot) {
    for _ in 0..600 {
        if slot.lock().unwrap().is_some() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("exec slot never installed by interactive handler");
}

fn stage_event(slot: &ExecEventSlot, ev: ExecEvent) {
    let guard = slot.lock().unwrap();
    let tx = guard.as_ref().expect("slot installed before staging");
    tx.send(ev).expect("stage into installed slot");
}

/// Poll the owner until it returns to `Idle` (teardown drops the guard on the
/// handler thread, which we can't join through the detached acceptor).
fn wait_owner_idle(owner: &SharedShellOwner) {
    for _ in 0..600 {
        if current_owner(owner) == ShellOwner::Idle {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!(
        "channel never returned to Idle (owner still {:?})",
        current_owner(owner)
    );
}

/// Client-side interactive handshake: send `Hello`, read + return the relay's
/// synth `HelloAck` message. The relay answers `Hello` BEFORE originating
/// `ShellOpenPty` (Codex P2 ordering), so the client handshakes first.
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

/// Full happy-path round-trip through the acceptor:
/// `IpcConnect::Interactive` → `Hello`/synth-`HelloAck` → relay originates the
/// single `ShellOpenPty` → forwarded `ShellInput`/`PtyResize` → staged
/// `ShellOutput` echo → `ShellExit` → teardown `ShellClose` → owner `Idle`.
#[test]
fn e2e_interactive_round_trip_through_acceptor() {
    let gui = FakeGui::spawn();
    let mut client = gui.connect();

    // Dispatch frame: the streaming interactive path.
    write_connect(
        &mut client,
        &IpcConnect::Interactive(IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 120,
            rows: 40,
        }),
    )
    .unwrap();

    // Hello → synth HelloAck from the cache (NOT forwarded to the wire), BEFORE
    // the relay originates ShellOpenPty.
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

    // Only after the HelloAck does the relay originate the ONE ShellOpenPty (the
    // term sends none) with the geometry from the dispatch frame.
    match gui.recv_wire() {
        Message::ShellOpenPty { shell, cols, rows } => {
            assert_eq!(shell, "pwsh");
            assert_eq!(cols, 120);
            assert_eq!(rows, 40);
        }
        other => panic!("expected ShellOpenPty after handshake, got {other:?}"),
    }
    assert_eq!(current_owner(&gui.owner), ShellOwner::Interactive);

    // Heartbeat dropped; ShellInput + PtyResize forwarded to the wire.
    write_packet_frame(&mut client, &Packet::new(Message::Heartbeat, 0)).unwrap();
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
    write_packet_frame(
        &mut client,
        &Packet::new(
            Message::PtyResize {
                cols: 100,
                rows: 30,
            },
            0,
        ),
    )
    .unwrap();

    match gui.recv_wire() {
        Message::ShellInput { data } => assert_eq!(data, b"echo hi\r"),
        other => panic!("expected forwarded ShellInput, got {other:?}"),
    }
    assert!(matches!(
        gui.recv_wire(),
        Message::PtyResize {
            cols: 100,
            rows: 30
        }
    ));

    // Staged host ShellOutput echoes back to the socket, then ShellExit.
    wait_slot_installed(&gui.slots.exec);
    stage_event(&gui.slots.exec, ExecEvent::ShellOutput(b"hi\r\n".to_vec()));
    match read_packet_frame(&mut client).expect("ShellOutput").message {
        Message::ShellOutput { data } => assert_eq!(data, b"hi\r\n"),
        other => panic!("expected ShellOutput, got {other:?}"),
    }
    stage_event(&gui.slots.exec, ExecEvent::ShellExit(0));
    assert!(matches!(
        read_packet_frame(&mut client).expect("ShellExit").message,
        Message::ShellExit { code: 0 }
    ));

    // Teardown: the relay sends the single host-side ShellClose and releases
    // the channel.
    assert!(matches!(gui.recv_wire(), Message::ShellClose));
    wait_owner_idle(&gui.owner);
}

/// A second interactive connect while the first session holds the channel must
/// fail fast with a "shell busy" terminal frame and never originate a second
/// `ShellOpenPty`.
#[test]
fn e2e_second_interactive_connect_is_busy() {
    let gui = FakeGui::spawn();

    // Session 1: establish and hold the channel.
    let mut c1 = gui.connect();
    write_connect(
        &mut c1,
        &IpcConnect::Interactive(IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 80,
            rows: 24,
        }),
    )
    .unwrap();
    // Confirm session 1 acquired the channel: handshake, then ShellOpenPty.
    let _ = client_handshake(&mut c1);
    assert!(matches!(gui.recv_wire(), Message::ShellOpenPty { .. }));
    assert_eq!(current_owner(&gui.owner), ShellOwner::Interactive);

    // Session 2: connect while session 1 holds the channel → "shell busy".
    let mut c2 = gui.connect();
    write_connect(
        &mut c2,
        &IpcConnect::Interactive(IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 80,
            rows: 24,
        }),
    )
    .unwrap();
    match read_packet_frame(&mut c2).expect("busy frame").message {
        Message::Error { msg, .. } => assert!(msg.contains("busy"), "msg: {msg}"),
        other => panic!("expected 'shell busy' Error, got {other:?}"),
    }

    // The refused connect must NOT have originated a second ShellOpenPty. Drain
    // the wire: only session 1's ShellClose may appear once we tear it down —
    // no stray ShellOpenPty from session 2 before that.
    write_packet_frame(&mut c1, &Packet::new(Message::Disconnect, 0)).unwrap();
    // Session 1 teardown emits exactly one ShellClose; nothing from session 2.
    assert!(matches!(gui.recv_wire(), Message::ShellClose));
    assert!(
        gui.outgoing_rx.try_recv().is_err(),
        "refused session must not queue any packet on the wire"
    );
    wait_owner_idle(&gui.owner);
}

/// The channel must be reusable after a session tears down: a fresh
/// interactive connect after the first completes succeeds (guards against the
/// owner guard leaking on the acceptor path).
#[test]
fn e2e_channel_reusable_after_teardown() {
    let gui = FakeGui::spawn();

    // First session: open, then immediately tear down via ShellExit.
    let mut c1 = gui.connect();
    write_connect(
        &mut c1,
        &IpcConnect::Interactive(IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 80,
            rows: 24,
        }),
    )
    .unwrap();
    let _ = client_handshake(&mut c1);
    assert!(matches!(gui.recv_wire(), Message::ShellOpenPty { .. }));
    wait_slot_installed(&gui.slots.exec);
    stage_event(&gui.slots.exec, ExecEvent::ShellExit(0));
    // read the ShellExit the relay forwards, then teardown ShellClose.
    assert!(matches!(
        read_packet_frame(&mut c1).expect("ShellExit").message,
        Message::ShellExit { code: 0 }
    ));
    assert!(matches!(gui.recv_wire(), Message::ShellClose));
    wait_owner_idle(&gui.owner);

    // Second session on the now-free channel must acquire cleanly.
    let mut c2 = gui.connect();
    write_connect(
        &mut c2,
        &IpcConnect::Interactive(IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 80,
            rows: 24,
        }),
    )
    .unwrap();
    let _ = client_handshake(&mut c2);
    assert!(matches!(gui.recv_wire(), Message::ShellOpenPty { .. }));
    assert_eq!(current_owner(&gui.owner), ShellOwner::Interactive);

    // Sanity: the second session really owns it — a competing acquire fails.
    assert!(
        try_acquire(&gui.owner, ShellOwner::Exec, false).is_none(),
        "second session must exclusively hold the channel"
    );

    // Clean teardown.
    write_packet_frame(&mut c2, &Packet::new(Message::Disconnect, 0)).unwrap();
    assert!(matches!(gui.recv_wire(), Message::ShellClose));
    wait_owner_idle(&gui.owner);
}

/// Pull the run's UUID out of the payload the runner put on the wire, so the
/// test can answer with a sentinel the runner will actually accept.
fn uuid_from_payload(payload: &str) -> String {
    let marker = "__WD_DONE_";
    let start = payload.find(marker).expect("payload carries a sentinel") + marker.len();
    let after = &payload[start..];
    let end = after.find("__").expect("sentinel is terminated");
    after[..end].to_string()
}

/// Stage the four host-side events one `wd --exec` run expects: a prompt to be
/// dropped, the READY marker, the output, and the sentinel.
fn stage_exec_run(slot: &ExecEventSlot, uuid: &str) {
    stage_event(slot, ExecEvent::ShellOutput(b"PS C:\\>\n".to_vec()));
    stage_event(
        slot,
        ExecEvent::ShellOutput(format!("__WD_READY_{uuid}__\n").into_bytes()),
    );
    stage_event(slot, ExecEvent::ShellOutput(b"hi\n".to_vec()));
    stage_event(
        slot,
        ExecEvent::ShellOutput(format!("__WD_DONE_{uuid}__0\n").into_bytes()),
    );
}

/// Connect as `wd --exec` and run to completion on a background thread,
/// returning (exit code, stdout).
fn spawn_exec_client(socket: PathBuf) -> thread::JoinHandle<(i32, Vec<u8>)> {
    thread::spawn(move || {
        let mut c = UnixStream::connect(&socket).expect("connect exec");
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write_connect(
            &mut c,
            &IpcConnect::Exec(IpcRequest {
                cmd: "echo hi".into(),
                ssh: None,
                timeout_secs: 5,
                compress: false,
            }),
        )
        .unwrap();
        let mut out = Vec::new();
        loop {
            match read_response(&mut c).expect("exec response") {
                IpcResponse::Stdout(b) => out.extend_from_slice(&b),
                IpcResponse::Exit(code) => break (code, out),
                IpcResponse::Error(m) => panic!("exec handler error: {m}"),
                IpcResponse::TransportUnavailable(m) => {
                    panic!("exec refused unexpectedly: {m}")
                }
            }
        }
    })
}

/// The feature itself, end to end: an agent's `wd --exec` runs to completion
/// while the owner sits in an interactive console. On a legacy host this is
/// exactly the case that got refused with exit 125.
#[test]
fn e2e_interactive_and_exec_run_concurrently_on_a_dual_host() {
    let gui = FakeGui::spawn_dual();

    let mut console = gui.open_interactive(true);
    assert_eq!(current_owner(&gui.owner), ShellOwner::Interactive);

    let exec_client = spawn_exec_client(gui.socket.clone());

    // The exec run takes the other slot: `ShellOpen`, then its payload. The
    // console's keystrokes would be `PtyInput`, so nothing here can be
    // confused for them.
    assert!(matches!(gui.recv_wire(), Message::ShellOpen { .. }));
    let payload = loop {
        match gui.recv_wire() {
            Message::ShellInput { data } => {
                let text = String::from_utf8_lossy(&data).to_string();
                if text.contains("__WD_DONE_") {
                    break text;
                }
            }
            other => panic!("unexpected packet while waiting for the payload: {other:?}"),
        }
    };

    wait_slot_installed(&gui.slots.exec);
    stage_exec_run(&gui.slots.exec, &uuid_from_payload(&payload));

    let (code, out) = exec_client.join().expect("exec client thread");
    assert_eq!(code, 0, "exec must succeed alongside a live console");
    assert!(
        String::from_utf8_lossy(&out).contains("hi"),
        "stdout: {out:?}"
    );
    // The finished run closes its own slot on its own opcode.
    assert!(matches!(gui.recv_wire(), Message::ShellClose));

    // And the console is untouched by all of that: host output addressed to
    // the pty slot still reaches it.
    stage_event(
        gui.console_slot(true),
        ExecEvent::ShellOutput(b"prompt>".to_vec()),
    );
    match read_packet_frame(&mut console)
        .expect("console still live")
        .message
    {
        Message::ShellOutput { data } => assert_eq!(data, b"prompt>"),
        other => panic!("expected console output, got {other:?}"),
    }

    // Teardown closes the pty slot, not the exec one.
    write_packet_frame(&mut console, &Packet::new(Message::Disconnect, 0)).unwrap();
    assert!(matches!(gui.recv_wire(), Message::PtyClose));
    wait_owner_idle(&gui.owner);
}

/// The compatibility half, pinned explicitly: against a host with one shell
/// slot the old refusal must still happen. Without this, rolling the Mac side
/// back to a legacy host could quietly start double-booking that slot.
#[test]
fn e2e_exec_is_refused_while_a_legacy_host_console_is_open() {
    let gui = FakeGui::spawn();
    let mut console = gui.open_interactive(false);

    let mut c = UnixStream::connect(&gui.socket).expect("connect exec");
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write_connect(
        &mut c,
        &IpcConnect::Exec(IpcRequest {
            cmd: "echo hi".into(),
            ssh: None,
            timeout_secs: 5,
            compress: false,
        }),
    )
    .unwrap();
    match read_response(&mut c).expect("refusal frame") {
        IpcResponse::TransportUnavailable(m) => {
            assert!(m.contains("busy"), "msg: {m}")
        }
        other => panic!("expected a transport-class refusal, got {other:?}"),
    }
    // Nothing of the refused run reached the wire.
    assert!(
        gui.outgoing_rx.try_recv().is_err(),
        "a refused exec must not queue any packet"
    );

    write_packet_frame(&mut console, &Packet::new(Message::Disconnect, 0)).unwrap();
    assert!(matches!(gui.recv_wire(), Message::ShellClose));
    wait_owner_idle(&gui.owner);
}

/// One console at a time, on a dual host too — there is still exactly one pty
/// slot on the other end.
#[test]
fn e2e_second_interactive_connect_is_busy_on_a_dual_host() {
    let gui = FakeGui::spawn_dual();
    let mut c1 = gui.open_interactive(true);

    let mut c2 = gui.connect();
    write_connect(
        &mut c2,
        &IpcConnect::Interactive(IpcInteractiveOpen {
            shell: "pwsh".into(),
            cols: 80,
            rows: 24,
        }),
    )
    .unwrap();
    match read_packet_frame(&mut c2).expect("busy frame").message {
        Message::Error { msg, .. } => assert!(msg.contains("busy"), "msg: {msg}"),
        other => panic!("expected 'shell busy' Error, got {other:?}"),
    }

    write_packet_frame(&mut c1, &Packet::new(Message::Disconnect, 0)).unwrap();
    assert!(matches!(gui.recv_wire(), Message::PtyClose));
    assert!(
        gui.outgoing_rx.try_recv().is_err(),
        "the refused console must not queue any packet on the wire"
    );
    wait_owner_idle(&gui.owner);
}

/// A dual host re-addresses the term's stdin: the term itself is unchanged and
/// still sends `ShellInput`, but what reaches the wire must be `PtyInput`, or
/// the host would type the owner's keystrokes into the exec slot.
#[test]
fn e2e_console_stdin_is_readdressed_on_a_dual_host() {
    let gui = FakeGui::spawn_dual();
    let mut console = gui.open_interactive(true);

    write_packet_frame(
        &mut console,
        &Packet::new(
            Message::ShellInput {
                data: b"echo hi\r".to_vec(),
            },
            0,
        ),
    )
    .unwrap();
    write_packet_frame(
        &mut console,
        &Packet::new(
            Message::PtyResize {
                cols: 100,
                rows: 30,
            },
            0,
        ),
    )
    .unwrap();

    match gui.recv_wire() {
        Message::PtyInput { data } => assert_eq!(data, b"echo hi\r"),
        other => panic!("console stdin must travel as PtyInput, got {other:?}"),
    }
    // PtyResize is shared by both generations and goes through untouched.
    assert!(matches!(
        gui.recv_wire(),
        Message::PtyResize {
            cols: 100,
            rows: 30
        }
    ));

    write_packet_frame(&mut console, &Packet::new(Message::Disconnect, 0)).unwrap();
    assert!(matches!(gui.recv_wire(), Message::PtyClose));
    wait_owner_idle(&gui.owner);
}
