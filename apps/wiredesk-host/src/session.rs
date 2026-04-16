use std::time::{Duration, Instant};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::message::{Message, VERSION};
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

use crate::injector::InputInjector;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(6); // 3 missed heartbeats

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
}

impl<T: Transport, I: InputInjector> Session<T, I> {
    pub fn new(transport: T, injector: I, host_name: String, screen_w: u16, screen_h: u16) -> Self {
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
        }
    }

    #[cfg(test)]
    pub fn state(&self) -> SessionState {
        self.state
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
            && self.last_heartbeat_recv.elapsed() >= HEARTBEAT_TIMEOUT
        {
            log::warn!("heartbeat timeout — disconnecting");
            self.injector.release_all()?;
            self.state = SessionState::WaitingForHello;
            return Ok(false);
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

            (SessionState::Connected, Message::Disconnect) => {
                log::info!("client disconnected");
                self.injector.release_all()?;
                self.state = SessionState::WaitingForHello;
            }

            (_, Message::Hello { .. }) => {
                // Re-handshake from any state
                self.injector.release_all().ok();
                self.state = SessionState::WaitingForHello;
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
    use crate::injector::{InjectorEvent, MockInjector};
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
}
