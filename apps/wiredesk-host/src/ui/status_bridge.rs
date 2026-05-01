//! Forward `SessionStatus` updates from the session thread to the nwg UI
//! thread.
//!
//! Pattern: session thread → `mpsc::Sender<SessionStatus>` → bridge thread.
//! The bridge stores the latest status in a shared `Arc<Mutex<...>>` and
//! pings the UI via `nwg::NoticeSender::notice()`. The UI handler then
//! reads the mutex on the main thread (where it can safely touch nwg
//! controls) and updates icon + labels.
//!
//! On non-Windows targets there's no nwg, so we expose a stripped-down
//! version that just stores the latest status — useful for the dev-mode
//! foreground loop on macOS.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::session_thread::SessionStatus;

#[cfg(windows)]
pub fn spawn(
    rx: mpsc::Receiver<SessionStatus>,
    last: Arc<Mutex<SessionStatus>>,
    notice: native_windows_gui::NoticeSender,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(status) = rx.recv() {
            if let Ok(mut g) = last.lock() {
                *g = status;
            }
            notice.notice();
        }
    })
}

/// Cross-platform version: just stash latest status, no nwg notice. Used
/// in the dev-mode foreground loop on macOS / Linux.
#[cfg_attr(windows, allow(dead_code))]
pub fn spawn_no_notice(
    rx: mpsc::Receiver<SessionStatus>,
    last: Arc<Mutex<SessionStatus>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(status) = rx.recv() {
            if let Ok(mut g) = last.lock() {
                *g = status;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn no_notice_bridge_stores_latest_status() {
        let (tx, rx) = mpsc::channel();
        let last = Arc::new(Mutex::new(SessionStatus::Waiting));
        let _h = spawn_no_notice(rx, last.clone());

        tx.send(SessionStatus::Connected {
            client_name: "x".to_string(),
        })
        .unwrap();
        tx.send(SessionStatus::Disconnected("link down".to_string()))
            .unwrap();

        // Give the bridge thread a moment to drain.
        for _ in 0..50 {
            if matches!(*last.lock().unwrap(), SessionStatus::Disconnected(_)) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            *last.lock().unwrap(),
            SessionStatus::Disconnected(_)
        ));
    }

    #[test]
    fn no_notice_bridge_exits_on_sender_drop() {
        let (tx, rx) = mpsc::channel();
        let last = Arc::new(Mutex::new(SessionStatus::Waiting));
        let h = spawn_no_notice(rx, last);
        drop(tx);
        h.join().expect("thread should exit cleanly");
    }
}
