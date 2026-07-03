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
//!
//! **Timeout-safe framing.** Because `recv` runs under a read timeout for the
//! whole session, a stateless `read_exact`-based frame reader would be unsafe:
//! if the timeout lands *after* a frame's length prefix (or part of its body)
//! has already been consumed, `read_exact` discards those bytes and the next
//! `recv()` starts mid-frame → the length-prefixed stream desyncs and every
//! later packet decodes as garbage. Instead `recv` accumulates raw bytes into a
//! persistent `rx_buf` via single `read()` calls (which never discard the bytes
//! they return) and only yields a `Packet` once a full frame is buffered; a
//! mid-frame timeout returns `"recv timeout"` with the partial frame preserved.

use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_exec_core::ipc::{write_connect, write_packet_frame, IpcConnect, MAX_FRAME_BYTES};
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

/// Length-prefix header width (u32 BE), matching `write_frame` in
/// `wiredesk-exec-core::ipc`.
const FRAME_LEN_PREFIX: usize = 4;

/// Per-`read()` chunk size. A single interactive frame is a `ShellOutput`
/// (≤ `MAX_PAYLOAD` + header ≈ 4 KB), so 8 KB usually assembles a whole frame
/// in one read; larger frames simply loop and accumulate.
const READ_CHUNK: usize = 8192;

/// Read timeout for `recv`. Short enough that the reader thread wakes to
/// check `stop` promptly on Ctrl+] (so the `join()` returns without waiting
/// for the GUI to close the socket), long enough not to busy-spin.
const RECV_TIMEOUT: Duration = Duration::from_millis(100);

pub struct IpcStreamTransport {
    stream: UnixStream,
    /// Last hard IO result. Timeouts do NOT flip this (they're normal); only
    /// a real read/write failure marks the handle disconnected.
    connected: bool,
    /// Bytes read from the socket that don't yet form a complete frame. A read
    /// timeout mid-frame leaves the partial bytes here so the next `recv()`
    /// resumes exactly where it left off (see the module docstring on
    /// timeout-safe framing). Each reader/writer clone owns its own buffer —
    /// `try_clone` builds a fresh `IpcStreamTransport` with an empty `rx_buf`.
    rx_buf: Vec<u8>,
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
            rx_buf: Vec::new(),
        })
    }

    /// Try to split one complete frame off the front of `rx_buf`, decoding it
    /// into a `Packet`. Returns `Ok(None)` when fewer than a full frame's bytes
    /// are buffered (caller reads more). Enforces `MAX_FRAME_BYTES` on the
    /// length prefix and surfaces a corrupt body as a hard disconnect (the
    /// stream can't be re-synced once a frame boundary is wrong).
    fn take_buffered_frame(&mut self) -> Result<Option<Packet>> {
        if self.rx_buf.len() < FRAME_LEN_PREFIX {
            return Ok(None);
        }
        let len = u32::from_be_bytes([
            self.rx_buf[0],
            self.rx_buf[1],
            self.rx_buf[2],
            self.rx_buf[3],
        ]);
        if len > MAX_FRAME_BYTES {
            self.connected = false;
            return Err(WireDeskError::Transport(format!(
                "ipc frame length {len} exceeds {MAX_FRAME_BYTES}-byte cap"
            )));
        }
        let total = FRAME_LEN_PREFIX + len as usize;
        if self.rx_buf.len() < total {
            return Ok(None);
        }
        // Split the frame body out and keep any trailing bytes (a following
        // frame that arrived in the same read) for the next call.
        let rest = self.rx_buf.split_off(total);
        let body = &self.rx_buf[FRAME_LEN_PREFIX..total];
        let packet = Packet::from_bytes(body).map_err(|e| {
            self.connected = false;
            WireDeskError::Transport(format!("ipc packet decode: {e}"))
        })?;
        self.rx_buf = rest;
        Ok(Some(packet))
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

    /// Write the `IpcConnect` dispatch frame — the mandatory first frame of a
    /// connection, sent before any `Packet`s. Kept a distinct method (not
    /// `send`) because the dispatch frame is a bincode `IpcConnect`, not a wire
    /// `Packet`; `send` only ever carries `Packet`s after this frame. Used by
    /// the term's `try_interactive_socket_at` to announce `Interactive` mode.
    pub fn send_connect(&mut self, conn: &IpcConnect) -> Result<()> {
        write_connect(&mut self.stream, conn)
            .map_err(|e| WireDeskError::Transport(format!("ipc connect frame: {e}")))
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
        loop {
            // Yield a frame as soon as one is fully buffered — including a
            // frame left over from a previous read that also delivered the
            // next one.
            if let Some(packet) = self.take_buffered_frame()? {
                return Ok(packet);
            }
            // Need more bytes. A single `read()` returns whatever is available
            // (possibly a partial frame) without ever discarding it, so a
            // timeout here can't desync the stream — the partial bytes stay in
            // `rx_buf` for the next call.
            let mut chunk = [0u8; READ_CHUNK];
            match self.stream.read(&mut chunk) {
                // EOF (GUI closed the socket) — disconnect so the reader loop
                // tears the bridge down. Any partial frame in `rx_buf` is moot;
                // no more bytes are coming.
                Ok(0) => {
                    self.connected = false;
                    return Err(WireDeskError::Transport("ipc closed: eof".into()));
                }
                Ok(n) => self.rx_buf.extend_from_slice(&chunk[..n]),
                // A read timeout on an idle socket is normal — surface the exact
                // "recv timeout" shape the reader thread already tolerates so it
                // wakes to check `stop` (Ctrl+] exits cleanly). Not a disconnect;
                // `rx_buf` keeps any partial frame.
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    return Err(WireDeskError::Transport("recv timeout".into()));
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.connected = false;
                    return Err(WireDeskError::Transport(format!("ipc read: {e}")));
                }
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
    fn send_connect_writes_dispatch_frame_then_packets() {
        use wiredesk_exec_core::ipc::{read_connect, read_packet_frame, IpcInteractiveOpen};

        let (a, b) = UnixStream::pair().expect("pair");
        let mut tx = IpcStreamTransport::from_stream(a).unwrap();
        let mut rx_raw = b; // peer reads the raw dispatch frame first

        // First frame: the IpcConnect dispatch discriminator.
        tx.send_connect(&IpcConnect::Interactive(IpcInteractiveOpen {
            shell: "powershell.exe".into(),
            cols: 120,
            rows: 40,
        }))
        .unwrap();
        // Subsequent frames: plain Packets (Hello handshake).
        tx.send(&Packet::new(
            Message::Hello {
                version: 1,
                client_name: "mac-term".into(),
            },
            0,
        ))
        .unwrap();

        // Peer decodes the dispatch frame, then the packet frame — in order.
        match read_connect(&mut rx_raw).expect("read_connect") {
            IpcConnect::Interactive(open) => {
                assert_eq!(open.shell, "powershell.exe");
                assert_eq!(open.cols, 120);
                assert_eq!(open.rows, 40);
            }
            IpcConnect::Exec(req) => panic!("expected Interactive, got Exec: {req:?}"),
        }
        let pkt = read_packet_frame(&mut rx_raw).expect("packet after dispatch");
        assert!(matches!(pkt.message, Message::Hello { .. }));
    }

    #[test]
    fn recv_survives_mid_frame_timeout_without_desync() {
        // Regression: a read timeout landing *between* a frame's length prefix
        // and its body must NOT discard the consumed length bytes. With the old
        // `read_exact`-based reader this desynced the stream; the stateful
        // accumulator keeps the partial frame and reassembles it.
        use std::io::Write;
        use wiredesk_exec_core::ipc::write_packet_frame;

        let (mut writer_raw, b) = UnixStream::pair().expect("pair");
        let mut rx = IpcStreamTransport::from_stream(b).unwrap();

        // Encode a real frame, then feed it to the peer in two halves with a
        // recv() (which times out) in between.
        let mut framed = Vec::new();
        write_packet_frame(
            &mut framed,
            &Packet::new(Message::ShellInput { data: b"mid-frame".to_vec() }, 9),
        )
        .unwrap();
        // Split so the first write includes the 4-byte length prefix plus a
        // couple of body bytes — the exact case read_exact would have eaten.
        let split = FRAME_LEN_PREFIX + 2;
        writer_raw.write_all(&framed[..split]).unwrap();
        writer_raw.flush().unwrap();

        // First recv sees only a partial frame → must time out, not error or
        // consume-and-lose. The buffer retains the partial bytes.
        match rx.recv() {
            Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => {}
            other => panic!("expected recv timeout on partial frame, got {other:?}"),
        }
        assert!(rx.is_connected(), "partial frame must not disconnect");

        // Deliver the rest; the next recv reassembles the whole packet intact.
        writer_raw.write_all(&framed[split..]).unwrap();
        writer_raw.flush().unwrap();
        let pkt = loop {
            match rx.recv() {
                Ok(p) => break p,
                Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => continue,
                Err(e) => panic!("unexpected: {e}"),
            }
        };
        assert_eq!(pkt.seq, 9);
        assert_eq!(pkt.message, Message::ShellInput { data: b"mid-frame".to_vec() });
    }

    #[test]
    fn recv_yields_two_frames_from_one_read() {
        // Both frames arrive in a single write (hence one read()); recv must
        // return them one at a time, keeping the second buffered rather than
        // dropping it.
        let (a, b) = UnixStream::pair().expect("pair");
        let mut tx = IpcStreamTransport::from_stream(a).unwrap();
        let mut rx = IpcStreamTransport::from_stream(b).unwrap();

        tx.send(&Packet::new(Message::ShellExit { code: 1 }, 1)).unwrap();
        tx.send(&Packet::new(Message::ShellExit { code: 2 }, 2)).unwrap();

        let mut got = Vec::new();
        while got.len() < 2 {
            match rx.recv() {
                Ok(p) => got.push(p.seq),
                Err(WireDeskError::Transport(ref m)) if m.contains("timeout") => continue,
                Err(e) => panic!("unexpected: {e}"),
            }
        }
        assert_eq!(got, vec![1, 2]);
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
