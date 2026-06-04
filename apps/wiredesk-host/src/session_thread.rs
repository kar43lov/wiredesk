use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wiredesk_core::error::WireDeskError;

use crate::clipboard::ProgressCounters;
use crate::config::{self, HostConfig};
use crate::session::{Session, SessionState};

/// Status reported up to the UI thread (tray icon color, settings window
/// status row). `Connected` carries the latest `client_name` reported via
/// the Hello handshake.
///
/// `Notification` is a transient event — not a state — that the tray UI
/// surfaces as a balloon and then drops. It does NOT change tray-icon color
/// or settings-row status. Used today only for "image too large to send".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Disconnected(String),
    Waiting,
    Connected { client_name: String },
    Notification(String),
}

impl SessionStatus {
    #[allow(dead_code)] // wired up by tray icon color logic in Task 7
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// Human-readable label for the tray tooltip / settings status row.
    /// `Notification` is transient — it's surfaced as a balloon, not as
    /// the persistent status row, so this label is never user-visible.
    pub fn label(&self) -> String {
        match self {
            Self::Disconnected(reason) => format!("Disconnected: {reason}"),
            Self::Waiting => "Waiting for client…".to_string(),
            Self::Connected { client_name } => format!("Connected to {client_name}"),
            Self::Notification(msg) => msg.clone(),
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
///
/// `counters` is shared with the always-on-top transfer overlay (Task 1).
/// The session thread is the sole writer; the overlay thread only reads.
#[cfg(target_os = "windows")]
pub fn spawn(
    config: HostConfig,
    status_tx: mpsc::Sender<SessionStatus>,
    counters: ProgressCounters,
) -> JoinHandle<()> {
    use crate::injector::WindowsInjector;
    spawn_with_injector(config, status_tx, counters, |_| WindowsInjector::new())
}

/// Non-Windows fallback for development on macOS / Linux: uses the mock
/// injector. The actual SendInput happens only on Windows.
#[cfg(not(target_os = "windows"))]
pub fn spawn(
    config: HostConfig,
    status_tx: mpsc::Sender<SessionStatus>,
    counters: ProgressCounters,
) -> JoinHandle<()> {
    use crate::injector::MockInjector;
    spawn_with_injector(config, status_tx, counters, |_| {
        Ok::<_, WireDeskError>(MockInjector::default())
    })
}

fn spawn_with_injector<I, F>(
    config: HostConfig,
    status_tx: mpsc::Sender<SessionStatus>,
    counters: ProgressCounters,
    make_injector: F,
) -> JoinHandle<()>
where
    I: crate::injector::InputInjector + Send + 'static,
    F: FnOnce(&HostConfig) -> wiredesk_core::error::Result<I> + Send + 'static,
{
    thread::spawn(move || {
        let transport_cfg = config::to_transport_config(&config);

        // Injector is built ONCE — it doesn't depend on the serial port and
        // `make_injector` is `FnOnce`. Across reopens the same injector
        // migrates from the old (dismantled) Session into the new one via
        // `Session::into_injector`.
        let mut injector = match make_injector(&config) {
            Ok(i) => i,
            Err(e) => {
                log::error!("failed to init injector: {e}");
                let _ = status_tx.send(SessionStatus::Disconnected(format!("injector: {e}")));
                return;
            }
        };

        // `receive_files` toggle: Settings UI persists `HostConfig.receive_files`
        // and Save-and-Restart respawns the whole host process, so this Arc
        // is effectively read-once at boot. Wrapping in an Arc keeps the
        // signature aligned with `with_counters_and_toggles` (which also
        // serves Mac-style live mutation; we just never call `store` on the
        // host side).
        let receive_files = Arc::new(AtomicBool::new(config.receive_files));

        // Backoff attempt counter for consecutive open failures; reset to 0
        // on every successful open.
        let mut reopen_attempt: u32 = 0;

        // Outer reopen loop. Each iteration opens the transport (with backoff
        // on failure), runs the tick-loop until a frame-error storm fires,
        // then dismantles the Session (releasing the COM-port handle) and
        // loops to reopen — re-initialising the FT232H chip, which is the
        // known cure for a storm.
        'reopen: loop {
            // Open transport with exponential backoff. This subsumes the old
            // "open failed on process start → return" path: instead of giving
            // up, we keep retrying so a late-arriving / momentarily-busy port
            // still recovers without a manual restart.
            let transport = 'open: loop {
                match wiredesk_transport::open_transport(&transport_cfg) {
                    Ok(t) => {
                        log::info!("opened transport: {} ({})", t.name(), config.transport);
                        break 'open t;
                    }
                    Err(e) => {
                        log::error!(
                            "failed to open transport (mode={}): {e}",
                            config.transport
                        );
                        let _ = status_tx.send(SessionStatus::Disconnected(format!(
                            "{}: {e}",
                            config.transport
                        )));
                        let delay = next_backoff(reopen_attempt);
                        reopen_attempt += 1;
                        log::info!(
                            "reopening transport attempt={reopen_attempt} (retry in {}s)",
                            delay.as_secs()
                        );
                        thread::sleep(delay);
                    }
                }
            };
            // Successful open → reset the backoff for the next storm episode.
            reopen_attempt = 0;

            let mut sess = Session::with_counters_and_toggles(
                transport,
                injector,
                config.host_name.clone(),
                config.width,
                config.height,
                counters.clone(),
                receive_files.clone(),
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
                        // Feed the storm detector. A single bad frame is
                        // normal (logged + dropped); a sustained run means
                        // the chip glitched and only a port reopen recovers.
                        if sess.note_protocol_error() {
                            log::warn!(
                                "frame-error storm detected ({} consecutive) — reopening transport: {msg}",
                                wiredesk_core::storm::DEFAULT_STORM_THRESHOLD
                            );
                            injector = sess.into_injector();
                            // Win serialport close is async — give the OS a
                            // moment to release the handle before reopening,
                            // else open hits "Access is denied".
                            thread::sleep(Duration::from_millis(500));
                            continue 'reopen;
                        }
                        log::warn!("dropping bad frame: {msg}");
                    }
                    Err(e) => {
                        log::error!("session error: {e}");
                        thread::sleep(Duration::from_secs(1));
                    }
                }

                // Drain any transient clipboard warning (e.g., "image too
                // large") into a one-shot Notification status. The UI shows a
                // balloon and then the status flow returns to the persistent
                // (Connected/Waiting/Disconnected) state below.
                if let Some(warning) = sess.take_clipboard_warning() {
                    let _ = status_tx.send(SessionStatus::Notification(warning));
                }

                let next = derive_status(sess.current_state(), sess.client_name());
                if last_reported.as_ref() != Some(&next) {
                    let _ = status_tx.send(next.clone());
                    last_reported = Some(next);
                }
            }
        }
    })
}

/// Exponential backoff schedule for transport reopen: 1s, 2s, 4s, 8s, 16s,
/// then capped at 30s. `attempt` is 0-based.
fn next_backoff(attempt: u32) -> Duration {
    const CAP_SECS: u64 = 30;
    let secs = 1u64.checked_shl(attempt).unwrap_or(CAP_SECS).min(CAP_SECS);
    Duration::from_secs(secs)
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
    fn backoff_schedule_caps_at_30s() {
        assert_eq!(next_backoff(0), Duration::from_secs(1));
        assert_eq!(next_backoff(1), Duration::from_secs(2));
        assert_eq!(next_backoff(2), Duration::from_secs(4));
        assert_eq!(next_backoff(3), Duration::from_secs(8));
        assert_eq!(next_backoff(4), Duration::from_secs(16));
        // 1<<5 = 32 → capped at 30
        assert_eq!(next_backoff(5), Duration::from_secs(30));
        assert_eq!(next_backoff(6), Duration::from_secs(30));
        // Far past the shift width — checked_shl returns None → cap.
        assert_eq!(next_backoff(64), Duration::from_secs(30));
        assert_eq!(next_backoff(1000), Duration::from_secs(30));
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
