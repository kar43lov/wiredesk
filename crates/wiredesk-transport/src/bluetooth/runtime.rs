//! Embedded tokio runtime for `BluetoothLeTransport`.
//!
//! btleplug (macOS) and `windows-rs` GATT (Windows) are async-only. The rest
//! of WireDesk is sync (std::thread + mpsc). Rather than convert every
//! callsite to async, the BLE transport owns a small multi-thread tokio
//! runtime here and exposes sync `send` / `recv` via `block_on`. Cost: two
//! worker threads per `BluetoothLeTransport` instance plus the per-call
//! block_on overhead — both negligible at our packet rate.

use std::future::Future;

use tokio::runtime::{Builder, Runtime};

/// Two worker threads — one for the notification stream pump on the Mac
/// side / WriteRequested handler on the Win side, and one slot for the
/// blocking caller's `send` future. Anything heavier risks contending with
/// the BLE stack itself.
const WORKER_THREADS: usize = 2;

/// Sync wrapper around a multi-thread tokio runtime.
pub struct EmbeddedRuntime {
    rt: Runtime,
}

impl EmbeddedRuntime {
    /// Build a multi-thread runtime sized for the BLE transport. Returns
    /// `Err` if the OS refuses to spawn the worker threads (rare; treat as
    /// a fatal transport-init error).
    pub fn new() -> std::io::Result<Self> {
        let rt = Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .thread_name("wiredesk-ble")
            .enable_all()
            .build()?;
        Ok(Self { rt })
    }

    /// Block the current OS thread until `fut` resolves. Used from sync
    /// `Transport::send` / `recv` to bridge into async BLE calls.
    pub fn block_on<F: Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    /// Spawn a fire-and-forget background task on the runtime — used for
    /// the long-lived notification pump.
    pub fn spawn<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.rt.spawn(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_runs_to_completion() {
        let rt = EmbeddedRuntime::new().expect("runtime build");
        let v = rt.block_on(async { 42 });
        assert_eq!(v, 42);
    }

    #[test]
    fn spawn_runs_on_runtime_threads() {
        // The spawned future increments a counter; we collect the result
        // through a oneshot to avoid sharing state across runtimes.
        let rt = EmbeddedRuntime::new().expect("runtime build");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = rt.spawn(async move {
            tx.send(7u32).expect("send");
        });
        let v = rt.block_on(async { rx.await.expect("recv") });
        assert_eq!(v, 7);
        rt.block_on(async { handle.await.expect("join") });
    }

    #[test]
    fn block_on_chains_async_calls() {
        // Sanity check that a multi-step async sequence (timer + future
        // composition) actually drives — would catch a missing
        // `enable_all()` regression.
        let rt = EmbeddedRuntime::new().expect("runtime build");
        let v = rt.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            10 + 5
        });
        assert_eq!(v, 15);
    }
}
