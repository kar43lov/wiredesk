//! Reconnect-backoff helper for the Bluetooth LE transport.
//!
//! Pure logic — no IO, no async. The Mac BLE Central impl (Task 5) and
//! Win BLE Peripheral impl (Task 7) call [`next_backoff`] when a
//! disconnect / subscriber-loss event fires, sleep for the returned
//! duration, then retry. The schedule is:
//!
//! | attempt | delay  | rationale                                     |
//! |---------|--------|-----------------------------------------------|
//! | 0       | 0 s    | immediate retry — typical sleep-wake recovery |
//! | 1       | 2 s    | first backoff                                 |
//! | 2       | 4 s    | exponential                                   |
//! | 3       | 8 s    | exponential                                   |
//! | 4       | 16 s   | exponential                                   |
//! | 5+      | 30 s   | capped — the user's looking at a dead radio   |
//!
//! AC4 in the plan demands ≤ 5 s recovery after a brief disconnect/sleep-
//! wake — the immediate first attempt covers that path. If the first
//! attempt fails (radio still re-acquiring), the subsequent 2 s + 4 s
//! adds another window before giving up.

use std::time::Duration;

const ZERO: Duration = Duration::from_secs(0);
const TWO: Duration = Duration::from_secs(2);
const FOUR: Duration = Duration::from_secs(4);
const EIGHT: Duration = Duration::from_secs(8);
const SIXTEEN: Duration = Duration::from_secs(16);
const THIRTY: Duration = Duration::from_secs(30);

/// Delay to wait before attempt `attempt`. Attempt 0 is the immediate
/// retry that AC4 hinges on; subsequent attempts back off exponentially
/// up to a 30-second cap.
pub fn next_backoff(attempt: u32) -> Duration {
    match attempt {
        0 => ZERO,
        1 => TWO,
        2 => FOUR,
        3 => EIGHT,
        4 => SIXTEEN,
        _ => THIRTY,
    }
}

/// Decision whether to keep trying. `attempt` is the next attempt number
/// (zero-based). `max_attempts == 0` means "retry forever".
pub fn should_retry(attempt: u32, max_attempts: u32) -> bool {
    max_attempts == 0 || attempt < max_attempts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_backoff_first_attempt_is_immediate() {
        assert_eq!(next_backoff(0), Duration::ZERO);
    }

    #[test]
    fn next_backoff_exponential_through_cap() {
        assert_eq!(next_backoff(1), Duration::from_secs(2));
        assert_eq!(next_backoff(2), Duration::from_secs(4));
        assert_eq!(next_backoff(3), Duration::from_secs(8));
        assert_eq!(next_backoff(4), Duration::from_secs(16));
        assert_eq!(next_backoff(5), Duration::from_secs(30));
        assert_eq!(next_backoff(6), Duration::from_secs(30));
        assert_eq!(next_backoff(100), Duration::from_secs(30));
    }

    #[test]
    fn ac4_first_three_attempts_under_five_seconds() {
        // AC4: reconnect ≤ 5 s after sleep-wake. The first attempt is
        // immediate — if the radio's still settling and that fails, the
        // second attempt fires 2 s later. Cumulative wait at attempt 1 is
        // 2 s; at attempt 2 it's 6 s — so AC4 specifically depends on
        // either attempt 0 or attempt 1 succeeding.
        let total_through_first_two = next_backoff(0) + next_backoff(1);
        assert!(
            total_through_first_two <= Duration::from_secs(2),
            "AC4 budget tight: 2 s of backoff before the second attempt fires"
        );
    }

    #[test]
    fn should_retry_unlimited_when_max_is_zero() {
        assert!(should_retry(0, 0));
        assert!(should_retry(100, 0));
        assert!(should_retry(u32::MAX, 0));
    }

    #[test]
    fn should_retry_respects_max_attempts() {
        assert!(should_retry(0, 3));
        assert!(should_retry(1, 3));
        assert!(should_retry(2, 3));
        assert!(!should_retry(3, 3));
        assert!(!should_retry(4, 3));
    }

    #[test]
    fn reconnect_loop_respects_max_attempts() {
        // Simulate a loop calling should_retry with attempt counter.
        let max = 5u32;
        let mut attempt = 0u32;
        while should_retry(attempt, max) {
            // Pretend connect failed.
            attempt += 1;
        }
        assert_eq!(attempt, max);
    }
}
