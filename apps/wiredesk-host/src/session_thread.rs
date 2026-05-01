use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wiredesk_core::error::WireDeskError;

use crate::config::HostConfig;
use crate::session::{Session, SessionState};

/// Status reported up to the UI thread (tray icon color, settings window
/// status row). `Connected` carries the latest `client_name` reported via
/// the Hello handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Disconnected(String),
    Waiting,
    Connected { client_name: String },
}

impl SessionStatus {
    #[allow(dead_code)] // wired up by tray icon color logic in Task 7
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// Human-readable label for the tray tooltip / settings status row.
    pub fn label(&self) -> String {
        match self {
            Self::Disconnected(reason) => format!("Disconnected: {reason}"),
            Self::Waiting => "Waiting for client…".to_string(),
            Self::Connected { client_name } => format!("Connected to {client_name}"),
        }
    }
}

/// Spawn the session loop on its own thread. The thread owns the serial
/// transport + injector, runs `Session::tick` in a loop, and emits
/// `SessionStatus` updates whenever the underlying state changes (so the
/// UI doesn't need to poll).
///
/// Returns a `JoinHandle` so the caller can keep the thread alive — the
/// loop itself runs forever; on fatal error we log and sleep before
/// retrying, mirroring the original `main.rs` semantics.
#[cfg(target_os = "windows")]
pub fn spawn(config: HostConfig, status_tx: mpsc::Sender<SessionStatus>) -> JoinHandle<()> {
    use crate::injector::WindowsInjector;
    spawn_with_injector(config, status_tx, |_| WindowsInjector::new())
}

/// Non-Windows fallback for development on macOS / Linux: uses the mock
/// injector. The actual SendInput happens only on Windows.
#[cfg(not(target_os = "windows"))]
pub fn spawn(config: HostConfig, status_tx: mpsc::Sender<SessionStatus>) -> JoinHandle<()> {
    use crate::injector::MockInjector;
    spawn_with_injector(config, status_tx, |_| Ok::<_, WireDeskError>(MockInjector::default()))
}

fn spawn_with_injector<I, F>(
    config: HostConfig,
    status_tx: mpsc::Sender<SessionStatus>,
    make_injector: F,
) -> JoinHandle<()>
where
    I: crate::injector::InputInjector + Send + 'static,
    F: FnOnce(&HostConfig) -> wiredesk_core::error::Result<I> + Send + 'static,
{
    thread::spawn(move || {
        let transport =
            match wiredesk_transport::serial::SerialTransport::open(&config.port, config.baud) {
                Ok(t) => t,
                Err(e) => {
                    log::error!("failed to open serial port {}: {e}", config.port);
                    let _ = status_tx.send(SessionStatus::Disconnected(format!("serial: {e}")));
                    return;
                }
            };

        let injector = match make_injector(&config) {
            Ok(i) => i,
            Err(e) => {
                log::error!("failed to init injector: {e}");
                let _ = status_tx.send(SessionStatus::Disconnected(format!("injector: {e}")));
                return;
            }
        };

        let mut sess = Session::new(
            transport,
            injector,
            config.host_name.clone(),
            config.width,
            config.height,
        );

        let _ = status_tx.send(SessionStatus::Waiting);
        let mut last_reported: Option<SessionStatus> = None;

        loop {
            match sess.tick() {
                Ok(_) => {}
                Err(WireDeskError::Transport(ref msg)) if msg.contains("timeout") => {
                    // Recv timeout — normal, just loop.
                }
                Err(WireDeskError::Protocol(ref msg)) => {
                    log::warn!("dropping bad frame: {msg}");
                }
                Err(e) => {
                    log::error!("session error: {e}");
                    thread::sleep(Duration::from_secs(1));
                }
            }

            let next = derive_status(sess.current_state(), sess.client_name());
            if last_reported.as_ref() != Some(&next) {
                let _ = status_tx.send(next.clone());
                last_reported = Some(next);
            }
        }
    })
}

pub fn derive_status(state: SessionState, client_name: Option<&str>) -> SessionStatus {
    match state {
        SessionState::WaitingForHello => SessionStatus::Waiting,
        SessionState::Connected => SessionStatus::Connected {
            client_name: client_name.unwrap_or("(unknown)").to_string(),
        },
        SessionState::Disconnected => SessionStatus::Disconnected("link down".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_label_disconnected() {
        let s = SessionStatus::Disconnected("link down".to_string());
        assert_eq!(s.label(), "Disconnected: link down");
        assert!(!s.is_connected());
    }

    #[test]
    fn status_label_waiting() {
        let s = SessionStatus::Waiting;
        assert_eq!(s.label(), "Waiting for client…");
        assert!(!s.is_connected());
    }

    #[test]
    fn status_label_connected() {
        let s = SessionStatus::Connected {
            client_name: "macbook".to_string(),
        };
        assert_eq!(s.label(), "Connected to macbook");
        assert!(s.is_connected());
    }

    #[test]
    fn derive_status_maps_session_state() {
        assert_eq!(
            derive_status(SessionState::WaitingForHello, None),
            SessionStatus::Waiting
        );
        assert_eq!(
            derive_status(SessionState::Connected, Some("client-A")),
            SessionStatus::Connected {
                client_name: "client-A".to_string()
            }
        );
        assert_eq!(
            derive_status(SessionState::Connected, None),
            SessionStatus::Connected {
                client_name: "(unknown)".to_string()
            }
        );
    }
}
