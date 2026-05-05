//! Shell-event broadcast slot for the IPC handler.
//!
//! `reader_thread` already emits `ShellOutput` / `ShellExit` /
//! `ShellError` to the GUI's `events_tx`. The IPC handler (Task 6)
//! needs the same stream so it can drive the shared runner from a
//! Unix socket. Instead of multi-producing the existing channel,
//! we add a parallel optional `Sender<ExecEvent>` slot that the IPC
//! handler installs on accept and clears on drop (RAII so panics
//! don't strand the slot).

use std::sync::{mpsc, Arc, Mutex};

use wiredesk_exec_core::ExecEvent;

/// Optional broadcast endpoint for shell events. `None` when no IPC
/// connection is active (the common case — GUI alone). `Some(tx)`
/// while a `wd --exec` IPC handler thread is running; the handler
/// installs its `tx` via `ExecSlotGuard::install` and clears it on
/// drop. The reader thread holds an `Arc` clone and consults the
/// `Option` on every shell event.
pub type ExecEventSlot = Arc<Mutex<Option<mpsc::Sender<ExecEvent>>>>;

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

/// Helper used by `reader_thread` on every shell-event packet:
/// fan-out to the IPC slot if installed, swallow Sender-disconnect
/// errors silently (the IPC handler may have detached mid-stream).
pub fn broadcast_exec_event(slot: &ExecEventSlot, event: ExecEvent) {
    if let Ok(guard) = slot.lock() {
        if let Some(tx) = guard.as_ref() {
            // SendError = receiver dropped. Acceptable — IPC handler
            // tore down its rx, reader keeps going.
            let _ = tx.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn slot_none_broadcast_is_noop() {
        // Reader broadcasting into an empty slot must not panic.
        let slot: ExecEventSlot = Arc::new(Mutex::new(None));
        broadcast_exec_event(&slot, ExecEvent::ShellOutput(b"hello".to_vec()));
        // No-op verified by absence of panic; nothing else to assert.
    }

    #[test]
    fn install_then_broadcast_routes_event() {
        let slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel::<ExecEvent>();
        let guard = ExecSlotGuard::install(&slot, tx);

        broadcast_exec_event(&slot, ExecEvent::ShellOutput(b"data".to_vec()));
        broadcast_exec_event(&slot, ExecEvent::ShellExit(7));

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
        let slot: ExecEventSlot = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::channel::<ExecEvent>();
        let _guard = ExecSlotGuard::install(&slot, tx);
        drop(rx); // Sender now dead.

        // Multiple broadcasts must not panic.
        for _ in 0..3 {
            broadcast_exec_event(&slot, ExecEvent::ShellOutput(b"x".to_vec()));
        }
    }

    #[test]
    fn re_install_after_drop_routes_to_new_sender() {
        // Sequential `wd --exec` runs: each IPC handler installs its
        // own slot, drops it on completion, the next handler must get
        // fresh events (not stuck on the previous Sender).
        let slot: ExecEventSlot = Arc::new(Mutex::new(None));

        // Run 1.
        let (tx1, rx1) = mpsc::channel::<ExecEvent>();
        {
            let _g = ExecSlotGuard::install(&slot, tx1);
            broadcast_exec_event(&slot, ExecEvent::ShellExit(0));
        }
        assert!(matches!(rx1.recv().unwrap(), ExecEvent::ShellExit(0)));
        assert!(slot.lock().unwrap().is_none());

        // Run 2.
        let (tx2, rx2) = mpsc::channel::<ExecEvent>();
        {
            let _g = ExecSlotGuard::install(&slot, tx2);
            broadcast_exec_event(&slot, ExecEvent::ShellExit(1));
        }
        assert!(matches!(rx2.recv().unwrap(), ExecEvent::ShellExit(1)));
        // Run 1's rx must NOT have seen run 2's event.
        assert!(rx1.try_recv().is_err(), "run-1 receiver was already dropped");
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
