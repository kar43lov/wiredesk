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

/// Bundle of state shared between the session thread (writer) and the UI
/// thread (reader). Splits "persistent state" (Connected/Waiting/Disconnected)
/// from a transient "pending notification" (balloon-only "image too large").
/// Without the split a Notification would overwrite the persistent state in
/// the Mutex, leaving settings UI labelled "image too large" indefinitely.
#[derive(Debug, Default, Clone)]
pub struct StatusState {
    pub persistent: PersistentStatus,
    pub pending_notification: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum PersistentStatus {
    #[default]
    Waiting,
    Connected(String),
    Disconnected(String),
}

impl PersistentStatus {
    pub fn to_session_status(&self) -> SessionStatus {
        match self {
            Self::Waiting => SessionStatus::Waiting,
            Self::Connected(client_name) => SessionStatus::Connected {
                client_name: client_name.clone(),
            },
            Self::Disconnected(reason) => SessionStatus::Disconnected(reason.clone()),
        }
    }
}

#[cfg(windows)]
pub fn spawn(
    rx: mpsc::Receiver<SessionStatus>,
    state: Arc<Mutex<StatusState>>,
    notice: native_windows_gui::NoticeSender,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(status) = rx.recv() {
            if let Ok(mut g) = state.lock() {
                apply(&mut g, status);
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
    state: Arc<Mutex<StatusState>>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(status) = rx.recv() {
            if let Ok(mut g) = state.lock() {
                apply(&mut g, status);
            }
        }
    })
}

/// Route an incoming `SessionStatus` into the correct slot of `StatusState`.
/// Persistent statuses overwrite `persistent`; `Notification` queues into
/// `pending_notification` without disturbing `persistent`.
fn apply(state: &mut StatusState, status: SessionStatus) {
    match status {
        SessionStatus::Waiting => state.persistent = PersistentStatus::Waiting,
        SessionStatus::Connected { client_name } => {
            state.persistent = PersistentStatus::Connected(client_name);
        }
        SessionStatus::Disconnected(reason) => {
            state.persistent = PersistentStatus::Disconnected(reason);
        }
        SessionStatus::Notification(msg) => {
            state.pending_notification = Some(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn no_notice_bridge_stores_latest_status() {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(StatusState::default()));
        let _h = spawn_no_notice(rx, state.clone());

        tx.send(SessionStatus::Connected {
            client_name: "x".to_string(),
        })
        .unwrap();
        tx.send(SessionStatus::Disconnected("link down".to_string()))
            .unwrap();

        for _ in 0..50 {
            if matches!(
                state.lock().unwrap().persistent,
                PersistentStatus::Disconnected(_)
            ) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(
            state.lock().unwrap().persistent,
            PersistentStatus::Disconnected(_)
        ));
    }

    #[test]
    fn notification_does_not_overwrite_persistent() {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(StatusState::default()));
        let _h = spawn_no_notice(rx, state.clone());

        tx.send(SessionStatus::Connected {
            client_name: "client-A".to_string(),
        })
        .unwrap();
        tx.send(SessionStatus::Notification("image too large".to_string()))
            .unwrap();

        // Drain
        for _ in 0..50 {
            if state.lock().unwrap().pending_notification.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let g = state.lock().unwrap();
        assert!(matches!(g.persistent, PersistentStatus::Connected(_)));
        assert_eq!(g.pending_notification.as_deref(), Some("image too large"));
    }

    #[test]
    fn no_notice_bridge_exits_on_sender_drop() {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(StatusState::default()));
        let h = spawn_no_notice(rx, state);
        drop(tx);
        h.join().expect("thread should exit cleanly");
    }
}
