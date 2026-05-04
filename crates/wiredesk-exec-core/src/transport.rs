//! Abstract transport for the runner — decouples the sentinel-driven
//! state machine from the underlying byte-pipe (direct serial in
//! `wiredesk-term`, mpsc-bridged in the GUI's IPC handler).

use std::time::Duration;

use crate::types::{ExecError, ExecEvent};

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
