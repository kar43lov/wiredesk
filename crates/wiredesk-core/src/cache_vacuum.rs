//! Cache directory vacuum helper for the clipboard-files feature.
//!
//! Receive-side writes inbound files into a per-user cache directory
//! (`~/Library/Caches/WireDesk` on macOS, `%TEMP%\WireDesk` on Windows).
//! Without periodic cleanup that directory grows unbounded — a 20 MB
//! cap × N pastes per day adds up. The startup vacuum trims everything
//! older than `older_than` (default 24h via `apps/*/src/main.rs`).
//!
//! The split into `should_remove` (pure) + `vacuum_cache_dir` (fs side
//! effects) keeps the time-comparison logic unit-testable without
//! touching the filesystem, which matters because mtime-rewriting on
//! macOS/Linux requires either `filetime` or `utimensat` libc calls.
//!
//! Errors on individual file removals are logged via `log::warn` but
//! never propagate — partial cleanup is better than panicking at
//! startup. Non-existent target directory returns `Ok(0)` (first-run
//! behaviour). Subdirectories are skipped silently: the cache only
//! stores top-level files written by `IncomingClipboard::commit`.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Pure predicate: should a file with this mtime be removed given the
/// configured age threshold? Returns `false` on clock skew (mtime in
/// the future) — we'd rather keep a file we shouldn't have than nuke a
/// fresh one because the system clock jumped backwards.
pub fn should_remove(mtime: SystemTime, now: SystemTime, older_than: Duration) -> bool {
    match now.duration_since(mtime) {
        Ok(age) => age > older_than,
        Err(_) => false, // mtime > now (clock skew or future-stamped file)
    }
}

/// Remove every regular file under `dir` whose mtime is older than
/// `older_than`. Subdirectories are not traversed and not removed.
///
/// Returns the number of files successfully removed. A missing
/// directory is not an error — callers run this at startup and the
/// cache may not exist yet.
///
/// Per-file errors (permission denied, race with concurrent remove,
/// unreadable metadata) are logged at `warn` and skipped — the next
/// startup will try again. Only a failure to enumerate the directory
/// itself surfaces as `Err`.
pub fn vacuum_cache_dir(dir: &Path, older_than: Duration) -> Result<usize, std::io::Error> {
    if !dir.exists() {
        return Ok(0);
    }

    let now = SystemTime::now();
    let mut removed = 0usize;

    let entries = std::fs::read_dir(dir)?;
    for entry_res in entries {
        let entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                log::warn!("cache_vacuum: read_dir entry error: {err}");
                continue;
            }
        };

        let path = entry.path();
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                log::warn!("cache_vacuum: metadata error for {}: {err}", path.display());
                continue;
            }
        };

        if !metadata.is_file() {
            // Skip subdirectories, symlinks-to-dir, special files.
            continue;
        }

        let mtime = match metadata.modified() {
            Ok(t) => t,
            Err(err) => {
                log::warn!(
                    "cache_vacuum: mtime unavailable for {}: {err}",
                    path.display()
                );
                continue;
            }
        };

        if !should_remove(mtime, now, older_than) {
            continue;
        }

        match std::fs::remove_file(&path) {
            Ok(()) => {
                removed += 1;
                log::debug!("cache_vacuum: removed {}", path.display());
            }
            Err(err) => {
                log::warn!(
                    "cache_vacuum: remove_file failed for {}: {err}",
                    path.display()
                );
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};
    use std::fs;
    use tempfile::TempDir;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    #[test]
    fn should_remove_old_file() {
        let now = SystemTime::now();
        let mtime = now - Duration::from_secs(25 * 3600); // 25h ago
        assert!(should_remove(mtime, now, DAY));
    }

    #[test]
    fn should_remove_young_file() {
        let now = SystemTime::now();
        let mtime = now - Duration::from_secs(23 * 3600); // 23h ago
        assert!(!should_remove(mtime, now, DAY));
    }

    #[test]
    fn should_remove_exact_boundary() {
        // Exactly older_than: should NOT be removed (strict `>`).
        let now = SystemTime::now();
        let mtime = now - DAY;
        assert!(!should_remove(mtime, now, DAY));
    }

    #[test]
    fn should_remove_future_mtime() {
        // Clock skew: mtime in the future — don't panic, don't remove.
        let now = SystemTime::now();
        let mtime = now + Duration::from_secs(3600);
        assert!(!should_remove(mtime, now, DAY));
    }

    #[test]
    fn vacuum_missing_dir_ok() {
        let nonexistent = std::path::PathBuf::from("/tmp/wd-cache-vacuum-nonexistent-xyz123");
        // Make sure it really doesn't exist.
        let _ = fs::remove_dir_all(&nonexistent);
        let result = vacuum_cache_dir(&nonexistent, DAY);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn vacuum_empty_dir_returns_zero() {
        let dir = TempDir::new().expect("tempdir");
        let removed = vacuum_cache_dir(dir.path(), DAY).expect("vacuum");
        assert_eq!(removed, 0);
    }

    #[test]
    fn vacuum_dir_removes_old_files() {
        let dir = TempDir::new().expect("tempdir");

        // Create three files: one old, one fresh, one mid-age.
        let old_path = dir.path().join("old.bin");
        let fresh_path = dir.path().join("fresh.bin");
        let mid_path = dir.path().join("mid.bin");
        fs::write(&old_path, b"old content").expect("write old");
        fs::write(&fresh_path, b"fresh content").expect("write fresh");
        fs::write(&mid_path, b"mid content").expect("write mid");

        // Rewind mtimes: old = 30h ago, mid = 25h ago, fresh = now.
        let now_ft = FileTime::from_system_time(SystemTime::now());
        let old_ft =
            FileTime::from_system_time(SystemTime::now() - Duration::from_secs(30 * 3600));
        let mid_ft =
            FileTime::from_system_time(SystemTime::now() - Duration::from_secs(25 * 3600));
        set_file_mtime(&old_path, old_ft).expect("set old mtime");
        set_file_mtime(&mid_path, mid_ft).expect("set mid mtime");
        set_file_mtime(&fresh_path, now_ft).expect("set fresh mtime");

        let removed = vacuum_cache_dir(dir.path(), DAY).expect("vacuum");
        assert_eq!(removed, 2, "old and mid should be removed");
        assert!(!old_path.exists(), "old file should be gone");
        assert!(!mid_path.exists(), "mid file should be gone");
        assert!(fresh_path.exists(), "fresh file should survive");
    }

    #[test]
    fn vacuum_dir_ignores_subdirs() {
        let dir = TempDir::new().expect("tempdir");

        // Create old subdirectory with old contents.
        let subdir = dir.path().join("subdir");
        fs::create_dir(&subdir).expect("mkdir");
        let sub_file = subdir.join("nested.bin");
        fs::write(&sub_file, b"nested").expect("write nested");

        // Create old top-level file.
        let top_file = dir.path().join("top.bin");
        fs::write(&top_file, b"top").expect("write top");

        let old_ft =
            FileTime::from_system_time(SystemTime::now() - Duration::from_secs(30 * 3600));
        set_file_mtime(&top_file, old_ft).expect("set top mtime");
        set_file_mtime(&sub_file, old_ft).expect("set nested mtime");
        // Also age the subdir itself.
        set_file_mtime(&subdir, old_ft).expect("set subdir mtime");

        let removed = vacuum_cache_dir(dir.path(), DAY).expect("vacuum");
        assert_eq!(removed, 1, "only top-level file removed");
        assert!(!top_file.exists(), "top-level removed");
        assert!(subdir.exists(), "subdir untouched");
        assert!(sub_file.exists(), "nested file untouched");
    }

    #[test]
    fn vacuum_dir_keeps_fresh_files() {
        let dir = TempDir::new().expect("tempdir");
        let fresh = dir.path().join("fresh.bin");
        fs::write(&fresh, b"content").expect("write");

        let removed = vacuum_cache_dir(dir.path(), DAY).expect("vacuum");
        assert_eq!(removed, 0);
        assert!(fresh.exists());
    }

    #[test]
    fn vacuum_zero_threshold_removes_everything() {
        // older_than = 0 → any file with mtime <= now gets removed.
        let dir = TempDir::new().expect("tempdir");
        let f1 = dir.path().join("a.bin");
        let f2 = dir.path().join("b.bin");
        fs::write(&f1, b"a").expect("write a");
        fs::write(&f2, b"b").expect("write b");

        // Make sure mtime is strictly less than now (>0 duration_since).
        std::thread::sleep(Duration::from_millis(10));

        let removed = vacuum_cache_dir(dir.path(), Duration::from_secs(0)).expect("vacuum");
        assert_eq!(removed, 2);
        assert!(!f1.exists());
        assert!(!f2.exists());
    }
}
