use std::time::{Duration, Instant};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::message::{Message, VERSION};
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

use crate::clipboard::{ClipboardSync, ProgressCounters};
use crate::injector::InputInjector;
use crate::shell::{ShellEvent, ShellProcess};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// Heartbeat timeout while the link is idle. 3 missed heartbeats — fast
/// enough for the user to notice an unplugged cable but loose enough to
/// tolerate a single dropped CRC.
const HEARTBEAT_TIMEOUT_IDLE: Duration = Duration::from_secs(6);
/// Heartbeat timeout while a clipboard transfer is in flight (incoming
/// reassembly armed or outgoing chunks queued). At 11 KB/s an 80–500 KB
/// image takes 7–45 s on the wire, during which the strict 6 s timeout
/// would falsely fire — the peer is busy receiving chunks and its
/// heartbeats can be queued behind ours. ×5 the idle timeout: enough
/// slack for ~3 MB of in-flight payload before we treat silence as a
/// real disconnect.
const HEARTBEAT_TIMEOUT_BUSY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SessionState {
    WaitingForHello,
    Connected,
    Disconnected,
}

pub struct Session<T: Transport, I: InputInjector> {
    transport: T,
    injector: I,
    state: SessionState,
    seq: u16,
    last_heartbeat_sent: Instant,
    last_heartbeat_recv: Instant,
    host_name: String,
    screen_w: u16,
    screen_h: u16,
    shell: Option<ShellProcess>,
    clipboard: ClipboardSync,
    /// Latest client display name reported via Hello (None until handshake).
    client_name: Option<String>,
}

impl<T: Transport, I: InputInjector> Session<T, I> {
    /// Convenience ctor with default (zero-init) progress counters.
    /// Used by the `#[cfg(test)]` fixtures; production wiring goes
    /// through `with_counters` so the overlay sees the same atomics.
    #[cfg(test)]
    pub fn new(transport: T, injector: I, host_name: String, screen_w: u16, screen_h: u16) -> Self {
        Self::with_counters(
            transport,
            injector,
            host_name,
            screen_w,
            screen_h,
            ProgressCounters::default(),
        )
    }

    pub fn with_counters(
        transport: T,
        injector: I,
        host_name: String,
        screen_w: u16,
        screen_h: u16,
        counters: ProgressCounters,
    ) -> Self {
        let now = Instant::now();
        Self {
            transport,
            injector,
            state: SessionState::WaitingForHello,
            seq: 0,
            last_heartbeat_sent: now,
            last_heartbeat_recv: now,
            host_name,
            screen_w,
            screen_h,
            shell: None,
            clipboard: ClipboardSync::with_counters(counters),
            client_name: None,
        }
    }

    pub fn current_state(&self) -> SessionState {
        self.state
    }

    pub fn client_name(&self) -> Option<&str> {
        self.client_name.as_deref()
    }

    #[cfg(test)]
    pub fn state(&self) -> SessionState {
        self.state
    }

    #[cfg(test)]
    pub fn clipboard_state(&self) -> &ClipboardSync {
        &self.clipboard
    }

    /// Drain any transient warning the clipboard layer queued up since the
    /// last call (e.g., "image too large"). The session thread forwards
    /// this to the tray UI as a balloon notification.
    pub fn take_clipboard_warning(&mut self) -> Option<String> {
        self.clipboard.take_warning()
    }

    #[cfg(test)]
    pub fn has_shell(&self) -> bool {
        self.shell.is_some()
    }

    /// Test-only: rewind `last_heartbeat_recv` so the next tick() sees the
    /// heartbeat-timeout branch and drives the disconnect cleanup path.
    #[cfg(test)]
    pub fn force_heartbeat_timeout(&mut self) {
        // Use the busy timeout so the rewind triggers regardless of which
        // branch the runtime check picks.
        self.last_heartbeat_recv =
            Instant::now() - HEARTBEAT_TIMEOUT_BUSY - Duration::from_secs(1);
    }

    /// Effective heartbeat timeout — extended while a clipboard transfer
    /// is in flight to avoid false-positive disconnects when the wire is
    /// saturated by chunk traffic.
    fn heartbeat_timeout(&self) -> Duration {
        if self.clipboard.transfer_in_flight() {
            HEARTBEAT_TIMEOUT_BUSY
        } else {
            HEARTBEAT_TIMEOUT_IDLE
        }
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    fn send(&mut self, msg: Message) -> Result<()> {
        let seq = self.next_seq();
        let packet = Packet::new(msg, seq);
        self.transport.send(&packet)
    }

    /// Process one incoming packet. Returns Ok(true) if packet was processed,
    /// Ok(false) if no packet available (timeout), Err on fatal error.
    pub fn tick(&mut self) -> Result<bool> {
        // Send heartbeat if needed
        if self.state == SessionState::Connected
            && self.last_heartbeat_sent.elapsed() >= HEARTBEAT_INTERVAL
        {
            self.send(Message::Heartbeat)?;
            self.last_heartbeat_sent = Instant::now();
        }

        // Check heartbeat timeout
        if self.state == SessionState::Connected
            && self.last_heartbeat_recv.elapsed() >= self.heartbeat_timeout()
        {
            log::warn!("heartbeat timeout — disconnecting");
            self.injector.release_all()?;
            self.shell_kill();
            self.clipboard.reset();
            self.state = SessionState::WaitingForHello;
            self.client_name = None;
            return Ok(false);
        }

        // Drain pending shell output and exit events without blocking
        self.pump_shell_events()?;

        // Push local clipboard changes (poll-rate-limited internally).
        if self.state == SessionState::Connected {
            for msg in self.clipboard.poll() {
                self.send(msg)?;
            }
        }

        // Try to receive a packet
        let packet = match self.transport.recv() {
            Ok(p) => p,
            Err(WireDeskError::Transport(ref msg)) if msg.contains("timeout") => {
                return Ok(false);
            }
            Err(e) => return Err(e),
        };

        self.handle_packet(packet)?;
        Ok(true)
    }

    /// Drain any stdout/stderr from running shell into outbound packets.
    /// Also detects shell exit and notifies the client.
    fn pump_shell_events(&mut self) -> Result<()> {
        if self.shell.is_none() {
            return Ok(());
        }

        // Up to N events per tick to avoid starving recv()
        const MAX_PER_TICK: usize = 16;
        let mut outputs: Vec<Vec<u8>> = Vec::new();
        let mut exit_code: Option<i32> = None;

        if let Some(sh) = self.shell.as_ref() {
            for _ in 0..MAX_PER_TICK {
                match sh.events_rx.try_recv() {
                    Ok(ShellEvent::Output(data)) => outputs.push(data),
                    Ok(ShellEvent::Exit(code)) => {
                        exit_code = Some(code);
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }

        for chunk in outputs {
            // Split into max-payload-sized chunks (512 bytes is the protocol limit)
            for piece in chunk.chunks(480) {
                self.send(Message::ShellOutput { data: piece.to_vec() })?;
            }
        }

        // Detect process exit even if we didn't get an Exit event
        if exit_code.is_none() {
            if let Some(sh) = self.shell.as_mut() {
                exit_code = sh.try_exit_code();
            }
        }

        if let Some(code) = exit_code {
            log::info!("shell exited with code {code}");
            self.shell = None;
            self.send(Message::ShellExit { code })?;
        }

        Ok(())
    }

    fn shell_kill(&mut self) {
        if let Some(mut sh) = self.shell.take() {
            sh.kill();
        }
    }

    fn handle_packet(&mut self, packet: Packet) -> Result<()> {
        match (&self.state, &packet.message) {
            (SessionState::WaitingForHello, Message::Hello { version, client_name }) => {
                if *version != VERSION {
                    log::warn!("version mismatch: client v{version}, host v{VERSION}");
                    self.send(Message::Error {
                        code: 1,
                        msg: format!("unsupported version {version}, expected {VERSION}"),
                    })?;
                    return Ok(());
                }
                log::info!("HELLO from '{client_name}' v{version}");
                self.client_name = Some(client_name.clone());
                self.send(Message::HelloAck {
                    version: VERSION,
                    host_name: self.host_name.clone(),
                    screen_w: self.screen_w,
                    screen_h: self.screen_h,
                })?;
                self.state = SessionState::Connected;
                self.last_heartbeat_recv = Instant::now();
                log::info!("connected (screen: {}x{})", self.screen_w, self.screen_h);
            }

            (SessionState::Connected, Message::Heartbeat) => {
                self.last_heartbeat_recv = Instant::now();
            }

            (SessionState::Connected, Message::MouseMove { x, y }) => {
                self.injector.mouse_move_absolute(*x, *y)?;
            }

            (SessionState::Connected, Message::MouseButton { button, pressed }) => {
                self.injector.mouse_button(*button, *pressed)?;
            }

            (SessionState::Connected, Message::MouseScroll { delta_x, delta_y }) => {
                self.injector.mouse_scroll(*delta_x, *delta_y)?;
            }

            (SessionState::Connected, Message::KeyDown { scancode, modifiers }) => {
                self.injector.key_down(*scancode, *modifiers)?;
            }

            (SessionState::Connected, Message::KeyUp { scancode, modifiers }) => {
                self.injector.key_up(*scancode, *modifiers)?;
            }

            (SessionState::Connected, Message::ClipOffer { format, total_len }) => {
                self.clipboard.on_offer(*format, *total_len);
            }

            (SessionState::Connected, Message::ClipChunk { index, data }) => {
                self.clipboard.on_chunk(*index, data.clone());
            }

            (SessionState::Connected, Message::ClipDecline { format }) => {
                // Peer doesn't want this transfer (its receive_* toggle is
                // off). Drop the pending outbox so we stop saturating the
                // wire with chunks the peer is going to discard. Without
                // this the link's RX direction stays full of data the peer
                // ignores, starving its TX (mouse, heartbeats, decline
                // ack itself) and triggering a heartbeat timeout.
                let dropped = self.clipboard.cancel_outgoing();
                if dropped > 0 {
                    log::info!(
                        "clipboard: peer declined offer (format={format}); dropped {dropped} queued packets"
                    );
                }
            }

            (SessionState::Connected, Message::ShellOpen { shell }) => {
                if self.shell.is_some() {
                    log::warn!("ShellOpen received but a shell is already running");
                    self.send(Message::Error {
                        code: 2,
                        msg: "shell already open".into(),
                    })?;
                } else {
                    log::info!("opening shell '{shell}'");
                    match ShellProcess::spawn(shell, None) {
                        Ok(proc) => self.shell = Some(proc),
                        Err(e) => {
                            log::error!("failed to spawn shell: {e}");
                            self.send(Message::Error {
                                code: 3,
                                msg: format!("shell spawn: {e}"),
                            })?;
                        }
                    }
                }
            }

            (SessionState::Connected, Message::ShellOpenPty { shell, cols, rows }) => {
                if self.shell.is_some() {
                    log::warn!("ShellOpenPty received but a shell is already running");
                    self.send(Message::Error {
                        code: 2,
                        msg: "shell already open".into(),
                    })?;
                } else {
                    log::info!("opening pty shell '{shell}' ({cols}x{rows})");
                    match ShellProcess::spawn(shell, Some((*cols, *rows))) {
                        Ok(proc) => self.shell = Some(proc),
                        Err(e) => {
                            log::error!("failed to spawn pty shell: {e}");
                            self.send(Message::Error {
                                code: 3,
                                msg: format!("pty shell spawn: {e}"),
                            })?;
                        }
                    }
                }
            }

            (SessionState::Connected, Message::PtyResize { cols, rows }) => {
                if let Some(sh) = self.shell.as_ref() {
                    sh.resize(*cols, *rows);
                }
                // No shell open → silently ignore. Pre-spawn resize is
                // a benign race when client computes initial size in
                // parallel with ShellOpenPty.
            }

            (SessionState::Connected, Message::ShellInput { data }) => {
                if let Some(sh) = self.shell.as_ref() {
                    if !sh.write(data.clone()) {
                        log::warn!("shell stdin writer is gone");
                    }
                }
            }

            (SessionState::Connected, Message::ShellClose) => {
                // Close stdin first so the shell sees EOF — bash/zsh
                // (and most well-behaved CLIs) exit cleanly. PowerShell
                // launched with -NoExit ignores stdin EOF and keeps
                // running, leaving `self.shell` stuck at `Some(...)`
                // which then makes the *next* ShellOpen fail with
                // "shell already open". Force-kill after the close so
                // the slot is always free when the client re-opens.
                if let Some(sh) = self.shell.as_ref() {
                    sh.close();
                }
                self.shell_kill();
            }

            (SessionState::Connected, Message::Disconnect) => {
                log::info!("client disconnected");
                self.injector.release_all()?;
                self.shell_kill();
                self.clipboard.reset();
                self.state = SessionState::WaitingForHello;
                self.client_name = None;
            }

            (_, Message::Hello { .. }) => {
                // Re-handshake from any state — drop in-flight clipboard
                // reassembly so a half-finished transfer doesn't leak
                // across sessions, AND kill any leftover shell so the
                // new client's ShellOpen doesn't bounce off "shell
                // already open" (typical when the previous wiredesk-term
                // exited too fast for heartbeat-timeout to fire).
                self.injector.release_all().ok();
                self.clipboard.reset();
                self.shell_kill();
                self.state = SessionState::WaitingForHello;
                self.client_name = None;
                self.handle_packet(packet)?;
            }

            (state, msg) => {
                log::debug!("ignored {msg:?} in state {state:?}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injector::MockInjector;
    use wiredesk_transport::mock::MockTransport;

    fn setup() -> (Session<MockTransport, MockInjector>, MockTransport) {
        let (host_transport, client_transport) = MockTransport::pair();
        let injector = MockInjector::default();
        let session = Session::new(host_transport, injector, "test-host".into(), 1920, 1080);
        (session, client_transport)
    }

    #[test]
    fn handshake() {
        let (mut session, mut client) = setup();
        assert_eq!(session.state(), SessionState::WaitingForHello);

        // Client sends HELLO
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "test".into() },
            0,
        )).unwrap();

        session.tick().unwrap();
        assert_eq!(session.state(), SessionState::Connected);

        // Host should have sent HELLO_ACK
        let ack = client.recv().unwrap();
        match ack.message {
            Message::HelloAck { screen_w, screen_h, .. } => {
                assert_eq!(screen_w, 1920);
                assert_eq!(screen_h, 1080);
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    #[test]
    fn input_forwarding() {
        let (mut session, mut client) = setup();

        // Handshake first
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "test".into() },
            0,
        )).unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        // Send mouse move
        client.send(&Packet::new(Message::MouseMove { x: 100, y: 200 }, 1)).unwrap();
        session.tick().unwrap();

        // Send key
        client.send(&Packet::new(Message::KeyDown { scancode: 0x1E, modifiers: 0x01 }, 2)).unwrap();
        session.tick().unwrap();

        // Verify injector received events
        // Note: we need to access injector through session, but it's moved in.
        // For now, just verify no errors occurred.
    }

    #[test]
    fn disconnect_releases_keys() {
        let (mut session, mut client) = setup();

        // Handshake
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "test".into() },
            0,
        )).unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        // Disconnect
        client.send(&Packet::new(Message::Disconnect, 1)).unwrap();
        session.tick().unwrap();

        assert_eq!(session.state(), SessionState::WaitingForHello);
    }

    #[test]
    fn rehandshake() {
        let (mut session, mut client) = setup();

        // First handshake
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "first".into() },
            0,
        )).unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();
        assert_eq!(session.state(), SessionState::Connected);

        // Second HELLO (reconnect)
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "second".into() },
            0,
        )).unwrap();
        session.tick().unwrap();
        assert_eq!(session.state(), SessionState::Connected);

        // Should get a new HELLO_ACK
        let ack = client.recv().unwrap();
        assert!(matches!(ack.message, Message::HelloAck { .. }));
    }

    /// Bring `session` to Connected and seed an in-flight reassembly
    /// (one ClipOffer + one ClipChunk). Returns the live client transport
    /// so the caller can keep driving messages.
    fn setup_with_partial_reassembly() -> (Session<MockTransport, MockInjector>, MockTransport) {
        let (mut session, mut client) = setup();
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "test".into() },
            0,
        )).unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        // Push a partial reassembly: 1024-byte text offer + one 256-byte chunk.
        client.send(&Packet::new(Message::ClipOffer { format: 0, total_len: 1024 }, 1)).unwrap();
        session.tick().unwrap();
        client.send(&Packet::new(Message::ClipChunk { index: 0, data: vec![b'a'; 256] }, 2)).unwrap();
        session.tick().unwrap();

        assert_eq!(session.clipboard_state().expected_len(), 1024,
            "precondition: in-flight reassembly must be active");
        (session, client)
    }

    #[test]
    fn heartbeat_timeout_resets_clipboard() {
        // Heartbeat-timeout branch must drop in-flight reassembly so a
        // half-finished transfer doesn't leak into the next session.
        let (mut session, _client) = setup_with_partial_reassembly();

        session.force_heartbeat_timeout();
        let _ = session.tick(); // returns Ok(false) once the timeout fires

        assert_eq!(session.clipboard_state().expected_len(), 0,
            "heartbeat timeout must reset clipboard reassembly");
    }

    #[test]
    fn disconnect_resets_clipboard() {
        // Message::Disconnect must drop in-flight reassembly.
        let (mut session, mut client) = setup_with_partial_reassembly();

        client.send(&Packet::new(Message::Disconnect, 3)).unwrap();
        session.tick().unwrap();

        assert_eq!(session.clipboard_state().expected_len(), 0,
            "Disconnect must reset clipboard reassembly");
    }

    #[test]
    fn rehandshake_resets_clipboard() {
        // A fresh Hello during an active session must drop in-flight
        // reassembly so the new session starts clean.
        let (mut session, mut client) = setup_with_partial_reassembly();

        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "second".into() },
            3,
        )).unwrap();
        session.tick().unwrap();

        assert_eq!(session.clipboard_state().expected_len(), 0,
            "re-handshake must reset clipboard reassembly");
    }

    #[test]
    fn pty_resize_without_shell_is_silent_noop() {
        // Pre-spawn PtyResize is benign (client computes size in parallel
        // with ShellOpenPty). Session must accept it, ignore it, and
        // not respond with Error.
        let (mut session, mut client) = setup();
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "test".into() },
            0,
        )).unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        client.send(&Packet::new(Message::PtyResize { cols: 80, rows: 24 }, 1)).unwrap();
        session.tick().unwrap();

        // Sending another packet should still work (no protocol breakage).
        client.send(&Packet::new(Message::Heartbeat, 2)).unwrap();
        session.tick().unwrap();
        assert_eq!(session.state(), SessionState::Connected);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn shell_open_pty_on_non_windows_returns_error_to_client() {
        // PTY-backed shell is Windows-only. A Mac/Linux host must
        // surface the spawn error back to the client through the
        // existing Message::Error path — silent fallback to pipe-mode
        // would mask a misconfigured deployment.
        let (mut session, mut client) = setup();
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "test".into() },
            0,
        )).unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        assert!(!session.has_shell());

        client.send(&Packet::new(
            Message::ShellOpenPty { shell: "/bin/sh".into(), cols: 80, rows: 24 },
            1,
        )).unwrap();
        session.tick().unwrap();

        assert!(!session.has_shell(), "non-Windows host must refuse PTY shell");

        // Host should have sent an Error packet — drain heartbeats /
        // other messages, but expect at least one Error.
        let mut saw_error = false;
        for _ in 0..8 {
            match client.recv() {
                Ok(p) => {
                    if matches!(p.message, Message::Error { .. }) {
                        saw_error = true;
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        assert!(saw_error, "expected Message::Error from non-Windows pty-spawn");
    }
}
