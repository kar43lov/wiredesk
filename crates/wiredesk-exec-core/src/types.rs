//! Shared types for `wd --exec`-style sentinel-driven command execution.

use thiserror::Error;

/// Which host shell flavour we're targeting when formatting the
/// sentinel-bearing command. The host always runs PowerShell; when
/// `--ssh` chains us to a remote box we typically end up in bash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell,
    Bash,
}

/// Events surfaced by an `ExecTransport`. The runner walks these line
/// by line and matches against the sentinel/READY markers.
///
/// `Idle` is the "no data this tick" signal — separate from `Closed`
/// because the runner re-checks its overall `--timeout` against
/// `Instant::now()` on every Idle and only fails the whole call when
/// that wall-clock budget is exhausted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecEvent {
    /// Bytes read from the host shell's stdout (raw, not yet ANSI-stripped).
    ShellOutput(Vec<u8>),
    /// Host shell terminated with this exit code (rare in `--exec` flow,
    /// the sentinel usually fires first; see PR #11 for context).
    ShellExit(i32),
    /// Host emitted a `Message::Error` packet — typically benign log
    /// noise the runner records but doesn't act on.
    HostError(String),
    /// No event in this `recv_event` window. Caller re-checks timeout.
    Idle,
}

/// Failure modes for `ExecTransport`. `Transport(...)` covers serial
/// IO errors and mpsc Sender disconnects mid-send. `Closed` is the
/// permanent-shutdown signal — for `IpcExecTransport` it means the
/// reader thread's mpsc receiver was dropped (GUI tearing down); for
/// `SerialExecTransport` it's EOF / disconnected line.
#[derive(Debug, Error)]
pub enum ExecError {
    #[error("transport: {0}")]
    Transport(String),
    #[error("transport closed")]
    Closed,
}

/// State machine for the runner. PS-only mode skips straight to
/// `AwaitingSentinel` (the formatted command is sent immediately —
/// PS pipe-mode reads stdin line-by-line, no need to sync). SSH
/// mode goes `AwaitingRemotePrompt → AwaitingSentinel`: we MUST wait
/// for the remote shell to emit its prompt before pushing the payload,
/// otherwise PS's .NET StreamReader read-ahead swallows whatever line
/// we sent after `ssh -tt ALIAS\n` (PS has consumed line 1 + buffered
/// line 2 BEFORE spawning ssh; line 2 is stuck in PS-memory, never
/// reaches the ssh subprocess).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum OneShotState {
    AwaitingRemotePrompt,
    AwaitingSentinel,
}
