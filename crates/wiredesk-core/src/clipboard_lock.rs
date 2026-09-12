//! Process-wide lock around every OS-clipboard call.
//!
//! Both apps read and write the system clipboard from more than one thread:
//! the client polls it on `spawn_poll_thread` while the reader thread commits
//! whatever the peer sent, and the client's file paths go through
//! `NSPasteboard` directly rather than through `arboard`. On macOS those calls
//! are not safe to make at the same time — two threads reading the general
//! pasteboard concurrently crash inside `objc_msgSend`, and the process dies
//! with `fatal runtime error: Rust cannot catch foreign exceptions` before any
//! Rust code can react.
//!
//! Measured 2026-09-11 on macOS 15 (arboard 3.6.1, the newest release): one
//! thread holding a clipboard and reading it in a loop while another creates a
//! clipboard, reads it and drops it aborted 2 runs out of 3. Serialising only
//! the calls — creation and drop left alone — was clean 4 runs out of 4, which
//! also says this is about concurrent access and not about object lifetime.
//! The same shape runs in production on every reconnect: `reader_loop` builds
//! an `IncomingClipboard` per link while the poll thread keeps reading.
//!
//! The guard is taken around single calls — never around a block that could
//! call back in, which would deadlock on a non-reentrant `Mutex`. A waiting
//! thread waits for one clipboard call: microseconds for text, milliseconds
//! for a multi-megabyte image. That is the whole cost, and the alternative to
//! paying it is the process aborting.

use std::sync::{Mutex, MutexGuard};

static CLIPBOARD: Mutex<()> = Mutex::new(());

/// Take the clipboard lock for the duration of one OS call.
///
/// Poisoning is ignored on purpose: the guarded data is `()`, so a panic in
/// another thread leaves nothing inconsistent behind, and refusing to hand out
/// the lock afterwards would turn one panicking clipboard poll into a
/// permanently dead clipboard.
pub fn hold() -> MutexGuard<'static, ()> {
    CLIPBOARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_is_handed_out_again_after_a_panic() {
        let _ = std::panic::catch_unwind(|| {
            let _g = hold();
            panic!("poison the mutex");
        });
        // Would block forever on a plain `unwrap()` of a poisoned mutex.
        let _g = hold();
    }

    #[test]
    fn the_lock_serialises_two_threads() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;
        let inside = Arc::new(AtomicU32::new(0));
        let max_seen = Arc::new(AtomicU32::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let inside = Arc::clone(&inside);
            let max_seen = Arc::clone(&max_seen);
            handles.push(std::thread::spawn(move || {
                for _ in 0..200 {
                    let _g = hold();
                    let n = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(n, Ordering::SeqCst);
                    inside.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "two threads were inside the lock at once"
        );
    }
}
