//! Shell-event endpoints for the IPC handlers.
//!
//! The reader thread decodes the host's shell traffic and has to hand it to
//! whoever asked for it: a `wd --exec` run, or an interactive `wd` console.
//! Instead of multi-producing the GUI's own channel, each handler installs a
//! `Sender<ExecEvent>` into a slot and clears it on drop (RAII, so a panicking
//! handler can't strand it).
//!
//! There are two such slots, because since 2026-09-12 the host has two shell
//! slots and both can be live at once — see [`ShellSlots`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use wiredesk_exec_core::ExecEvent;

/// One shell-event endpoint. `None` when nothing is consuming that slot (the
/// common case — GUI alone). `Some(tx)` while a handler thread is running; it
/// installs its `tx` via [`ExecSlotGuard::install`] and clears it on drop. The
/// reader thread holds an `Arc` clone and consults the `Option` on every event.
pub type ExecEventSlot = Arc<Mutex<Option<mpsc::Sender<ExecEvent>>>>;

/// Which consumer an incoming shell event is addressed to. The reader decides
/// this from the opcode alone — `ShellOutput` is exec's, `PtyOutput` is the
/// console's — so it never has to know which handlers happen to be running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// `wd --exec` — the `Shell*` opcodes.
    Exec,
    /// Interactive `wd` — the `Pty*` opcodes.
    Pty,
}

/// Both endpoints plus the one bit of host-generation state routing needs.
///
/// Cheap to clone (all `Arc`s); each link's reader and the IPC acceptor get
/// their own clone of the same underlying slots.
#[derive(Clone)]
pub struct ShellSlots {
    /// Where `wd --exec` traffic goes.
    pub exec: ExecEventSlot,
    /// Where the interactive console's traffic goes.
    pub pty: ExecEventSlot,
    /// True while the connected host predates the dedicated pty slot, i.e.
    /// its console streams on the `Shell*` opcodes like `wd --exec` does.
    /// Set from `HelloAck` on every handshake, cleared on every link-down —
    /// so "we don't know yet" reads as the strict (non-fallback) mode.
    /// See [`route`] for what it enables and why it must not be on otherwise.
    pub legacy_fallback: Arc<AtomicBool>,
}

impl ShellSlots {
    /// Forget which generation the host was — back to the strict mode where
    /// nothing falls back. Called on every link-down, in lock-step with the
    /// host-info cache: until the next `HelloAck` says otherwise, "we don't
    /// know" must not read as "the old host".
    pub fn forget_host_generation(&self) {
        self.legacy_fallback.store(false, Ordering::Relaxed);
    }

    /// Fresh, empty slots in strict mode.
    // The IPC relay that drives these is macOS-only for now (see ipc.rs);
    // the type stays cross-platform so its tests run everywhere.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn new() -> Self {
        Self {
            exec: Arc::new(Mutex::new(None)),
            pty: Arc::new(Mutex::new(None)),
            legacy_fallback: Arc::new(AtomicBool::new(false)),
        }
    }

    /// True while either slot has a consumer — i.e. some shell session is
    /// open on the host and the wire may be busy with its output.
    pub fn any_installed(&self) -> bool {
        let live = |s: &ExecEventSlot| s.lock().map(|g| g.is_some()).unwrap_or(false);
        live(&self.exec) || live(&self.pty)
    }
}

impl Default for ShellSlots {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard for the slot. `install` swaps the slot's `Option` to
/// `Some(tx)`; `drop` restores it to `None` even if the IPC handler
/// thread panics. This keeps `single_inflight` semantics safe — the
/// next `wd --exec` connection won't see a dead Sender from a
/// previous session.
///
/// `dead_code` allowed here because the IPC handler that calls
/// `install` lives in Task 6 (`ipc.rs`, Mac-only). The struct is
/// already covered by lifecycle tests in this module.
#[allow(dead_code)]
pub struct ExecSlotGuard {
    slot: ExecEventSlot,
}

impl ExecSlotGuard {
    /// Set the slot to `Some(tx)`, returning a guard that restores it
    /// to `None` on drop. Overwrites any previous value (the caller
    /// is expected to hold the `single_inflight` lock, so this only
    /// runs serially).
    #[allow(dead_code)]
    pub fn install(slot: &ExecEventSlot, tx: mpsc::Sender<ExecEvent>) -> Self {
        let _ = slot.lock().expect("exec slot poisoned").replace(tx);
        Self { slot: slot.clone() }
    }
}

impl Drop for ExecSlotGuard {
    fn drop(&mut self) {
        // Best-effort clear. If the mutex is poisoned at drop time,
        // we can't do anything useful — the process is already in
        // a bad state.
        if let Ok(mut guard) = self.slot.lock() {
            *guard = None;
        }
    }
}

/// Hand one event to a slot. Returns the event back when the slot has no
/// consumer, so the caller can decide what else to do with it.
///
/// A `SendError` counts as delivered: the handler tore its receiver down
/// mid-stream, which is its business, and the reader keeps going.
fn deliver(slot: &ExecEventSlot, event: ExecEvent) -> Option<ExecEvent> {
    match slot.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(tx) => {
                let _ = tx.send(event);
                None
            }
            None => Some(event),
        },
        // Poisoned: some handler panicked while holding the lock. Nothing
        // useful to do here; treat the slot as absent.
        Err(_) => Some(event),
    }
}

/// Used by the reader on every shell-event packet: give it to the slot the
/// opcode names, and fall back to the other one **only** against a legacy
/// host.
///
/// The fallback exists because a host without the dedicated pty slot streams
/// its console on the `Shell*` opcodes, so an event the reader labels `Exec`
/// may in fact belong to an interactive session — and under the legacy policy
/// at most one consumer is ever installed, so "the other one" is unambiguous.
///
/// 🔴 It must never be on in dual mode. `wd --exec`'s post-run drain gives up
/// after 30 s; a host that answers later sends a `ShellExit`/`ShellClosed`
/// whose exec consumer is already gone. Falling back would put it in the
/// console's queue, and the interactive relay ends its session on `ShellExit`
/// — the owner's terminal would hang up on someone else's leftover.
pub fn route(slots: &ShellSlots, kind: SlotKind, event: ExecEvent) {
    let (primary, secondary) = match kind {
        SlotKind::Exec => (&slots.exec, &slots.pty),
        SlotKind::Pty => (&slots.pty, &slots.exec),
    };
    let Some(event) = deliver(primary, event) else {
        return;
    };
    if slots.legacy_fallback.load(Ordering::Relaxed) {
        let _ = deliver(secondary, event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// Slots with the legacy fallback armed, as a reader sees them while
    /// talking to a pre-two-slot host.
    fn legacy_slots() -> ShellSlots {
        let slots = ShellSlots::new();
        slots.legacy_fallback.store(true, Ordering::Relaxed);
        slots
    }

    #[test]
    fn routing_into_empty_slots_is_a_noop() {
        // Nothing is consuming: the GUI alone, no `wd` of any kind running.
        // Both modes must simply drop the event rather than panic.
        for slots in [ShellSlots::new(), legacy_slots()] {
            route(
                &slots,
                SlotKind::Exec,
                ExecEvent::ShellOutput(b"x".to_vec()),
            );
            route(&slots, SlotKind::Pty, ExecEvent::ShellExit(0));
        }
    }

    #[test]
    fn install_then_route_reaches_the_addressed_consumer() {
        let slots = ShellSlots::new();
        let (tx, rx) = mpsc::channel::<ExecEvent>();
        let guard = ExecSlotGuard::install(&slots.exec, tx);

        route(
            &slots,
            SlotKind::Exec,
            ExecEvent::ShellOutput(b"data".to_vec()),
        );
        route(&slots, SlotKind::Exec, ExecEvent::ShellExit(7));

        match rx.recv().unwrap() {
            ExecEvent::ShellOutput(b) => assert_eq!(b, b"data"),
            other => panic!("expected ShellOutput, got {other:?}"),
        }
        match rx.recv().unwrap() {
            ExecEvent::ShellExit(7) => {}
            other => panic!("expected ShellExit(7), got {other:?}"),
        }
        drop(guard);
    }

    #[test]
    fn each_consumer_only_gets_what_is_addressed_to_it() {
        // Both live at once — the dual-mode steady state. An event must never
        // cross over, in either direction: exec output in the console would
        // corrupt the owner's screen, console output in exec would corrupt the
        // agent's stdout (and could even look like a sentinel).
        let slots = ShellSlots::new();
        let (etx, erx) = mpsc::channel::<ExecEvent>();
        let (ptx, prx) = mpsc::channel::<ExecEvent>();
        let _eg = ExecSlotGuard::install(&slots.exec, etx);
        let _pg = ExecSlotGuard::install(&slots.pty, ptx);

        route(
            &slots,
            SlotKind::Exec,
            ExecEvent::ShellOutput(b"e".to_vec()),
        );
        route(&slots, SlotKind::Pty, ExecEvent::ShellOutput(b"p".to_vec()));

        assert_eq!(
            erx.try_recv().unwrap(),
            ExecEvent::ShellOutput(b"e".to_vec())
        );
        assert!(erx.try_recv().is_err(), "exec saw the console's event");
        assert_eq!(
            prx.try_recv().unwrap(),
            ExecEvent::ShellOutput(b"p".to_vec())
        );
        assert!(prx.try_recv().is_err(), "console saw exec's event");
    }

    #[test]
    fn legacy_host_falls_back_to_whichever_consumer_is_there() {
        // A pre-two-slot host streams its console on the `Shell*` opcodes, so
        // the reader labels the event `Exec` while it belongs to the
        // interactive session. Under the legacy policy only one consumer can
        // be installed, so handing it over is unambiguous.
        let slots = legacy_slots();
        let (ptx, prx) = mpsc::channel::<ExecEvent>();
        let _pg = ExecSlotGuard::install(&slots.pty, ptx);

        route(
            &slots,
            SlotKind::Exec,
            ExecEvent::ShellOutput(b"hi".to_vec()),
        );
        assert_eq!(
            prx.try_recv().unwrap(),
            ExecEvent::ShellOutput(b"hi".to_vec()),
            "legacy console must receive Shell*-addressed output"
        );
    }

    #[test]
    fn dual_mode_never_falls_back() {
        // The regression this guard exists for: a `ShellExit` arriving after
        // its exec consumer is gone (drain hit its 30 s cap, host answered
        // later). In dual mode it must be dropped — the interactive relay
        // ends the session on `ShellExit`, so a fallback here would hang up
        // the owner's live console on someone else's leftover.
        let slots = ShellSlots::new();
        assert!(
            !slots.legacy_fallback.load(Ordering::Relaxed),
            "strict by default"
        );
        let (ptx, prx) = mpsc::channel::<ExecEvent>();
        let _pg = ExecSlotGuard::install(&slots.pty, ptx);

        route(&slots, SlotKind::Exec, ExecEvent::ShellExit(0));
        route(&slots, SlotKind::Exec, ExecEvent::ShellClosed);
        assert!(
            prx.try_recv().is_err(),
            "a stray exec event must never reach the console"
        );
    }

    #[test]
    fn any_installed_tracks_both_slots() {
        // This is what the reader's liveness budget keys off: a console open
        // for minutes has to count as "the wire may be busy" just like a
        // running command does.
        let slots = ShellSlots::new();
        assert!(!slots.any_installed());
        let (tx, _rx) = mpsc::channel::<ExecEvent>();
        {
            let _g = ExecSlotGuard::install(&slots.pty, tx);
            assert!(slots.any_installed(), "an open console counts");
        }
        assert!(!slots.any_installed());
    }

    #[test]
    fn drop_clears_slot() {
        let slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let (tx, _rx) = mpsc::channel::<ExecEvent>();
        {
            let _guard = ExecSlotGuard::install(&slot, tx);
            assert!(slot.lock().unwrap().is_some(), "slot installed");
        }
        // Guard dropped at end of scope.
        assert!(slot.lock().unwrap().is_none(), "slot cleared on drop");
    }

    #[test]
    fn sender_disconnected_mid_stream_keeps_reader_alive() {
        // IPC handler installed slot, then dropped its receiver mid-run.
        // Reader-side broadcast on the dead sender must NOT panic —
        // SendError is silently swallowed by `let _ = tx.send(...)`.
        let slots = ShellSlots::new();
        let (tx, rx) = mpsc::channel::<ExecEvent>();
        let _guard = ExecSlotGuard::install(&slots.exec, tx);
        drop(rx); // Sender now dead.

        // Multiple routes must not panic.
        for _ in 0..3 {
            route(
                &slots,
                SlotKind::Exec,
                ExecEvent::ShellOutput(b"x".to_vec()),
            );
        }
        // And a dead receiver still counts as delivered: even in legacy mode
        // the event must not spill into the other slot.
        let slots = legacy_slots();
        let (tx, rx) = mpsc::channel::<ExecEvent>();
        let _guard = ExecSlotGuard::install(&slots.exec, tx);
        let (ptx, prx) = mpsc::channel::<ExecEvent>();
        let _pg = ExecSlotGuard::install(&slots.pty, ptx);
        drop(rx);
        route(&slots, SlotKind::Exec, ExecEvent::ShellExit(1));
        assert!(
            prx.try_recv().is_err(),
            "a dead receiver is not an empty slot"
        );
    }

    #[test]
    fn re_install_after_drop_routes_to_new_sender() {
        // Sequential `wd --exec` runs: each IPC handler installs its
        // own slot, drops it on completion, the next handler must get
        // fresh events (not stuck on the previous Sender).
        let slots = ShellSlots::new();

        // Run 1.
        let (tx1, rx1) = mpsc::channel::<ExecEvent>();
        {
            let _g = ExecSlotGuard::install(&slots.exec, tx1);
            route(&slots, SlotKind::Exec, ExecEvent::ShellExit(0));
        }
        assert!(matches!(rx1.recv().unwrap(), ExecEvent::ShellExit(0)));
        assert!(slots.exec.lock().unwrap().is_none());

        // Run 2.
        let (tx2, rx2) = mpsc::channel::<ExecEvent>();
        {
            let _g = ExecSlotGuard::install(&slots.exec, tx2);
            route(&slots, SlotKind::Exec, ExecEvent::ShellExit(1));
        }
        assert!(matches!(rx2.recv().unwrap(), ExecEvent::ShellExit(1)));
        // Run 1's rx must NOT have seen run 2's event.
        assert!(
            rx1.try_recv().is_err(),
            "run-1 receiver was already dropped"
        );
    }

    #[test]
    fn panic_in_holder_thread_still_releases_slot() {
        // Worst-case lifecycle: handler thread panics mid-run. Drop
        // guard semantics in std-Rust unwind through the panic and
        // run our impl Drop — slot must end up cleared, even though
        // we never reached a graceful shutdown path.
        let slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let (tx, _rx) = mpsc::channel::<ExecEvent>();
        let slot_clone = slot.clone();

        let handle = thread::spawn(move || {
            let _g = ExecSlotGuard::install(&slot_clone, tx);
            panic!("simulated handler panic");
        });

        let result = handle.join();
        assert!(result.is_err(), "thread should have panicked");
        // Drop must have run during unwind — slot empty now.
        assert!(slot.lock().unwrap().is_none(), "slot cleared after panic");
    }
}
