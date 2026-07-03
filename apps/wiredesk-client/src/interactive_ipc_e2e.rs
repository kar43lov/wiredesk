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
    read_packet_frame, write_connect, write_packet_frame, IpcConnect, IpcInteractiveOpen,
};
use wiredesk_exec_core::ExecEvent;
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;

use crate::exec_bridge::ExecEventSlot;
use crate::ipc::spawn_ipc_acceptor;
use crate::link::{HostInfo, SharedHostInfo};
use crate::shell_channel::{current_owner, new_shared_owner, try_acquire, SharedShellOwner, ShellOwner};

/// Shared wiring for a fake-GUI: the acceptor's dependencies plus the mock
/// `outgoing_rx` (captures packets the relay forwards to the "wire") and the
/// installed `exec_slot` (drivable host shell-event source).
struct FakeGui {
    _tmp: TempDir,
    socket: PathBuf,
    outgoing_rx: mpsc::Receiver<Packet>,
    exec_slot: ExecEventSlot,
    owner: SharedShellOwner,
}

impl FakeGui {
    /// Bind a temp socket and spawn the real acceptor against fresh mocks,
    /// with a populated host-info cache (win-host, 2560x1440) and `link_up`.
    fn spawn() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let socket = tmp.path().join("wd-exec.sock");

        let (outgoing_tx, outgoing_rx) = mpsc::channel::<Packet>();
        let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let owner = new_shared_owner();
        let inflight: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        let host_info: SharedHostInfo = Arc::new(Mutex::new(Some(HostInfo {
            host_name: "win-host".into(),
            screen_w: 2560,
            screen_h: 1440,
        })));
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
        // Let the acceptor bind before the first connect.
        thread::sleep(Duration::from_millis(50));

        Self {
            _tmp: tmp,
            socket,
            outgoing_rx,
            exec_slot,
            owner,
        }
    }

    fn connect(&self) -> UnixStream {
        let c = UnixStream::connect(&self.socket).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        c
    }

    fn recv_wire(&self) -> Message {
        self.outgoing_rx
            .recv_timeout(Duration::from_secs(2))
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

/// Full happy-path round-trip through the acceptor:
/// `IpcConnect::Interactive` → relay originates the single `ShellOpenPty` →
/// `Hello`/synth-`HelloAck` → forwarded `ShellInput`/`PtyResize` → staged
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

    // The relay originates the ONE ShellOpenPty (the term sends none) with the
    // geometry from the dispatch frame.
    match gui.recv_wire() {
        Message::ShellOpenPty { shell, cols, rows } => {
            assert_eq!(shell, "pwsh");
            assert_eq!(cols, 120);
            assert_eq!(rows, 40);
        }
        other => panic!("expected ShellOpenPty first, got {other:?}"),
    }
    assert_eq!(current_owner(&gui.owner), ShellOwner::Interactive);

    // Hello → synth HelloAck from the cache (NOT forwarded to the wire).
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
        &Packet::new(Message::PtyResize { cols: 100, rows: 30 }, 0),
    )
    .unwrap();

    match gui.recv_wire() {
        Message::ShellInput { data } => assert_eq!(data, b"echo hi\r"),
        other => panic!("expected forwarded ShellInput, got {other:?}"),
    }
    assert!(matches!(
        gui.recv_wire(),
        Message::PtyResize { cols: 100, rows: 30 }
    ));

    // Staged host ShellOutput echoes back to the socket, then ShellExit.
    wait_slot_installed(&gui.exec_slot);
    stage_event(&gui.exec_slot, ExecEvent::ShellOutput(b"hi\r\n".to_vec()));
    match read_packet_frame(&mut client).expect("ShellOutput").message {
        Message::ShellOutput { data } => assert_eq!(data, b"hi\r\n"),
        other => panic!("expected ShellOutput, got {other:?}"),
    }
    stage_event(&gui.exec_slot, ExecEvent::ShellExit(0));
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
    // Confirm session 1 acquired the channel (ShellOpenPty originated).
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
    assert!(matches!(gui.recv_wire(), Message::ShellOpenPty { .. }));
    wait_slot_installed(&gui.exec_slot);
    stage_event(&gui.exec_slot, ExecEvent::ShellExit(0));
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
    assert!(matches!(gui.recv_wire(), Message::ShellOpenPty { .. }));
    assert_eq!(current_owner(&gui.owner), ShellOwner::Interactive);

    // Sanity: the second session really owns it — a competing acquire fails.
    assert!(
        try_acquire(&gui.owner, ShellOwner::Exec).is_none(),
        "second session must exclusively hold the channel"
    );

    // Clean teardown.
    write_packet_frame(&mut c2, &Packet::new(Message::Disconnect, 0)).unwrap();
    assert!(matches!(gui.recv_wire(), Message::ShellClose));
    wait_owner_idle(&gui.owner);
}
