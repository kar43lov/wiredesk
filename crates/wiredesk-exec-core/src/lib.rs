//! Shared core for `wd --exec`-style sentinel-driven execution. Used
//! by both the standalone `wiredesk-term` (direct serial path) and
//! the GUI client's IPC handler so the production sentinel logic
//! exists in exactly one place.

pub mod helpers;
pub mod ipc;
pub mod runner;
pub mod transport;
pub mod types;

pub use helpers::{
    clean_stdout, format_command, format_timeout_diagnostic, is_powershell_prompt,
    is_remote_prompt, parse_ready, parse_sentinel, strip_ansi,
};
pub use ipc::{default_socket_path, read_request, read_response, write_request, write_response, IpcRequest, IpcResponse};
pub use runner::run_oneshot;
pub use transport::ExecTransport;
pub use types::{ExecError, ExecEvent, OneShotState, ShellKind};
