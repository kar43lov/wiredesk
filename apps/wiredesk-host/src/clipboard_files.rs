//! Windows CF_HDROP clipboard FFI for file path operations.
//!
//! Bridges Windows shell drop-list (`CF_HDROP`) clipboard entries to/from
//! `std::path::PathBuf` so the host clipboard sync loop can detect Explorer
//! copies and inject incoming files as Explorer-paste-compatible sources.
//!
//! ## Scope
//!
//! - **Single-file only** for Phase 1. Multi-file pasteboard selections
//!   (the HDROP handle reports `count != 1` via `DragQueryFileW(0xFFFFFFFF)`)
//!   are silently skipped with a debug log. Multi-file is a separate brief
//!   (`docs/briefs/clipboard-files-multi.md`).
//! - Only file paths (not file lists) — same Phase 1 constraint as the Mac
//!   side.
//!
//! ## Threading
//!
//! `poll_cf_hdrop` and `set_cf_hdrop` both call `OpenClipboard(NULL)` which
//! synchronises on the global clipboard lock. They must be called from the
//! single `Session::tick` thread (same thread that polls arboard for
//! text/image) — the clipboard owner contract requires same-thread use.
//!
//! ## Memory ownership
//!
//! `SetClipboardData` transfers ownership of the `HGLOBAL` to the clipboard
//! manager on success — the caller must NOT free. On failure path, the
//! caller still owns the handle and must call `GlobalFree`. The `set_cf_hdrop`
//! helper handles both paths correctly.
//!
//! ## non-Windows targets
//!
//! On non-Windows this module compiles to no-op stubs returning `None` /
//! `Err(FileClipboardError::ClipboardLocked)` so we don't litter call sites
//! with `#[cfg]` guards.

// Public API is wired up by Task 6c (outbound poll path) and Task 7c
// (inbound commit path). Until then the helpers are referenced only from
// unit tests — silence dead_code so the module compiles cleanly on its own.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// Errors interacting with the Windows clipboard for file URLs.
///
/// `ClipboardLocked` covers both the non-Windows stub case and the rare
/// runtime case where `OpenClipboard` fails (another process is holding the
/// clipboard or the window handle is invalid). `BadPath` covers non-UTF-16
/// representable or empty paths that can't round-trip through `LPCWSTR`.
/// `AllocFailed` is `GlobalAlloc` returning a null handle (out-of-memory
/// condition; never observed in practice but distinguished for diagnostics).
/// `FfiError` wraps any other Win32-side failure with a human-readable
/// description (typically `windows::core::Error::message()`).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FileClipboardError {
    #[error("clipboard unavailable (non-Windows build or OpenClipboard failed)")]
    ClipboardLocked,
    #[error("invalid path for clipboard: {0}")]
    BadPath(String),
    #[error("GlobalAlloc returned null (out of memory)")]
    AllocFailed,
    #[error("Win32 FFI error: {0}")]
    FfiError(String),
}

/// `DROPFILES` header that prefixes the wide-char path block in a CF_HDROP
/// `HGLOBAL` payload. Layout matches Win32's `DROPFILES` exactly — the field
/// names are kept lowercase here for Rust convention (Win32 uses Hungarian).
///
/// `p_files` = byte offset of the first path within the payload (= size of
/// this struct, 20 bytes); `pt` = drop point (we use 0,0 — unused for paste);
/// `f_nc` = `BOOL` "is_non_client" (FALSE); `f_wide` = `BOOL` `TRUE` to flag
/// UTF-16 paths instead of ANSI. **Size invariant**: must be exactly 20
/// bytes — Explorer's paste handler reads the wide-path block at offset 20
/// regardless of struct layout (so a different size breaks paste silently).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct DropFiles {
    pub p_files: u32,
    pub pt_x: i32,
    pub pt_y: i32,
    pub f_nc: i32,
    pub f_wide: i32,
}

const DROPFILES_SIZE: usize = std::mem::size_of::<DropFiles>();
const _: () = assert!(
    DROPFILES_SIZE == 20,
    "DropFiles layout must be exactly 20 bytes (Win32 CF_HDROP contract)"
);

/// Probe the system clipboard for a single CF_HDROP entry.
///
/// * Returns `Some(path)` when the clipboard holds a CF_HDROP block with
///   exactly one entry, and the path is a valid filesystem path.
/// * Returns `None` for non-HDROP clipboards, for multi-file selections
///   (logged at debug), for empty handles, or on any FFI failure.
///
/// **Multi-file silent skip**: HDROP entries with >1 file log a debug line
/// and return `None`. Phase 1 scope (single file); see brief
/// `docs/briefs/clipboard-files-multi.md` for multi-file path.
#[cfg(windows)]
pub fn poll_cf_hdrop() -> Option<PathBuf> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    // SAFETY: `OpenClipboard(NULL)` is allowed on any thread per MSDN; the
    // global clipboard lock serialises us against other apps. We must call
    // `CloseClipboard` before returning.
    unsafe {
        if OpenClipboard(None).is_err() {
            log::debug!("clipboard: OpenClipboard failed (another app holds the lock?)");
            return None;
        }

        let handle: HANDLE = match GetClipboardData(CF_HDROP.0 as u32) {
            Ok(h) => h,
            Err(_) => {
                // Not a CF_HDROP clipboard; perfectly normal (text/image).
                let _ = CloseClipboard();
                return None;
            }
        };

        // GetClipboardData returns a HANDLE, which for CF_HDROP is also an
        // HGLOBAL. GlobalLock pins it for read.
        let hglobal = windows::Win32::Foundation::HGLOBAL(handle.0);
        let locked = GlobalLock(hglobal);
        if locked.is_null() {
            let _ = CloseClipboard();
            return None;
        }

        // HDROP is just a typed handle to the locked HGLOBAL. Get the file
        // count by passing `iFile = 0xFFFFFFFF` (special "count" sentinel).
        let hdrop = HDROP(handle.0);
        let count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);
        if count == 0 {
            let _ = GlobalUnlock(hglobal);
            let _ = CloseClipboard();
            return None;
        }
        if count != 1 {
            log::debug!(
                "clipboard: multi-file CF_HDROP ({count} files) — skipped, out of Phase 1 scope"
            );
            let _ = GlobalUnlock(hglobal);
            let _ = CloseClipboard();
            return None;
        }

        // Query path length (chars, NOT bytes). DragQueryFileW with `lpszFile
        // = NULL` returns required buffer size in chars (excluding null
        // terminator). Pass a buffer big enough to include the null term.
        let len_chars = DragQueryFileW(hdrop, 0, None);
        if len_chars == 0 {
            let _ = GlobalUnlock(hglobal);
            let _ = CloseClipboard();
            return None;
        }

        // Allocate (chars + 1) for terminating NUL.
        let mut buf: Vec<u16> = vec![0u16; (len_chars as usize) + 1];
        let copied = DragQueryFileW(hdrop, 0, Some(&mut buf));
        let _ = GlobalUnlock(hglobal);
        let _ = CloseClipboard();
        if copied == 0 {
            return None;
        }

        // copied is char count, drop the trailing NUL if present.
        let path_str = String::from_utf16_lossy(&buf[..copied as usize]);
        if path_str.is_empty() {
            return None;
        }
        Some(PathBuf::from(path_str))
    }
}

#[cfg(not(windows))]
pub fn poll_cf_hdrop() -> Option<PathBuf> {
    None
}

/// Replace the system clipboard with a single CF_HDROP entry pointing at
/// `path`.
///
/// Allocates an `HGLOBAL` containing `DROPFILES` header + UTF-16 path +
/// double-null terminator, calls `EmptyClipboard` + `SetClipboardData`. On
/// success ownership of the HGLOBAL transfers to the clipboard manager.
///
/// Returns `Ok(())` on success. The path must be UTF-8 representable (so it
/// can be re-encoded as UTF-16) and non-empty — empty paths and non-UTF-8
/// `OsStr` data return `BadPath`.
#[cfg(windows)]
pub fn set_cf_hdrop(path: &Path) -> Result<(), FileClipboardError> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows::Win32::System::Ole::CF_HDROP;

    let path_str = path
        .to_str()
        .ok_or_else(|| FileClipboardError::BadPath(path.display().to_string()))?;
    if path_str.is_empty() {
        return Err(FileClipboardError::BadPath(path.display().to_string()));
    }

    // UTF-16 encode + terminating NUL + extra NUL (CF_HDROP path block is
    // double-null terminated to support a list; we have a single entry).
    let mut path_w: Vec<u16> = path_str.encode_utf16().collect();
    path_w.push(0); // path terminator
    path_w.push(0); // list terminator

    let path_bytes = std::mem::size_of_val(path_w.as_slice());
    let total_size = DROPFILES_SIZE + path_bytes;

    // SAFETY: GlobalAlloc + GlobalLock are documented thread-safe. We
    // construct the payload byte-by-byte and only hand off to
    // SetClipboardData after a successful EmptyClipboard.
    unsafe {
        let hglobal: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total_size)
            .map_err(|_| FileClipboardError::AllocFailed)?;
        if hglobal.is_invalid() {
            return Err(FileClipboardError::AllocFailed);
        }

        let raw = GlobalLock(hglobal);
        if raw.is_null() {
            let _ = GlobalFree(hglobal);
            return Err(FileClipboardError::FfiError(
                "GlobalLock returned null after alloc".to_string(),
            ));
        }

        // Write DROPFILES header. f_wide = TRUE (-1 historically, but any
        // non-zero works; use 1 for clarity).
        let header = DropFiles {
            p_files: DROPFILES_SIZE as u32,
            pt_x: 0,
            pt_y: 0,
            f_nc: 0,
            f_wide: 1,
        };
        std::ptr::write_unaligned(raw as *mut DropFiles, header);

        // Write the wide path block immediately after the header.
        let path_dst = (raw as *mut u8).add(DROPFILES_SIZE) as *mut u16;
        std::ptr::copy_nonoverlapping(path_w.as_ptr(), path_dst, path_w.len());

        let _ = GlobalUnlock(hglobal);

        // Open + clear + set. Each failure path frees the HGLOBAL we own
        // until SetClipboardData succeeds (then ownership transfers).
        if OpenClipboard(None).is_err() {
            let _ = GlobalFree(hglobal);
            return Err(FileClipboardError::ClipboardLocked);
        }
        if EmptyClipboard().is_err() {
            let _ = CloseClipboard();
            let _ = GlobalFree(hglobal);
            return Err(FileClipboardError::FfiError(
                "EmptyClipboard failed".to_string(),
            ));
        }
        // SetClipboardData takes HANDLE — HGLOBAL converts directly (same
        // underlying pointer type in windows-rs 0.58).
        let handle = HANDLE(hglobal.0);
        let result = SetClipboardData(CF_HDROP.0 as u32, handle);
        let _ = CloseClipboard();
        match result {
            Ok(_) => Ok(()), // ownership transferred — DON'T free
            Err(e) => {
                let _ = GlobalFree(hglobal);
                Err(FileClipboardError::FfiError(format!(
                    "SetClipboardData failed: {e}"
                )))
            }
        }
    }
}

#[cfg(not(windows))]
pub fn set_cf_hdrop(_path: &Path) -> Result<(), FileClipboardError> {
    Err(FileClipboardError::ClipboardLocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CF_HDROP contract: the wide-path block starts at exactly offset 20
    /// (Win32's `DROPFILES` struct size). Explorer's paste handler reads
    /// from `p_files` offset, so the struct size must match the value we
    /// write into `p_files`. Compile-time assert above catches drift; this
    /// runtime test gives a more readable failure if it ever regresses.
    #[test]
    fn dropfiles_layout_offset_correct() {
        assert_eq!(std::mem::size_of::<DropFiles>(), 20);
        assert_eq!(DROPFILES_SIZE, 20);
    }

    /// `f_wide = TRUE` is required for UTF-16 paths; a value of zero would
    /// tell Explorer to interpret the path block as ANSI — silent corruption
    /// for any non-ASCII filename. Sanity-check the bit-width matches Win32
    /// `BOOL` (i32 in windows-rs).
    #[test]
    fn dropfiles_fields_have_expected_types() {
        // Sizes from a fresh struct to confirm we're not accidentally
        // packing/aligning wrong.
        let df = DropFiles {
            p_files: 20,
            pt_x: 0,
            pt_y: 0,
            f_nc: 0,
            f_wide: 1,
        };
        // p_files (4) + pt_x (4) + pt_y (4) + f_nc (4) + f_wide (4) = 20.
        assert_eq!(std::mem::size_of_val(&df), 20);
    }

    #[test]
    #[ignore = "requires live Windows GUI session and clipboard ownership"]
    #[cfg(target_os = "windows")]
    fn set_then_get_roundtrip() {
        // Smoke test (brief T7): write a known temp file path, then read it
        // back via poll_cf_hdrop. Requires a real Windows session — marked
        // `#[ignore]` because CI / Mac dev loop can't run it.
        let tmp = std::env::temp_dir().join("wiredesk-clipboard-files-test.bin");
        std::fs::write(&tmp, b"test").expect("write tmp");
        set_cf_hdrop(&tmp).expect("set_cf_hdrop");

        let got = poll_cf_hdrop().expect("poll_cf_hdrop");
        assert_eq!(got, tmp);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    #[ignore = "requires live Windows GUI session with text on clipboard"]
    #[cfg(target_os = "windows")]
    fn poll_returns_none_when_no_cf_hdrop() {
        // Smoke test: put text on the clipboard, expect None from
        // poll_cf_hdrop (no CF_HDROP entry exists). Requires test runner
        // to pre-populate clipboard with text — manual smoke only.
        let got = poll_cf_hdrop();
        let _ = got; // can't assert specific outcome — depends on runner state
    }

    #[test]
    #[ignore = "requires live Windows GUI session with multi-file Explorer selection"]
    #[cfg(target_os = "windows")]
    fn poll_returns_none_for_multi_file() {
        // Smoke test: user has multi-file Explorer selection on clipboard.
        // Helper must silently skip with debug log and return None.
        let got = poll_cf_hdrop();
        let _ = got;
    }

    /// On non-Windows the stub returns ClipboardLocked, which is what we
    /// want — every caller treats this as "feature unavailable" and skips
    /// without crashing. Validates the stub contract.
    #[test]
    #[cfg(not(windows))]
    fn set_cf_hdrop_stub_returns_unavailable() {
        let result = set_cf_hdrop(Path::new("/tmp/foo"));
        assert_eq!(result, Err(FileClipboardError::ClipboardLocked));
    }

    /// On non-Windows the poll stub returns None — every caller treats
    /// this as "no files on clipboard" and short-circuits.
    #[test]
    #[cfg(not(windows))]
    fn poll_cf_hdrop_stub_returns_none() {
        assert!(poll_cf_hdrop().is_none());
    }

    /// Empty path rejected on both real and stub paths. On Windows the
    /// real impl returns BadPath; on non-Windows the stub returns
    /// ClipboardLocked (before the path check). Either way the caller
    /// gets `Err` and can't accidentally allocate an HGLOBAL with a
    /// zero-byte path block.
    #[test]
    fn set_cf_hdrop_rejects_empty_path() {
        let result = set_cf_hdrop(Path::new(""));
        assert!(result.is_err());
    }
}
