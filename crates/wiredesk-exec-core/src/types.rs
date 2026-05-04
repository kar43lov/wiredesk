//! Shared types for `wd --exec`-style sentinel-driven command execution.

/// Which host shell flavour we're targeting when formatting the
/// sentinel-bearing command. The host always runs PowerShell; when
/// `--ssh` chains us to a remote box we typically end up in bash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    PowerShell,
    Bash,
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
