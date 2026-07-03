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
//! **Policy (plan Task 4):**
//!   * cross-kind is **fail-fast** — while an `Interactive` session holds
//!     the channel, an `Exec` acquire fails immediately (→ term exit 125),
//!     and vice-versa. No queuing: a minutes-long interactive session must
//!     never block Claude's `--exec`.
//!   * exec-vs-exec **FIFO is retained** by the existing
//!     `single_inflight: Arc<Mutex<()>>`, nested *under* the `Exec` owner
//!     state — the exec handler first `try_acquire(Exec)` (cross-kind
//!     fail-fast), then blocks on `single_inflight` to serialise
//!     exec-vs-exec as today. That mutex lives in the IPC acceptor; the
//!     wiring lands in Task 7.
//!
//! `try_acquire` returns `None` when the channel is not `Idle`, so the
//! caller can emit a "shell busy" frame and bail. On success it returns a
//! RAII `ShellChannelGuard` that resets the owner to `Idle` on drop —
//! even if the holder thread panics.

use std::sync::{Arc, Mutex};

/// Who currently owns the host's single shell slot.
///
/// `dead_code` allowed on the busy variants: they're constructed only by
/// `try_acquire` callers, which land in Task 7 — until then only the
/// module's tests exercise them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ShellOwner {
    /// No shell session in flight — the channel is free to claim.
    Idle,
    /// A `wd --exec` one-shot holds the channel (exec-vs-exec FIFO is
    /// further serialised by `single_inflight`, nested under this state).
    Exec,
    /// An interactive `wd` PTY session holds the channel exclusively.
    Interactive,
}

/// Shared owner-state handle. Cloned into the IPC acceptor (exec +
/// interactive handlers) and reset via the RAII guard on teardown.
pub type SharedShellOwner = Arc<Mutex<ShellOwner>>;

/// Construct a fresh, `Idle` shared owner.
///
/// `dead_code` allowed until Task 7 threads this into `spawn_ipc_acceptor`;
/// the lifecycle is exercised by this module's tests, and `main.rs`
/// constructs one to reserve the wiring point.
#[allow(dead_code)]
pub fn new_shared_owner() -> SharedShellOwner {
    Arc::new(Mutex::new(ShellOwner::Idle))
}

/// RAII guard: while held, the channel owner is set to the acquired
/// kind. On drop it resets the owner to `Idle` — including during a
/// panic unwind, so a panicking handler thread never strands the lock.
///
/// `dead_code` allowed until Task 7 wires the acquire calls into the
/// IPC acceptor; the lifecycle is already covered by this module's tests.
#[allow(dead_code)]
pub struct ShellChannelGuard {
    owner: SharedShellOwner,
}

impl Drop for ShellChannelGuard {
    fn drop(&mut self) {
        // Best-effort reset. A poisoned mutex means some holder already
        // panicked while the lock was held; the process is in a bad
        // state and there's nothing useful to do here.
        if let Ok(mut guard) = self.owner.lock() {
            *guard = ShellOwner::Idle;
        }
    }
}

/// Try to claim the channel for `kind`. Returns `Some(guard)` only when
/// the channel is currently `Idle`; otherwise `None` (busy — cross-kind
/// or same-kind, both fail-fast at this layer). exec-vs-exec FIFO is
/// handled one level up by `single_inflight`, not here.
#[allow(dead_code)]
pub fn try_acquire(owner: &SharedShellOwner, kind: ShellOwner) -> Option<ShellChannelGuard> {
    debug_assert_ne!(
        kind,
        ShellOwner::Idle,
        "try_acquire(Idle) is meaningless — acquire Exec or Interactive"
    );
    let mut guard = owner.lock().expect("shell owner poisoned");
    if *guard != ShellOwner::Idle {
        return None;
    }
    *guard = kind;
    Some(ShellChannelGuard {
        owner: owner.clone(),
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
        assert_eq!(*owner.lock().unwrap(), ShellOwner::Interactive);
    }

    #[test]
    fn idle_acquire_exec_ok() {
        let owner = new_shared_owner();
        let guard = try_acquire(&owner, ShellOwner::Exec);
        assert!(guard.is_some());
        assert_eq!(*owner.lock().unwrap(), ShellOwner::Exec);
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
    fn second_acquire_same_kind_fails_fast() {
        // Same-kind is also fail-fast at this layer (exec-vs-exec FIFO
        // is a separate single_inflight concern, not this lock's job).
        let owner = new_shared_owner();
        let _held = try_acquire(&owner, ShellOwner::Interactive).expect("first acquire ok");
        assert!(try_acquire(&owner, ShellOwner::Interactive).is_none());
    }

    #[test]
    fn drop_guard_releases_channel() {
        let owner = new_shared_owner();
        {
            let _guard = try_acquire(&owner, ShellOwner::Interactive).expect("acquire ok");
            assert_eq!(*owner.lock().unwrap(), ShellOwner::Interactive);
        }
        assert_eq!(
            *owner.lock().unwrap(),
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
            *owner.lock().unwrap(),
            ShellOwner::Idle,
            "owner reset to Idle after panic unwind"
        );
    }
}
