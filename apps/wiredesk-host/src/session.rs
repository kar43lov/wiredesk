use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_core::storm::{StormCounter, DEFAULT_STORM_THRESHOLD};
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

/// Pure helper — pick busy or idle heartbeat budget. Extracted so the
/// branching can be unit-tested without spawning a real `ShellProcess`
/// (which forks PowerShell on Windows and isn't reachable from CI).
fn heartbeat_timeout_for(clipboard_busy: bool, shell_open: bool) -> Duration {
    if clipboard_busy || shell_open {
        HEARTBEAT_TIMEOUT_BUSY
    } else {
        HEARTBEAT_TIMEOUT_IDLE
    }
}

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
    /// Frame-error storm detector. Incremented on each `Protocol` recv error
    /// (via `note_protocol_error`), reset on each successfully decoded
    /// packet (in `tick`). When it fires, `session_thread` reopens the port.
    storm: StormCounter,
}

impl<T: Transport, I: InputInjector> Session<T, I> {
    /// Convenience ctor with default (zero-init) progress counters and a
    /// default-on `receive_files` toggle. Used by the `#[cfg(test)]` fixtures;
    /// production wiring goes through `with_counters_and_toggles` directly so
    /// the overlay sees the same atomics.
    #[cfg(test)]
    pub fn new(transport: T, injector: I, host_name: String, screen_w: u16, screen_h: u16) -> Self {
        Self::with_counters_and_toggles(
            transport,
            injector,
            host_name,
            screen_w,
            screen_h,
            ProgressCounters::default(),
            Arc::new(AtomicBool::new(true)),
        )
    }

    /// Full ctor: progress counters plus a `receive_files` runtime toggle
    /// threaded through to `ClipboardSync::with_counters_and_toggles`. Production
    /// session-thread spawn wires the toggle from `HostConfig.receive_files`
    /// so a `false` in TOML disables incoming `FORMAT_FILE` offers at boot;
    /// the Settings UI's Save-and-Restart respawns the host process, so the
    /// flag isn't live-mutable from outside the session loop (matches the
    /// existing send_images/receive_images pattern on the Mac side, which
    /// uses Arc only because the Mac UI has live config without restart).
    pub fn with_counters_and_toggles(
        transport: T,
        injector: I,
        host_name: String,
        screen_w: u16,
        screen_h: u16,
        counters: ProgressCounters,
        receive_files: Arc<AtomicBool>,
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
            clipboard: ClipboardSync::with_counters_and_toggles(counters, receive_files),
            client_name: None,
            storm: StormCounter::new(DEFAULT_STORM_THRESHOLD),
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
    #[allow(dead_code)] // consumed by tests that are themselves platform-gated
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

    /// Effective heartbeat timeout — extended while wire is saturated, so
    /// false-positive disconnects don't kill the channel mid-transfer.
    /// Two saturation sources today:
    ///
    /// 1. **Clipboard** — chunked image / large text transfer in flight.
    /// 2. **Shell** — `wd --exec` (especially `--ssh ALIAS curl ...`)
    ///    streaming command output back. With CH340 @ 115200 baud the wire
    ///    runs at ~11 KB/s; a 24 KB ES `_search` response monopolises it
    ///    for ~2 seconds plus any server-side delay, and the client's
    ///    writer thread (which also services heartbeats) can fall further
    ///    behind if it's writing back too. With the prior 6s idle timeout
    ///    the channel reliably tore down on every non-trivial ES read.
    ///    Live-test 2026-05-06: 43s between `opening shell` and
    ///    `heartbeat timeout — disconnecting` for an ES `_search?size=1`
    ///    query (~24 KB JSON response). With the busy budget (30s) plus
    ///    the natural prefix of MOTD / ssh hop the channel survives.
    ///
    /// `self.shell.is_some()` is the simplest signal — true between
    /// `ShellOpen` and `ShellClose`. Worst case if the shell is genuinely
    /// idle (e.g., user opened a shell and walked away), we wait 30s
    /// instead of 6s before tearing down. Acceptable.
    fn heartbeat_timeout(&self) -> Duration {
        heartbeat_timeout_for(
            self.clipboard.transfer_in_flight(),
            self.shell.is_some(),
        )
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

        // A real packet decoded → the channel is alive; clear the storm run
        // BEFORE handling (Codex iter3 P3): a handler error (e.g. injector
        // failure on a key event) returns early via `?`, and a decoded frame
        // must still break the protocol-error streak — the wire is fine, the
        // failure is local. This is the SINGLE reset site: the other Ok-paths
        // of tick() (heartbeat-timeout, recv-timeout) return without a decoded
        // packet, and resetting there would break "timeouts don't participate".
        self.storm.on_valid_packet();
        self.handle_packet(packet)?;
        Ok(true)
    }

    /// Record one protocol (decode) error from the recv path. Returns `true`
    /// once the consecutive-error run reaches the storm threshold, signalling
    /// `session_thread` to reopen the transport. Delegates to the internal
    /// [`StormCounter`]; `tick` resets the run on every decoded packet.
    pub fn note_protocol_error(&mut self) -> bool {
        self.storm.on_protocol_error()
    }

    /// Current consecutive protocol-error count (test/diagnostic hook).
    #[cfg(test)]
    pub fn storm_count(&self) -> u32 {
        self.storm.count()
    }

    /// Decompose the session, returning the injector and dropping the
    /// transport (which releases the underlying COM-port handle). Used by
    /// the reopen loop: the injector is built once via a `FnOnce` and must
    /// survive across transport reopens, so the old session is dismantled
    /// and the injector migrates into the freshly-opened one.
    pub fn into_injector(self) -> I {
        self.injector
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
                if let Some(decline) = self.clipboard.on_offer(*format, *total_len) {
                    // Forward the policy-decline back to the peer so it
                    // drops its outbox and stops streaming chunks we're
                    // going to discard — without this the link's RX
                    // direction stays full of data we ignore, starving
                    // TX (mouse, heartbeats, the decline itself).
                    self.send(decline)?;
                }
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
                // Task 7d: surface a tray-balloon for FORMAT_FILE so the
                // user has parity with the Mac toast "Peer declined file".
                // Other formats already had no UI feedback historically —
                // keeping that behaviour to avoid noise.
                if *format == wiredesk_protocol::message::FORMAT_FILE {
                    self.clipboard.push_warning(
                        "Peer declined file (Receive files off)".into(),
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
    fn heartbeat_timeout_for_pure_logic() {
        // Idle: no clipboard, no shell — strict 6s.
        assert_eq!(
            heartbeat_timeout_for(false, false),
            HEARTBEAT_TIMEOUT_IDLE,
            "with both flags false the budget must be IDLE so unplugged cables fire fast"
        );

        // Clipboard transfer alive — busy budget. (Pre-existing behaviour.)
        assert_eq!(
            heartbeat_timeout_for(true, false),
            HEARTBEAT_TIMEOUT_BUSY,
            "clipboard transfer must extend the budget"
        );

        // Shell open (no clipboard) — busy budget. (Regression for the
        // 2026-05-06 ES `_search` channel-tear-down: 24 KB JSON response
        // monopolised the wire long enough to miss the IDLE deadline.)
        assert_eq!(
            heartbeat_timeout_for(false, true),
            HEARTBEAT_TIMEOUT_BUSY,
            "open shell must extend the budget — wire saturation kills the IDLE deadline"
        );

        // Both — still busy.
        assert_eq!(
            heartbeat_timeout_for(true, true),
            HEARTBEAT_TIMEOUT_BUSY,
        );
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

    #[test]
    fn storm_fires_after_threshold_consecutive_errors() {
        let (mut session, _client) = setup();
        // threshold-1 reports → no storm yet
        for _ in 0..DEFAULT_STORM_THRESHOLD - 1 {
            assert!(!session.note_protocol_error());
        }
        assert_eq!(session.storm_count(), DEFAULT_STORM_THRESHOLD - 1);
        // threshold-th report → storm
        assert!(session.note_protocol_error());
        assert_eq!(session.storm_count(), DEFAULT_STORM_THRESHOLD);
    }

    #[test]
    fn storm_resets_on_valid_packet_via_tick() {
        let (mut session, mut client) = setup();
        // Handshake so subsequent packets are processed in Connected state.
        client.send(&Packet::new(
            Message::Hello { version: 1, client_name: "test".into() },
            0,
        )).unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();
        assert_eq!(session.storm_count(), 0, "handshake decode already reset");

        // Accumulate a partial storm run.
        for _ in 0..5 {
            assert!(!session.note_protocol_error());
        }
        assert_eq!(session.storm_count(), 5);

        // A real decoded packet (heartbeat) through tick() must reset the run.
        client.send(&Packet::new(Message::Heartbeat, 1)).unwrap();
        session.tick().unwrap();
        assert_eq!(session.storm_count(), 0, "decoded packet must reset storm run");
    }

    #[test]
    fn storm_count_persists_without_valid_packet() {
        // The storm run is reset ONLY by a decoded packet (via tick's
        // on_valid_packet call). Nothing else — including a heartbeat-timeout
        // or recv-timeout, which both return Ok(false) without decoding —
        // touches it. MockTransport::recv() blocks instead of timing out, so
        // we can't drive an empty tick here without hanging; the invariant
        // we assert is that the counter only moves via note_protocol_error /
        // on_valid_packet and never self-resets between error reports.
        let (mut session, _client) = setup();
        for _ in 0..3 {
            session.note_protocol_error();
        }
        assert_eq!(session.storm_count(), 3);
        // More errors with no interleaved valid packet keep climbing.
        for _ in 0..2 {
            session.note_protocol_error();
        }
        assert_eq!(session.storm_count(), 5, "run must persist without a decoded packet");
    }
}
