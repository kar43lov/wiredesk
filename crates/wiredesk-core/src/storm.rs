//! Frame-error storm detector for the serial link.
//!
//! When one of the FT232H chips physically glitches (clock/sampling
//! drifts), both directions of the null-modem start corrupting
//! systematically: COBS errors at a fixed position, CRC mismatches,
//! bit-flips in the magic byte (`0x44`→`0x45`). All of these surface as
//! `WireDeskError::Protocol`. The cure is known — close + reopen the
//! serial port to re-init the chip — but neither side does it on its own
//! (the Mac reader drops bad frames forever, the Win host falls into
//! `WaitingForHello` without touching the port).
//!
//! [`StormCounter`] turns a run of consecutive protocol errors into a
//! boolean "the channel is dead, reopen it" signal.
//!
//! **What counts as a storm:** `threshold` *consecutive* protocol errors
//! on recv. A storm emits ≥2 errors every 2s (corrupted heartbeats), so
//! the default threshold of 10 detects it within ~10s.
//!
//! **Why timeouts don't participate:** legitimate single bad frames (13–14
//! per day in the field logs, around connect time) are interleaved with
//! valid packets — any valid packet resets the counter, so they never
//! reach the threshold. recv timeout / idle errors are *not* protocol
//! corruption: they neither increment nor reset the counter. The counter
//! is reset *only* by a successfully decoded packet.

/// Default number of consecutive protocol errors that signals a storm.
pub const DEFAULT_STORM_THRESHOLD: u32 = 10;

/// Counts consecutive protocol (decode) errors on a transport's recv path.
///
/// Increment via [`on_protocol_error`](StormCounter::on_protocol_error) on
/// each `WireDeskError::Protocol`; reset via
/// [`on_valid_packet`](StormCounter::on_valid_packet) on each successfully
/// decoded packet. Timeout/idle recv errors must call neither.
#[derive(Debug, Clone)]
pub struct StormCounter {
    consecutive: u32,
    threshold: u32,
}

impl StormCounter {
    /// Create a counter that signals a storm at `threshold` consecutive
    /// protocol errors.
    pub fn new(threshold: u32) -> Self {
        Self {
            consecutive: 0,
            threshold,
        }
    }

    /// Record one protocol error. Returns `true` once the count of
    /// consecutive errors reaches the threshold (storm detected). Keeps
    /// returning `true` on subsequent errors while the storm persists.
    pub fn on_protocol_error(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive >= self.threshold
    }

    /// Record one successfully decoded packet — resets the run to 0.
    pub fn on_valid_packet(&mut self) {
        self.consecutive = 0;
    }

    /// Current count of consecutive protocol errors.
    pub fn count(&self) -> u32 {
        self.consecutive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_exactly_at_threshold() {
        let mut sc = StormCounter::new(DEFAULT_STORM_THRESHOLD);
        // threshold-1 errors → still false
        for _ in 0..DEFAULT_STORM_THRESHOLD - 1 {
            assert!(!sc.on_protocol_error());
        }
        assert_eq!(sc.count(), DEFAULT_STORM_THRESHOLD - 1);
        // threshold-th error → true
        assert!(sc.on_protocol_error());
        assert_eq!(sc.count(), DEFAULT_STORM_THRESHOLD);
    }

    #[test]
    fn valid_packet_resets_mid_run() {
        let mut sc = StormCounter::new(DEFAULT_STORM_THRESHOLD);
        for _ in 0..5 {
            assert!(!sc.on_protocol_error());
        }
        assert_eq!(sc.count(), 5);
        sc.on_valid_packet();
        assert_eq!(sc.count(), 0);
        // must take a full threshold run again to fire
        for _ in 0..DEFAULT_STORM_THRESHOLD - 1 {
            assert!(!sc.on_protocol_error());
        }
        assert!(sc.on_protocol_error());
    }

    #[test]
    fn keeps_firing_after_storm() {
        let mut sc = StormCounter::new(3);
        assert!(!sc.on_protocol_error());
        assert!(!sc.on_protocol_error());
        assert!(sc.on_protocol_error()); // 3 → fire
        assert!(sc.on_protocol_error()); // 4 → still fire
        assert!(sc.on_protocol_error()); // 5 → still fire
        assert_eq!(sc.count(), 5);
    }

    #[test]
    fn threshold_one_fires_immediately() {
        let mut sc = StormCounter::new(1);
        assert!(sc.on_protocol_error());
        assert_eq!(sc.count(), 1);
    }
}
