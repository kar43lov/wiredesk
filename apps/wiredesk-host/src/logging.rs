use std::fs;
use std::io;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

use tracing_appender::non_blocking::WorkerGuard;

/// Default location for host logs: `%APPDATA%\WireDesk` on Windows,
/// `~/Library/Application Support/WireDesk` on macOS, `$XDG_CONFIG_HOME/WireDesk`
/// elsewhere. Falls back to the working directory if no config dir is exposed.
pub fn log_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WireDesk")
}

/// Set up rolling daily file logging at the default location, install a
/// `log → tracing` bridge so legacy `log::*` macros (used by `session`,
/// `clipboard`, `injector`, `shell`) hit the same sink, and route panics
/// through `tracing::error!`.
///
/// Returns a `WorkerGuard` whose drop flushes the non-blocking writer —
/// `main()` must keep it alive until shutdown.
pub fn init_logging() -> io::Result<WorkerGuard> {
    init_logging_at(&log_dir())
}

pub fn init_logging_at(dir: &Path) -> io::Result<WorkerGuard> {
    fs::create_dir_all(dir)?;

    let appender = tracing_appender::rolling::daily(dir, "host.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    // try_init: tolerate the case where another component already set a
    // global subscriber (tests, or a fall-through path).
    let _ = tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(false)
        .try_init();

    // Bridge `log` → `tracing` so existing log::info!/warn!/error! across
    // the host crate ends up in our file too.
    let _ = tracing_log::LogTracer::init();

    Ok(guard)
}

/// Install the panic-to-tracing hook. Kept separate from `init_logging` so
/// tests can exercise the writer / subscriber wiring without leaking a
/// global hook across the test binary (which would corrupt other tests).
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
            panic!("kaboom");
        });
        let msg = rx.try_recv().expect("hook should have fired");
        assert!(msg.starts_with("PANIC at "), "msg={msg:?}");
        assert!(msg.contains("kaboom"), "msg={msg:?}");
        // Should reference the source file by name to be useful in logs.
        assert!(
            msg.contains("logging.rs") || msg.contains(".rs:"),
            "expected file location, got {msg:?}"
        );
    }

    #[test]
    fn rolling_appender_writes_to_log_file() {
        let dir = tempdir().unwrap();
        let appender = tracing_appender::rolling::daily(dir.path(), "test.log");
        let (mut non_blocking, guard) = tracing_appender::non_blocking(appender);
        writeln!(non_blocking, "hello WireDesk").unwrap();
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
            names.iter().any(|n| n.starts_with("test.log")),
            "expected file matching 'test.log*', got {names:?}"
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
