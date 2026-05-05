//! File + stderr logging для `wiredesk-client` и `WireDesk.app`.
//!
//! Mirrors the pattern used by `wiredesk-host` (`apps/wiredesk-host/src/logging.rs`):
//! a non-blocking daily-rolling file appender, `log → tracing` bridge so legacy
//! `log::*` macros land in the file, and a panic hook routed through
//! `tracing::error!`.
//!
//! Two differences from host:
//!
//! 1. **Dual sink** — file *and* stderr. Host runs as a background tray agent
//!    where stderr would never be seen; client is launched two ways
//!    (`wiredesk-client` from terminal, `WireDesk.app` from Finder/dock). For
//!    terminal launches stderr is the natural place; for `.app` launches
//!    stderr lands in `Console.app` under the bundle's process. The file is
//!    always-on and usable for post-mortem.
//!
//! 2. **`RUST_LOG` env-filter wired up** — host doesn't actually parse
//!    `RUST_LOG` despite carrying the `env-filter` feature. We do, because
//!    `wd --exec`-style live debugging often wants `RUST_LOG=debug` to see
//!    transport-level traces. Default is `info`.

use std::fs;
use std::io;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Default location for client logs: `~/Library/Application Support/WireDesk` on
/// macOS, `%APPDATA%\WireDesk` on Windows (cross-compile dev only),
/// `$XDG_CONFIG_HOME/WireDesk` elsewhere. Falls back to the working directory
/// if no config dir is exposed. Same dir as the host crate uses on its side —
/// that's by design: a single `WireDesk/` dir holds all artifacts (config,
/// host.log, client.log, wd-exec.sock).
pub fn log_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WireDesk")
}

/// Set up rolling daily file logging at the default location plus a stderr
/// layer, install a `log → tracing` bridge so legacy `log::*` macros (used
/// throughout `app`, `clipboard`, `ipc`, `keyboard_tap`, etc.) hit both sinks,
/// and route panics through `tracing::error!`.
///
/// Returns a `WorkerGuard` whose drop flushes the non-blocking writer queue —
/// `main()` must keep it alive until shutdown or trailing log lines may be
/// lost.
pub fn init_logging() -> io::Result<WorkerGuard> {
    init_logging_at(&log_dir())
}

pub fn init_logging_at(dir: &Path) -> io::Result<WorkerGuard> {
    fs::create_dir_all(dir)?;

    // Use the Builder API rather than `tracing_appender::rolling::daily()` —
    // the convenience function calls `.expect()` internally and panics on
    // file-open failures (locked file, permissions error, conflicting
    // existing path), bypassing the `Result`-returning contract of this
    // function and crashing the client on startup. Builder returns a real
    // `InitError` we can convert to `io::Error` so `main()`'s fallback to
    // stderr-only logging actually runs.
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("client.log")
        .build(dir)
        .map_err(|e| io::Error::other(format!("rolling appender: {e}")))?;
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_ansi(true)
        .with_target(false);

    // `RUST_LOG=debug,wiredesk_exec_core=trace` etc. parses here; default
    // matches what the prior `env_logger::Env::default().default_filter_or("info")`
    // gave us, so no behavioural regression at the INFO level.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // try_init: tolerate the case where another component (a test harness, for
    // instance) already set a global subscriber.
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .try_init();

    // Bridge `log` → `tracing`. Without this, the `log::info!`/`warn!`/`error!`
    // macros that the rest of the crate uses would simply vanish (env_logger
    // is gone).
    let _ = tracing_log::LogTracer::init();

    Ok(guard)
}

/// Stderr-only subscriber for the fallback path when the file appender
/// can't be opened (read-only home, permissions error, etc). Mirrors the
/// pre-tracing `env_logger` behaviour so log output isn't silently dropped
/// just because we couldn't open a log file.
///
/// `RUST_LOG` env-filter is honoured here too; default is `info`.
pub fn init_logging_stderr_only() {
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_ansi(true)
        .with_target(false);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .try_init();

    let _ = tracing_log::LogTracer::init();
}

/// Install the panic-to-tracing hook. Kept separate from `init_logging` so
/// tests can exercise the writer wiring without leaking a global hook across
/// the test binary.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(target: "panic", "{}", format_panic(info));
    }));
}

/// Render a `PanicHookInfo` to a single-line, log-friendly string with
/// location and message — independent of where it ends up.
pub fn format_panic(info: &PanicHookInfo<'_>) -> String {
    let loc = info
        .location()
        .map(|l| l.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    let payload = info.payload();
    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
        *s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic>"
    };
    format!("PANIC at {loc}: {msg}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;
    use tempfile::tempdir;

    /// `init_logging_at` installs a global panic hook as a side effect; tests
    /// that touch it must restore the previous hook so they don't leak into
    /// other tests in the same process.
    type StoredHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send>;
    struct PanicHookGuard(Option<StoredHook>);

    impl PanicHookGuard {
        fn capture() -> Self {
            Self(Some(std::panic::take_hook()))
        }
    }

    impl Drop for PanicHookGuard {
        fn drop(&mut self) {
            if let Some(hook) = self.0.take() {
                std::panic::set_hook(hook);
            }
        }
    }

    #[test]
    fn format_panic_includes_location_and_message() {
        let _guard = PanicHookGuard::capture();
        let (tx, rx) = mpsc::channel::<String>();
        std::panic::set_hook(Box::new(move |info| {
            let _ = tx.send(format_panic(info));
        }));
        let _ = std::panic::catch_unwind(|| {
            panic!("kaboom-client");
        });
        let msg = rx.try_recv().expect("hook should have fired");
        assert!(msg.starts_with("PANIC at "), "msg={msg:?}");
        assert!(msg.contains("kaboom-client"), "msg={msg:?}");
        assert!(
            msg.contains("logging.rs") || msg.contains(".rs:"),
            "expected file location, got {msg:?}"
        );
    }

    #[test]
    fn rolling_appender_writes_to_log_file() {
        let dir = tempdir().unwrap();
        let appender = tracing_appender::rolling::daily(dir.path(), "test-client.log");
        let (mut non_blocking, guard) = tracing_appender::non_blocking(appender);
        writeln!(non_blocking, "hello WireDesk client").unwrap();
        // Drop the writer first, then the guard, to flush the queue.
        drop(non_blocking);
        drop(guard);

        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.is_empty(), "expected at least one log file");
        assert!(
            names.iter().any(|n| n.starts_with("test-client.log")),
            "expected file matching 'test-client.log*', got {names:?}"
        );
    }

    #[test]
    fn init_logging_at_creates_missing_dir() {
        let parent = tempdir().unwrap();
        let log_dir = parent.path().join("nested").join("WireDesk");
        assert!(!log_dir.exists());
        let _worker = init_logging_at(&log_dir).unwrap();
        assert!(log_dir.is_dir(), "expected init_logging_at to create dir");
    }

    #[test]
    fn log_dir_is_under_config_dir() {
        let p = log_dir();
        assert_eq!(p.file_name().unwrap(), "WireDesk");
    }
}
