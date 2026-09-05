//! Windows CF_HDROP clipboard access for the host.
//!
//! The implementation now lives in `wiredesk_core::file_clipboard` — the
//! Windows client needs the identical Win32 dance (read Explorer's copy,
//! write an incoming file back as a paste source), and a second copy would
//! be a second place for the HGLOBAL ownership rules to drift. This module
//! stays as the host's spelling of that API so the call sites in
//! `clipboard.rs` read the same as before.
//!
//! Scope, threading and memory-ownership notes: see the core module.

// `set_cf_hdrop` and the error type only have callers inside
// `cfg(windows)` blocks; re-exporting them unconditionally keeps this
// module's surface identical on every target, which is what the tests
// below (and any future non-Windows stub caller) expect.
#[allow(unused_imports)]
pub use wiredesk_core::file_clipboard::{
    current_clipboard_seq, poll_cf_hdrop, set_cf_hdrop, FileClipboardError,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Re-export smoke test: the host's spelling of the API resolves to the
    /// core implementation and keeps the contract its callers rely on —
    /// `Err` for an unusable path, never a panic or a silent success.
    #[test]
    fn set_cf_hdrop_rejects_empty_path() {
        assert!(set_cf_hdrop(Path::new("")).is_err());
    }

    /// On non-Windows the poll stub returns None, which `clipboard.rs`
    /// treats as "no files on the clipboard".
    #[test]
    #[cfg(not(windows))]
    fn poll_cf_hdrop_stub_returns_none() {
        assert!(poll_cf_hdrop().is_none());
    }

    /// The sequence number gates the file-send probe; on non-Windows it is a
    /// constant, which makes the gate a permanent "unchanged".
    #[test]
    #[cfg(not(windows))]
    fn clipboard_seq_stub_is_constant() {
        assert_eq!(current_clipboard_seq(), current_clipboard_seq());
    }
}
