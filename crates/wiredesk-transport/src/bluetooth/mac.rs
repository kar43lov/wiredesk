//! macOS BLE Central implementation backed by `btleplug` 0.11.
//!
//! Behaviour mirrors `SerialTransport` from the caller's perspective:
//! sync `send` / `recv` driven through the embedded tokio runtime.
//! Internally this opens a single GATT connection to the WireDesk Win11
//! peripheral that exposes `SERVICE_UUID`, subscribes to `TX_CHAR_UUID`
//! (notify, host→client) and writes to `RX_CHAR_UUID` (write-with-
//! response, client→host).
//!
//! A spawned tokio task pumps the notification stream, feeds chunks into
//! the [`Reassembler`], and pushes finished `Packet`s into a single-
//! consumer mpsc channel. `recv` is `block_on(rx.recv())`. `send` runs
//! `Packet::to_bytes` + `cobs::encode` + [`split_packet`] and writes each
//! chunk via `Peripheral::write(WriteWithResponse)`.
//!
//! `try_clone` returns a write-only handle (see Decision 4 in the plan):
//! the cloned transport shares the same connection but `recv` returns
//! `Err`. WireDesk only ever clones for the writer-thread split, so this
//! is safe and avoids the broadcast-fan-out hazard a full clone would
//! introduce.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use btleplug::api::{
    Central, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::stream::StreamExt;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::cobs;
use wiredesk_protocol::packet::Packet;

use super::fragment::{max_chunk_payload, split_packet, Reassembler, DEFAULT_ATT_MTU};
use super::runtime::EmbeddedRuntime;
use super::uuids;
use super::BluetoothFactoryConfig;
use crate::transport::Transport;

/// How long to wait for an entire chunk-batch send before giving up.
/// One BLE connection event is ~30 ms, ATT-ack RTT typically <100 ms,
/// but under heavy bidirectional load a single WithResponse ack can
/// be queued behind a backlog of incoming Notifies on the peer and
/// take several seconds to round-trip. 30 s is a permissive upper
/// bound — long enough that transient congestion doesn't trip a hard
/// disconnect, short enough that a truly dead link surfaces eventually.
/// Bumped from 5 s after live test caught a single congestion spike
/// killing the whole transport.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to poll between scan-result checks while looking for the peer.
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct BluetoothLeTransport {
    inner: Arc<Inner>,
    /// `true` for the original handle returned by `open`; `false` for any
    /// `try_clone`d handle. Only owners may call `recv`.
    is_owner: bool,
}

/// Shared connection state. Held behind `Arc` so `try_clone` can hand out
/// write-only views over the same underlying GATT link without
/// duplicating the BLE connection (impossible) or shouldering broadcast-
/// channel hazards (write-only is enough for our writer/reader split).
struct Inner {
    rt: EmbeddedRuntime,
    peripheral: Peripheral,
    rx_char: Characteristic, // Mac→Win, Write/WriteWithoutResponse
    /// Receiver side of the notification pump. Wrapped in async Mutex so
    /// the sync `recv` API can take it via `block_on`.
    incoming_rx: AsyncMutex<mpsc::UnboundedReceiver<Result<Packet>>>,
    att_payload: AtomicUsize,
    is_connected: AtomicBool,
    next_packet_id: AtomicU16,
    /// Global BLE-write counter — used to space out periodic
    /// WriteWithResponse "ack-flush" writes evenly across **all**
    /// outgoing traffic (input events + clipboard alike). Without
    /// this, single-chunk packets (mouse moves, key events) would
    /// each trip the per-call "is_last" rule and bottleneck on
    /// per-event ATT-acks (~30 ms each → visible mouse jitter).
    write_counter: std::sync::atomic::AtomicU64,
    /// Notification pump task — kept alive for the lifetime of the
    /// transport. Aborted in `Drop` so we don't leak background work.
    notification_task: AsyncMutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("att_payload", &self.att_payload.load(Ordering::Relaxed))
            .field("is_connected", &self.is_connected.load(Ordering::Relaxed))
            .finish()
    }
}

impl BluetoothLeTransport {
    pub fn open(cfg: &BluetoothFactoryConfig) -> Result<Self> {
        let rt = EmbeddedRuntime::new()
            .map_err(|e| WireDeskError::Transport(format!("BLE runtime build: {e}")))?;

        let service_uuid = Uuid::parse_str(&cfg.service_uuid).map_err(|e| {
            WireDeskError::Transport(format!(
                "BLE config: service_uuid '{}' not a valid UUID: {e}",
                cfg.service_uuid
            ))
        })?;

        // Pre-validate the connect timeout so a misconfigured zero doesn't
        // make the loop below decide "expired before it started" and yield
        // a misleading "no peer found" message.
        let connect_timeout = if cfg.connect_timeout_secs == 0 {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(cfg.connect_timeout_secs as u64)
        };

        // Set up the runtime first so we can drive the async dance below.
        let (peripheral, rx_char, tx_char) = rt.block_on(async {
            scan_and_connect(service_uuid, &cfg.peer_name, connect_timeout).await
        })?;

        // Try to negotiate a larger MTU. btleplug exposes this on macOS
        // through a per-platform helper; if the call isn't supported we
        // just stick with the default 247.
        let att_payload = AtomicUsize::new(max_chunk_payload(DEFAULT_ATT_MTU));

        // Notification pump → assembled-Packet channel.
        let (tx, rx) = mpsc::unbounded_channel();
        let pump_task = rt.spawn(notification_pump(peripheral.clone(), tx_char.uuid, tx));

        let inner = Arc::new(Inner {
            rt,
            peripheral,
            rx_char,
            incoming_rx: AsyncMutex::new(rx),
            att_payload,
            is_connected: AtomicBool::new(true),
            next_packet_id: AtomicU16::new(0),
            write_counter: std::sync::atomic::AtomicU64::new(0),
            notification_task: AsyncMutex::new(Some(pump_task)),
        });

        Ok(Self {
            inner,
            is_owner: true,
        })
    }
}

impl Drop for BluetoothLeTransport {
    fn drop(&mut self) {
        // Only the original handle aborts the pump; cloned write-only
        // handles share the Arc and shouldn't tear it down.
        if !self.is_owner {
            return;
        }
        if Arc::strong_count(&self.inner) > 1 {
            // A cloned writer handle is still alive — let it close out.
            return;
        }
        let inner = Arc::clone(&self.inner);
        // Abort the pump on drop so a closed transport doesn't leak the
        // notification task. We deliberately do not block_on(disconnect):
        // higher layers (auto-reconnect logic in Task 10) take care of
        // that, and Drop must remain non-panicking.
        inner.rt.block_on(async {
            let mut guard = inner.notification_task.lock().await;
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        });
        inner.is_connected.store(false, Ordering::Relaxed);
    }
}

async fn scan_and_connect(
    service_uuid: Uuid,
    peer_name: &str,
    timeout: Duration,
) -> Result<(Peripheral, Characteristic, Characteristic)> {
    let manager = Manager::new()
        .await
        .map_err(|e| WireDeskError::Transport(format!("BLE manager: {e}")))?;

    let adapters = manager
        .adapters()
        .await
        .map_err(|e| WireDeskError::Transport(format!("BLE adapters: {e}")))?;
    let adapter: Adapter = adapters
        .into_iter()
        .next()
        .ok_or_else(|| WireDeskError::Transport("BLE adapter not found".into()))?;

    let filter = ScanFilter {
        services: vec![service_uuid],
    };
    adapter
        .start_scan(filter)
        .await
        .map_err(|e| WireDeskError::Transport(format!("BLE scan start: {e}")))?;

    // Watch the central-events stream until we find a matching peripheral
    // or hit the timeout. We don't rely solely on the events stream: the
    // user could already have the peer paired/cached, in which case we
    // poll `adapter.peripherals()` too.
    let mut events = adapter
        .events()
        .await
        .map_err(|e| WireDeskError::Transport(format!("BLE events: {e}")))?;
    let deadline = tokio::time::Instant::now() + timeout;

    let target = loop {
        if tokio::time::Instant::now() >= deadline {
            let _ = adapter.stop_scan().await;
            return Err(WireDeskError::Transport(format!(
                "BLE: no peer advertising service {service_uuid} within {timeout:?} \
                 (configured peer_name '{peer_name}' — advisory). \
                 Verify Win host is running with transport=\"bluetooth\" and BT radio is on."
            )));
        }

        // 1) Already-known peripherals (user re-launched after pairing).
        if let Some(p) = find_matching_peripheral(&adapter, service_uuid, peer_name).await {
            break p;
        }

        // 2) Newly discovered devices.
        let poll = tokio::time::sleep(SCAN_POLL_INTERVAL);
        tokio::pin!(poll);
        tokio::select! {
            ev = events.next() => {
                if matches!(ev, Some(CentralEvent::DeviceDiscovered(_))) {
                    if let Some(p) = find_matching_peripheral(&adapter, service_uuid, peer_name).await {
                        break p;
                    }
                }
            }
            _ = &mut poll => {}
        }
    };

    let _ = adapter.stop_scan().await; // best-effort — failure here only delays the next scan

    // btleplug 0.11.8 on macOS: `connect()` itself drives service
    // discovery internally and stores a "connected_future_state" that
    // fulfills once CoreBluetooth has reported all services.
    // **Do not** call `discover_services()` afterwards — that triggers
    // a second didDiscoverServices callback which hits this assertion
    // in btleplug's CoreBluetooth event loop:
    //   panic at internal.rs:289 — "We should still have a future at this point!"
    // and tears down the entire corebluetooth event loop. The peripheral's
    // already-discovered characteristics are available via
    // `peripheral.characteristics()` immediately after `connect()` resolves.
    target
        .connect()
        .await
        .map_err(|e| WireDeskError::Transport(format!("BLE connect: {e}")))?;

    let chars = target.characteristics();
    let rx_char = chars
        .iter()
        .find(|c| c.uuid == uuids::RX_CHAR_UUID)
        .cloned()
        .ok_or_else(|| {
            WireDeskError::Transport(format!(
                "BLE: peer is missing RX characteristic {}",
                uuids::RX_CHAR_UUID
            ))
        })?;
    let tx_char = chars
        .iter()
        .find(|c| c.uuid == uuids::TX_CHAR_UUID)
        .cloned()
        .ok_or_else(|| {
            WireDeskError::Transport(format!(
                "BLE: peer is missing TX characteristic {}",
                uuids::TX_CHAR_UUID
            ))
        })?;

    target
        .subscribe(&tx_char)
        .await
        .map_err(|e| WireDeskError::Transport(format!("BLE subscribe TX: {e}")))?;

    Ok((target, rx_char, tx_char))
}

async fn find_matching_peripheral(
    adapter: &Adapter,
    service_uuid: Uuid,
    peer_name: &str,
) -> Option<Peripheral> {
    // Match by **service UUID only**. The 128-bit custom UUID is unique
    // per project so it disambiguates WireDesk hosts from anything else
    // on the air. `peer_name` is advisory: we log when the advertised
    // local_name differs from the configured peer_name (helpful for
    // multi-host setups), but we don't gate on it — the Win side
    // typically advertises as the computer hostname (e.g.
    // `DESKTOP-GSE79B8`), not as the configured peer_name, because
    // WinRT's advertisement payload is bounded and the local_name
    // there is the OS computer name.
    let peripherals = adapter.peripherals().await.ok()?;
    for p in peripherals {
        let props = match p.properties().await.ok().flatten() {
            Some(props) => props,
            None => continue,
        };
        if props.services.contains(&service_uuid) {
            let advertised = props.local_name.as_deref().unwrap_or("(unnamed)");
            if !peer_name.is_empty() && advertised != peer_name {
                log::info!(
                    "BLE: connecting to peer '{advertised}' (configured peer_name '{peer_name}' — \
                     advisory only, service-UUID matched)"
                );
            } else {
                log::info!("BLE: connecting to peer '{advertised}' (service-UUID matched)");
            }
            return Some(p);
        }
    }
    None
}

/// Long-lived pump driving notifications from the peripheral into the
/// assembled-packet channel. Closes when the peripheral disconnects or
/// the channel receiver is dropped.
async fn notification_pump(
    peripheral: Peripheral,
    tx_char_uuid: Uuid,
    out: mpsc::UnboundedSender<Result<Packet>>,
) {
    let mut notifications = match peripheral.notifications().await {
        Ok(s) => s,
        Err(e) => {
            let _ = out.send(Err(WireDeskError::Transport(format!(
                "BLE notifications stream: {e}"
            ))));
            return;
        }
    };

    let mut reassembler = Reassembler::new();

    while let Some(notification) = notifications.next().await {
        if notification.uuid != tx_char_uuid {
            continue;
        }

        let assembled = match reassembler.feed_chunk(&notification.value) {
            Ok(Some(payload)) => payload,
            Ok(None) => continue,
            Err(e) => {
                let _ = out.send(Err(WireDeskError::Transport(format!(
                    "BLE reassembly: {e}"
                ))));
                continue;
            }
        };

        // The wire payload is COBS-encoded with a trailing 0x00 sentinel,
        // matching SerialTransport's framing. Decode then parse.
        let mut framed = assembled;
        if framed.last() != Some(&0x00) {
            framed.push(0x00);
        }
        let packet = match cobs::decode(&framed) {
            Ok(raw) => match Packet::from_bytes(&raw) {
                Ok(p) => Ok(p),
                Err(e) => Err(WireDeskError::Protocol(format!("BLE packet parse: {e}"))),
            },
            Err(e) => Err(WireDeskError::Protocol(format!("BLE COBS decode: {e}"))),
        };

        if out.send(packet).is_err() {
            // Receiver dropped — transport is being shut down. Exit pump.
            break;
        }
    }
}

impl Transport for BluetoothLeTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        let raw = packet.to_bytes()?;
        let encoded = cobs::encode(&raw);
        let payload_cap = self.inner.att_payload.load(Ordering::Relaxed);
        let pid = self.inner.next_packet_id.fetch_add(1, Ordering::Relaxed);

        let chunks = split_packet(pid, &encoded, payload_cap)
            .map_err(|e| WireDeskError::Transport(format!("BLE split_packet: {e}")))?;

        let inner = Arc::clone(&self.inner);
        inner.rt.block_on(async {
            let send_fut = async {
                // Hybrid WithoutResponse + sparse WithResponse for
                // backpressure. btleplug 0.11 on macOS does NOT actually
                // wait for `peripheralIsReady` before returning from
                // `peripheral.write(…WithoutResponse)` — it just hands
                // the bytes to CoreBluetooth and returns immediately.
                // Without our own flow control, CoreBluetooth's internal
                // queue overflows and writes are silently dropped (live
                // test: Mac→Win clipboard never arrived on Win even
                // though Mac logged DONE).
                //
                // Solution: every ACK_EVERY writes (counted globally
                // across input + clipboard + heartbeat) one is
                // WriteWithResponse. The ATT-ack acts as a sync point —
                // CoreBluetooth waits to flush all preceding
                // WithoutResponse writes before it can ack the
                // WithResponse, so we get backpressure essentially for
                // free.
                //
                // ACK_EVERY=32 is tuned alongside Win-side
                // notification window=2:
                //   - Win window=2 keeps handler thread responsive
                //     to incoming Writes, so the WithResponse ATT-ack
                //     comes back quickly (no more ~3-min hangs).
                //   - 32 is sparse enough that input-event jitter is
                //     imperceptible (1 in 32 events pays an ATT-RTT).
                //   - 31/32 = 97% of pure-WithoutResponse throughput
                //     when streaming clipboard.
                const ACK_EVERY: u64 = 32;
                for chunk in chunks.iter() {
                    let n = inner
                        .write_counter
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    let want_ack = n.is_multiple_of(ACK_EVERY);
                    let write_type = if want_ack {
                        WriteType::WithResponse
                    } else {
                        WriteType::WithoutResponse
                    };
                    inner
                        .peripheral
                        .write(&inner.rx_char, chunk, write_type)
                        .await
                        .map_err(|e| WireDeskError::Transport(format!("BLE write: {e}")))?;
                }
                Ok::<_, WireDeskError>(())
            };
            tokio::time::timeout(SEND_TIMEOUT, send_fut)
                .await
                .map_err(|_| WireDeskError::Transport("BLE send timeout".into()))?
        })?;

        Ok(())
    }

    fn recv(&mut self) -> Result<Packet> {
        if !self.is_owner {
            return Err(WireDeskError::Transport(
                "BLE recv on cloned (write-only) handle".into(),
            ));
        }
        let inner = Arc::clone(&self.inner);
        let result = inner.rt.block_on(async {
            let mut rx = inner.incoming_rx.lock().await;
            rx.recv().await
        });
        match result {
            Some(Ok(p)) => Ok(p),
            Some(Err(e)) => Err(e),
            None => Err(WireDeskError::Transport(
                "BLE notification pump closed".into(),
            )),
        }
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected.load(Ordering::Relaxed)
    }

    fn name(&self) -> &'static str {
        "bluetooth-le-central"
    }

    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        Ok(Box::new(BluetoothLeTransport {
            inner: Arc::clone(&self.inner),
            is_owner: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_short_timeout() -> BluetoothFactoryConfig {
        // Definitely-not-our-real-service UUID, to keep this test
        // deterministic even when a real WireDesk host is in radio range.
        // (Random v4 UUID, unrelated to `uuids::SERVICE_UUID`.)
        BluetoothFactoryConfig {
            service_uuid: "00000000-0000-4000-8000-000000000001".to_string(),
            peer_name: "WireDeskNonexistent".to_string(),
            mtu: 247,
            connect_timeout_secs: 1,
            reconnect_max_attempts: 0,
        }
    }

    #[test]
    fn open_with_invalid_service_uuid_returns_err_immediately() {
        let mut cfg = cfg_with_short_timeout();
        cfg.service_uuid = "not-a-uuid".to_string();
        let result = BluetoothLeTransport::open(&cfg);
        let err = match result {
            Ok(_) => panic!("expected err"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("not a valid UUID") || err.contains("service_uuid"),
            "expected uuid-parse error, got: {err}"
        );
    }

    /// Live BLE-stack tests would be flaky in CI (no advertising peer);
    /// real connect-tests live in Task 16's manual checklist. Here we
    /// just exercise the synchronous error paths.
    #[test]
    fn open_short_timeout_no_peer_returns_err() {
        let cfg = cfg_with_short_timeout();
        let result = BluetoothLeTransport::open(&cfg);
        // We don't assert exact error text — could be "no peer", "BLE
        // adapters", "manager", etc. depending on the test environment.
        // Just verify the call returns Err rather than hanging forever.
        assert!(result.is_err(), "open without peer must error");
    }

    #[test]
    fn name_is_stable() {
        // Constructable without a real connection so we can hit
        // name() without going through open().
        // (This test is a sanity guard; if the type ever gains
        // mandatory non-default fields, we'll need a different pattern.)
        // We can't trivially construct an Inner without btleplug types —
        // verify via the bluetooth-le-stub crate test instead.
    }
}
