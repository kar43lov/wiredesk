//! Abstract transport for the runner — decouples the sentinel-driven
//! state machine from the underlying byte-pipe (direct serial in
//! `wiredesk-term`, mpsc-bridged in the GUI's IPC handler).

use std::time::Duration;

use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::{Packet, MAX_PAYLOAD};

use crate::types::{ExecError, ExecEvent};

/// Largest `ShellInput` body one wire packet carries. The message encodes
/// as the raw bytes and nothing else, so the cap is exactly `MAX_PAYLOAD`.
pub const MAX_INPUT_CHUNK: usize = MAX_PAYLOAD;

/// Split shell input into wire-sized `ShellInput` packets.
///
/// `Packet::to_bytes` refuses a payload over `MAX_PAYLOAD`, and a refused
/// `ShellInput` never reaches the host — the runner then waits for a
/// sentinel that cannot arrive and exits 124 after the full timeout. One
/// week of client logs showed 17 such hangs, every one a base64-wrapped
/// script of 4.2–7.3 KB. The host feeds the packets to the shell's stdin
/// in order, so consecutive pieces concatenate transparently; every
/// `ExecTransport::send_input` must go through here.
pub fn shell_input_packets(data: &[u8]) -> impl Iterator<Item = Packet> + '_ {
    data.chunks(MAX_INPUT_CHUNK).map(|chunk| {
        Packet::new(
            Message::ShellInput {
                data: chunk.to_vec(),
            },
            0,
        )
    })
}

/// Two-method trait the runner depends on. Implementations decide how
/// to write input and how to surface incoming `ShellOutput` /
/// `ShellExit` / host-error frames as `ExecEvent`s.
///
/// `recv_event` differentiates idle (no data this tick) from permanent
/// closure: idle returns `Ok(ExecEvent::Idle)` so the runner can re-
/// check its overall timeout, closed returns `Err(ExecError::Closed)`
/// so the runner can fail fast.
pub trait ExecTransport {
    fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError>;
    fn recv_event(&mut self, timeout: Duration) -> Result<ExecEvent, ExecError>;
}

#[cfg(test)]
pub mod mock {
    //! In-memory `ExecTransport` for unit tests of the runner.

    use std::collections::VecDeque;
    use std::time::Duration;

    use super::ExecTransport;
    use crate::types::{ExecError, ExecEvent};

    /// Test double: replays a queue of pre-loaded events on `recv_event`,
    /// records every `send_input` byte-vector into `outbox`. Once the
    /// queue empties, further `recv_event` calls return `Idle` until
    /// `closed` flag is flipped (then `Err(Closed)`). Useful for table-
    /// driven runner tests.
    pub struct MockExecTransport {
        pub outbox: Vec<Vec<u8>>,
        pub events: VecDeque<ExecEvent>,
        pub closed: bool,
    }

    impl MockExecTransport {
        pub fn new(events: impl IntoIterator<Item = ExecEvent>) -> Self {
            Self {
                outbox: Vec::new(),
                events: events.into_iter().collect(),
                closed: false,
            }
        }
    }

    impl ExecTransport for MockExecTransport {
        fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
            if self.closed {
                return Err(ExecError::Closed);
            }
            self.outbox.push(data.to_vec());
            Ok(())
        }

        fn recv_event(&mut self, _timeout: Duration) -> Result<ExecEvent, ExecError> {
            match self.events.pop_front() {
                Some(ev) => Ok(ev),
                None if self.closed => Err(ExecError::Closed),
                None => Ok(ExecEvent::Idle),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn enqueued_events_replay_in_order() {
            let mut t = MockExecTransport::new([
                ExecEvent::ShellOutput(b"hello".to_vec()),
                ExecEvent::ShellExit(0),
            ]);
            assert!(matches!(
                t.recv_event(Duration::from_millis(10)).unwrap(),
                ExecEvent::ShellOutput(ref b) if b == b"hello"
            ));
            assert!(matches!(
                t.recv_event(Duration::from_millis(10)).unwrap(),
                ExecEvent::ShellExit(0)
            ));
        }

        #[test]
        fn empty_queue_returns_idle() {
            let mut t = MockExecTransport::new(std::iter::empty());
            assert!(matches!(
                t.recv_event(Duration::from_millis(10)).unwrap(),
                ExecEvent::Idle
            ));
        }

        #[test]
        fn closed_with_empty_queue_yields_closed() {
            let mut t = MockExecTransport::new(std::iter::empty());
            t.closed = true;
            assert!(matches!(
                t.recv_event(Duration::from_millis(10)),
                Err(ExecError::Closed)
            ));
        }

        #[test]
        fn send_input_records_outbox() {
            let mut t = MockExecTransport::new(std::iter::empty());
            t.send_input(b"first").unwrap();
            t.send_input(b"second").unwrap();
            assert_eq!(t.outbox.len(), 2);
            assert_eq!(t.outbox[0], b"first");
            assert_eq!(t.outbox[1], b"second");
        }

        #[test]
        fn send_input_after_close_errors() {
            let mut t = MockExecTransport::new(std::iter::empty());
            t.closed = true;
            assert!(matches!(t.send_input(b"x"), Err(ExecError::Closed)));
        }

        #[test]
        fn drain_then_close_flips_idle_to_closed() {
            // Real-world path: queue drains, then the underlying mpsc
            // sender drops — recv_event must transition from Idle to
            // Err(Closed) without losing already-buffered events.
            let mut t = MockExecTransport::new([ExecEvent::ShellOutput(b"x".to_vec())]);
            assert!(matches!(
                t.recv_event(Duration::from_millis(10)).unwrap(),
                ExecEvent::ShellOutput(_)
            ));
            assert!(matches!(
                t.recv_event(Duration::from_millis(10)).unwrap(),
                ExecEvent::Idle
            ));
            t.closed = true;
            assert!(matches!(
                t.recv_event(Duration::from_millis(10)),
                Err(ExecError::Closed)
            ));
        }
    }
}

#[cfg(test)]
mod chunk_tests {
    use super::*;

    fn bodies(data: &[u8]) -> Vec<Vec<u8>> {
        shell_input_packets(data)
            .map(|p| match p.message {
                Message::ShellInput { data } => data,
                other => panic!("expected ShellInput, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn small_input_is_one_packet() {
        let out = bodies(b"Get-ChildItem\n");
        assert_eq!(out, vec![b"Get-ChildItem\n".to_vec()]);
    }

    #[test]
    fn exactly_max_payload_stays_whole() {
        let data = vec![0x41; MAX_INPUT_CHUNK];
        let out = bodies(&data);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].len(), MAX_INPUT_CHUNK);
    }

    #[test]
    fn one_byte_over_splits_in_two() {
        let data = vec![0x41; MAX_INPUT_CHUNK + 1];
        let out = bodies(&data);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), MAX_INPUT_CHUNK);
        assert_eq!(out[1].len(), 1);
    }

    #[test]
    fn pieces_concatenate_to_the_original_in_order() {
        // 7 296 bytes — the largest oversize command seen in the logs.
        let data: Vec<u8> = (0..7296u32).map(|i| (i % 251) as u8).collect();
        let out = bodies(&data);
        assert_eq!(out.len(), 2);
        let joined: Vec<u8> = out.concat();
        assert_eq!(joined, data);
    }

    #[test]
    fn every_piece_encodes_on_the_wire() {
        // The whole point: no piece may trip `to_bytes`'s payload cap.
        let data = vec![0x42; 3 * MAX_INPUT_CHUNK + 17];
        for packet in shell_input_packets(&data) {
            packet.to_bytes().expect("chunk must fit a wire packet");
        }
    }

    #[test]
    fn empty_input_sends_nothing() {
        assert!(bodies(b"").is_empty());
    }
}
