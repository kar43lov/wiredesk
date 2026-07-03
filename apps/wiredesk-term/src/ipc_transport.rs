//! `IpcStreamTransport` — a `Transport` impl over the GUI's Unix socket.
//!
//! The interactive `wd` bridge normally opens the serial port directly
//! (`SerialTransport`), which is mutually exclusive with a running GUI (the
//! GUI already owns the port). This transport routes the exact same
//! `Packet` stream through the GUI's `wd-exec.sock` instead, so interactive
//! `wd` can run in parallel with an open GUI — even during active capture.
//!
//! After the `IpcConnect::Interactive` dispatch frame, both directions carry
//! `Packet`s framed by `write_packet_frame`/`read_packet_frame` (length-prefix,
//! no COBS/CRC — the Unix socket is already a reliable byte-stream). Because it
//! implements `Transport`, `bridge_loop` runs over it byte-for-byte unchanged.
//!
//! Two invariants keep `bridge_loop` unchanged:
//! - **`recv` uses a read timeout.** The reader thread only checks its `stop`
//!   flag between `recv()` calls and relies on a periodic `"recv timeout"`
//!   error (same shape `SerialTransport::recv` returns). A blocking read would
//!   hang the reader `join()` on Ctrl+] until the GUI closed the socket. So we
//!   set a ~100 ms read timeout and map `WouldBlock`/`TimedOut` →
//!   `Transport("recv timeout")`.
//! - **`send` drops `Message::Heartbeat`.** The GUI writer already heartbeats
//!   the real wire every 2 s; suppressing the term's heartbeat here avoids a
//!   double heartbeat with zero change to `bridge_loop`.

use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_exec_core::ipc::{read_packet_frame, write_packet_frame};
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

/// Read timeout for `recv`. Short enough that the reader thread wakes to
/// check `stop` promptly on Ctrl+] (so the `join()` returns without waiting
/// for the GUI to close the socket), long enough not to busy-spin.
const RECV_TIMEOUT: Duration = Duration::from_millis(100);

pub struct IpcStreamTransport {
    stream: UnixStream,
    /// Last hard IO result. Timeouts do NOT flip this (they're normal); only
    /// a real read/write failure marks the handle disconnected.
    connected: bool,
}

impl IpcStreamTransport {
    /// Wrap an already-connected `UnixStream`. Sets the `recv` read timeout so
    /// the reader thread stays responsive to `stop`. Used by tests and by
    /// `connect_at` after a successful connect.
    pub fn from_stream(stream: UnixStream) -> Result<Self> {
        stream
            .set_read_timeout(Some(RECV_TIMEOUT))
            .map_err(|e| WireDeskError::Transport(format!("ipc set_read_timeout: {e}")))?;
        Ok(Self {
            stream,
            connected: true,
        })
    }

    /// Try to connect to the GUI socket at `path`. Returns `Ok(None)` when the
    /// socket is absent or refused (`ENOENT`/`ECONNREFUSED` — and any other
    /// connect error), which the caller reads as "no usable GUI socket, fall
    /// back to direct serial" — mirroring the shipped `wd --exec`
    /// `try_socket_first` fallback shape. Path-parameterised for testability.
    pub fn connect_at(path: &Path) -> Result<Option<Self>> {
        match UnixStream::connect(path) {
            Ok(stream) => Ok(Some(Self::from_stream(stream)?)),
            // Any connect failure (missing socket, refused, permission) means
            // there's no GUI IPC to ride — signal fallback, not a hard error.
            Err(_) => Ok(None),
        }
    }
}

impl Transport for IpcStreamTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        // The GUI owns heartbeat on the real wire; suppress the term's own
        // heartbeat so the host doesn't see two. Returning Ok keeps
        // `bridge_loop`'s heartbeat thread happy with no signature change.
        if matches!(packet.message, Message::Heartbeat) {
            return Ok(());
        }
        match write_packet_frame(&mut self.stream, packet) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.connected = false;
                Err(WireDeskError::Transport(format!("ipc write: {e}")))
            }
        }
    }

    fn recv(&mut self) -> Result<Packet> {
        match read_packet_frame(&mut self.stream) {
            Ok(p) => Ok(p),
            // A read timeout on an idle socket is normal — surface the exact
            // "recv timeout" shape the reader thread already tolerates so it
            // wakes to check `stop` (Ctrl+] exits cleanly). Not a disconnect.
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                Err(WireDeskError::Transport("recv timeout".into()))
            }
            // Clean EOF (GUI closed the socket) — treat as disconnect so the
            // reader loop backs off / the bridge tears down.
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                self.connected = false;
                Err(WireDeskError::Transport(format!("ipc closed: {e}")))
            }
            Err(e) => {
                self.connected = false;
                Err(WireDeskError::Transport(format!("ipc read: {e}")))
            }
        }
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn name(&self) -> &'static str {
        "ipc-stream"
    }

    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        let cloned = self
            .stream
            .try_clone()
            .map_err(|e| WireDeskError::Transport(format!("ipc try_clone: {e}")))?;
        // The clone needs its own read timeout too (try_clone does not copy
        // socket options on all platforms).
        Ok(Box::new(IpcStreamTransport::from_stream(cloned)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Instant;

    fn corpus() -> Vec<Message> {
        vec![
            Message::Hello {
                version: 1,
                client_name: "mac-term".into(),
            },
            Message::ShellOpenPty {
                shell: "powershell.exe".into(),
                cols: 120,
                rows: 40,
            },
            Message::ShellInput {
                data: b"Get-Process\r".to_vec(),
            },
            Message::PtyResize { cols: 100, rows: 30 },
            Message::ShellOutput {
                data: vec![0, 1, 0xFF, b'o', b'k'],
            },
            Message::ShellExit { code: 0 },
            Message::ShellClose,
            Message::Disconnect,
        ]
    }

    #[test]
    fn send_recv_round_trip_all_shell_types() {
        let (a, b) = UnixStream::pair().expect("pair");
        let mut tx = IpcStreamTransport::from_stream(a).unwrap();
        let mut rx = IpcStreamTransport::from_stream(b).unwrap();

        let sent = corpus();
        let expect = sent.clone();
        let writer = thread::spawn(move || {
            for (i, msg) in sent.into_iter().enumerate() {
                tx.send(&Packet::new(msg, i as u16)).unwrap();
            }
        });

        for (i, msg) in expect.into_iter().enumerate() {
            // recv may report a spurious "recv timeout" before the writer's
            // bytes land — retry on that, it's the expected idle shape.
            let pkt = loop {
                match rx.recv() {
                    Ok(p) => break p,
                    Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => continue,
                    Err(e) => panic!("unexpected recv error: {e}"),
                }
            };
            assert_eq!(pkt.seq, i as u16);
            assert_eq!(pkt.message, msg);
        }
        writer.join().expect("writer");
    }

    #[test]
    fn send_heartbeat_writes_nothing_to_peer() {
        let (a, b) = UnixStream::pair().expect("pair");
        let mut tx = IpcStreamTransport::from_stream(a).unwrap();
        let mut rx = IpcStreamTransport::from_stream(b).unwrap();

        // Heartbeat is dropped by the transport — the peer must see only the
        // real packet that follows it, not a heartbeat frame.
        tx.send(&Packet::new(Message::Heartbeat, 0)).unwrap();
        tx.send(&Packet::new(Message::ShellInput { data: b"x".to_vec() }, 5))
            .unwrap();

        let pkt = loop {
            match rx.recv() {
                Ok(p) => break p,
                Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => continue,
                Err(e) => panic!("unexpected: {e}"),
            }
        };
        // First (and only) frame on the wire is the ShellInput, not a heartbeat.
        assert_eq!(pkt.seq, 5);
        assert_eq!(pkt.message, Message::ShellInput { data: b"x".to_vec() });
    }

    #[test]
    fn recv_on_idle_socket_returns_timeout_not_hang() {
        let (_a, b) = UnixStream::pair().expect("pair");
        let mut rx = IpcStreamTransport::from_stream(b).unwrap();

        let start = Instant::now();
        let err = rx.recv().unwrap_err();
        // Must return within a small multiple of RECV_TIMEOUT, not block.
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "recv on idle socket should time out promptly, took {:?}",
            start.elapsed()
        );
        match err {
            WireDeskError::Transport(m) => assert!(m.contains("timeout"), "got: {m}"),
            other => panic!("expected Transport(timeout), got {other:?}"),
        }
        // Timeout is not a disconnect.
        assert!(rx.is_connected());
    }

    #[test]
    fn try_clone_yields_independent_decoder() {
        let (a, b) = UnixStream::pair().expect("pair");
        let mut tx = IpcStreamTransport::from_stream(a).unwrap();
        let rx = IpcStreamTransport::from_stream(b).unwrap();
        let mut rx2: Box<dyn Transport> = rx.try_clone().unwrap();

        tx.send(&Packet::new(Message::ShellExit { code: 7 }, 3))
            .unwrap();

        // The clone reads the frame with its own decoder state.
        let pkt = loop {
            match rx2.recv() {
                Ok(p) => break p,
                Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => continue,
                Err(e) => panic!("unexpected: {e}"),
            }
        };
        assert_eq!(pkt.seq, 3);
        assert_eq!(pkt.message, Message::ShellExit { code: 7 });
        assert_eq!(rx2.name(), "ipc-stream");
    }

    #[test]
    fn connect_at_nonexistent_path_returns_none() {
        // A path with no listening socket → Ok(None) (fallback signal), never
        // a hard error.
        let path = std::env::temp_dir().join("wiredesk-no-such-socket-xyz.sock");
        let _ = std::fs::remove_file(&path);
        let result = IpcStreamTransport::connect_at(&path).unwrap();
        assert!(result.is_none(), "expected Ok(None) for absent socket");
    }

    #[test]
    fn recv_after_peer_close_reports_disconnect() {
        let (a, b) = UnixStream::pair().expect("pair");
        let mut rx = IpcStreamTransport::from_stream(b).unwrap();
        drop(a); // peer hangs up after the handle is set up
        // EOF surfaces as a Transport error and marks the handle disconnected.
        let err = rx.recv().unwrap_err();
        match err {
            WireDeskError::Transport(m) => assert!(m.contains("closed") || m.contains("read"), "got: {m}"),
            other => panic!("expected Transport, got {other:?}"),
        }
        assert!(!rx.is_connected());
    }
}
