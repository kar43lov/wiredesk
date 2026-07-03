//! Single-owner lock for the host's one shell slot.
//!
//! The Windows host has exactly one shell slot (`self.shell`). Two
//! consumers on the Mac side now compete for it:
//!   * `wd --exec` (one-shot, FIFO-serialised — many can queue),
//!   * interactive `wd` (a minutes-long PTY session — must never queue).
//!
//! A second `ShellOpen*` on the wire is rejected by the host with
//! `Error "shell already open"`. We front-run that with a client-side
//! owner state so a competing acquirer gets an immediate "shell busy"
//! terminal frame instead of a confusing host-side error.
//!
//! **Policy (plan Task 4 + Codex review):**
//!   * cross-kind is **fail-fast** — while an `Interactive` session holds the
//!     channel, any `Exec` acquire fails immediately (→ term exit 125), and
//!     while *any* `Exec` is present an `Interactive` acquire fails immediately.
//!     No queuing: a minutes-long interactive session must never block Claude's
//!     `--exec`.
//!   * exec-vs-exec is **not** fail-fast — `Exec` acquires **stack** (a
//!     ref-count). Multiple `wd --exec` handlers coexist as owners; their
//!     mutual FIFO ordering is enforced one level up by the
//!     `single_inflight: Arc<Mutex<()>>` mutex in the IPC acceptor.
//!
//! **Why a ref-count, not a single `Exec` state (Codex P2):** an exec handler
//! claims `Exec` *before* it queues on `single_inflight`, and releases it
//! *after* the inflight mutex. So the channel reads `Exec` for the whole span
//! that any exec is present — running **or** queued. Without the ref-count, a
//! queued exec B would only claim `Exec` after the running exec A released both
//! guards, leaving a window where the channel looked `Idle` and a newly
//! accepted interactive session could overtake B and force it to a false
//! "shell busy". The ref-count keeps the channel `Exec` across the A→B handoff,
//! so interactive only wins when the channel is *genuinely* idle.
//!
//! `try_acquire` returns `None` when the channel can't grant the requested kind,
//! so the caller can emit a "shell busy" frame and bail. On success it returns a
//! RAII `ShellChannelGuard` that releases its claim on drop — even if the holder
//! thread panics.

use std::sync::{Arc, Mutex};

/// Who currently owns the host's single shell slot. This is the *observable*
/// owner kind (for introspection / tests); the underlying state also tracks how
/// many exec handlers are stacked (see `ChannelState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellOwner {
    /// No shell session in flight — the channel is free to claim.
    Idle,
    /// One or more `wd --exec` one-shots hold the channel (exec-vs-exec FIFO is
    /// further serialised by `single_inflight`).
    Exec,
    /// An interactive `wd` PTY session holds the channel exclusively.
    Interactive,
}

/// Channel state behind the shared mutex. Tracks the interactive flag
/// (exclusive) and the count of exec handlers present (running or queued).
/// Public only because the `pub SharedShellOwner` alias names it; its fields
/// stay private, so it's driven solely via `try_acquire` / the RAII guard.
#[derive(Debug, Default)]
pub struct ChannelState {
    /// Number of exec handlers currently holding an `Exec` claim (running or
    /// queued on `single_inflight`). `> 0` ⇒ the channel is owned by exec.
    exec_refs: u32,
    /// An interactive session holds the channel exclusively.
    interactive: bool,
}

impl ChannelState {
    /// The observable owner kind derived from the raw state. Introspection only
    /// (used by `current_owner`) — production code drives the channel via
    /// `try_acquire` and the RAII guard, never by reading the owner kind.
    fn owner(&self) -> ShellOwner {
        if self.interactive {
            ShellOwner::Interactive
        } else if self.exec_refs > 0 {
            ShellOwner::Exec
        } else {
            ShellOwner::Idle
        }
    }
}

/// Shared owner-state handle. Cloned into the IPC acceptor (exec +
/// interactive handlers) and released via the RAII guard on teardown.
pub type SharedShellOwner = Arc<Mutex<ChannelState>>;

/// Construct a fresh, `Idle` shared owner. `main.rs` builds one and threads
/// it into `spawn_ipc_acceptor`, shared by the exec + interactive handlers.
pub fn new_shared_owner() -> SharedShellOwner {
    Arc::new(Mutex::new(ChannelState::default()))
}

/// The current observable owner of the channel. `Interactive` if an interactive
/// session holds it, else `Exec` if one or more exec handlers are present, else
/// `Idle`. On a poisoned mutex reports `Idle` (best-effort — the process is
/// already in a bad state). Introspection helper — currently only the test
/// suite reads the owner kind, so `#[allow(dead_code)]` keeps the release bin
/// (which drives the channel purely via `try_acquire`/guards) warning-free.
#[allow(dead_code)]
pub fn current_owner(owner: &SharedShellOwner) -> ShellOwner {
    owner.lock().map(|g| g.owner()).unwrap_or(ShellOwner::Idle)
}

/// RAII guard: while held, the guard's claim (one exec ref, or the interactive
/// flag) is registered on the channel. On drop it releases exactly that claim —
/// including during a panic unwind, so a panicking handler thread never strands
/// the lock.
pub struct ShellChannelGuard {
    owner: SharedShellOwner,
    /// Which claim this guard releases on drop: `Exec` decrements the ref-count,
    /// `Interactive` clears the flag. Never `Idle`.
    kind: ShellOwner,
}

impl Drop for ShellChannelGuard {
    fn drop(&mut self) {
        // Best-effort release. A poisoned mutex means some holder already
        // panicked while the lock was held; the process is in a bad state and
        // there's nothing useful to do here.
        if let Ok(mut guard) = self.owner.lock() {
            match self.kind {
                ShellOwner::Exec => guard.exec_refs = guard.exec_refs.saturating_sub(1),
                ShellOwner::Interactive => guard.interactive = false,
                ShellOwner::Idle => {}
            }
        }
    }
}

/// Try to claim the channel for `kind`.
///
///   * `Interactive` — exclusive: succeeds only when the channel is fully idle
///     (no interactive session, no exec present). Returns `None` otherwise.
///   * `Exec` — stacks: succeeds unless an interactive session holds the
///     channel, incrementing the exec ref-count. exec-vs-exec never fails here;
///     FIFO ordering is `single_inflight`'s job one level up.
///
/// On success returns a `ShellChannelGuard` that releases exactly this claim on
/// drop. `try_acquire(Idle)` is meaningless and returns `None`.
pub fn try_acquire(owner: &SharedShellOwner, kind: ShellOwner) -> Option<ShellChannelGuard> {
    let mut guard = owner.lock().expect("shell owner poisoned");
    match kind {
        ShellOwner::Interactive => {
            if guard.interactive || guard.exec_refs > 0 {
                return None;
            }
            guard.interactive = true;
        }
        ShellOwner::Exec => {
            if guard.interactive {
                return None;
            }
            guard.exec_refs += 1;
        }
        ShellOwner::Idle => {
            debug_assert!(
                false,
                "try_acquire(Idle) is meaningless — acquire Exec or Interactive"
            );
            return None;
        }
    }
    Some(ShellChannelGuard {
        owner: owner.clone(),
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn idle_acquire_interactive_ok() {
        let owner = new_shared_owner();
        let guard = try_acquire(&owner, ShellOwner::Interactive);
        assert!(guard.is_some(), "Idle channel must be claimable");
        assert_eq!(current_owner(&owner), ShellOwner::Interactive);
    }

    #[test]
    fn idle_acquire_exec_ok() {
        let owner = new_shared_owner();
        let guard = try_acquire(&owner, ShellOwner::Exec);
        assert!(guard.is_some());
        assert_eq!(current_owner(&owner), ShellOwner::Exec);
    }

    #[test]
    fn second_acquire_cross_kind_fails_fast() {
        // Interactive holds → an Exec acquire returns None immediately.
        let owner = new_shared_owner();
        let _held = try_acquire(&owner, ShellOwner::Interactive).expect("first acquire ok");
        assert!(
            try_acquire(&owner, ShellOwner::Exec).is_none(),
            "cross-kind acquire while Interactive-held must fail fast"
        );
        // And the reverse: Exec holds → Interactive fails fast.
        let owner2 = new_shared_owner();
        let _held2 = try_acquire(&owner2, ShellOwner::Exec).expect("first acquire ok");
        assert!(
            try_acquire(&owner2, ShellOwner::Interactive).is_none(),
            "cross-kind acquire while Exec-held must fail fast"
        );
    }

    #[test]
    fn interactive_is_exclusive_same_kind_fails_fast() {
        // A second interactive acquire while one is held must fail fast.
        let owner = new_shared_owner();
        let _held = try_acquire(&owner, ShellOwner::Interactive).expect("first acquire ok");
        assert!(try_acquire(&owner, ShellOwner::Interactive).is_none());
    }

    #[test]
    fn exec_acquires_stack_and_channel_stays_exec_until_last_release() {
        // Codex P2 fix: multiple exec handlers coexist (running + queued). The
        // channel reads `Exec` until the LAST exec guard drops — so interactive
        // can't overtake a queued exec through an A→B handoff window.
        let owner = new_shared_owner();
        let g1 = try_acquire(&owner, ShellOwner::Exec).expect("exec 1");
        let g2 = try_acquire(&owner, ShellOwner::Exec).expect("exec 2 stacks");
        assert_eq!(current_owner(&owner), ShellOwner::Exec);
        // While 2 execs are present, interactive must fail fast.
        assert!(
            try_acquire(&owner, ShellOwner::Interactive).is_none(),
            "interactive must not overtake while any exec is present"
        );
        // Drop one — still Exec (one remains). Interactive still refused.
        drop(g1);
        assert_eq!(current_owner(&owner), ShellOwner::Exec);
        assert!(try_acquire(&owner, ShellOwner::Interactive).is_none());
        // Drop the last — now Idle, interactive can claim.
        drop(g2);
        assert_eq!(current_owner(&owner), ShellOwner::Idle);
        assert!(try_acquire(&owner, ShellOwner::Interactive).is_some());
    }

    #[test]
    fn drop_guard_releases_channel() {
        let owner = new_shared_owner();
        {
            let _guard = try_acquire(&owner, ShellOwner::Interactive).expect("acquire ok");
            assert_eq!(current_owner(&owner), ShellOwner::Interactive);
        }
        assert_eq!(
            current_owner(&owner),
            ShellOwner::Idle,
            "guard drop must reset owner to Idle"
        );
        // Next acquire (any kind) succeeds now that it's free.
        assert!(try_acquire(&owner, ShellOwner::Exec).is_some());
    }

    #[test]
    fn panic_in_holder_thread_still_releases_channel() {
        // Mirror exec_bridge::panic_in_holder_thread_still_releases_slot:
        // a handler thread panicking mid-session must not strand the lock.
        let owner = new_shared_owner();
        let owner_clone = owner.clone();

        let handle = thread::spawn(move || {
            let _guard = try_acquire(&owner_clone, ShellOwner::Interactive)
                .expect("acquire ok in thread");
            panic!("simulated handler panic");
        });

        let result = handle.join();
        assert!(result.is_err(), "thread should have panicked");
        assert_eq!(
            current_owner(&owner),
            ShellOwner::Idle,
            "owner reset to Idle after panic unwind"
        );
    }
}
