//! Windows BLE Peripheral implementation backed by `windows-rs` WinRT
//! GATT bindings.
//!
//! Behaviour mirrors the Mac side, role-reversed:
//! - Win11 publishes a custom GATT service with `SERVICE_UUID` and two
//!   characteristics: `TX_CHAR_UUID` (Notify, Win→Mac) and `RX_CHAR_UUID`
//!   (Write/WriteWithResponse, Mac→Win).
//! - `send` notifies on TX. The Mac peer has subscribed via Indicate-style
//!   semantics (BLE Notify), so each `NotifyValueAsync` push reaches all
//!   subscribers fire-and-forget.
//! - `recv` blocks on a channel that the WriteRequested event handler
//!   feeds. Each incoming write is answered with `RespondAsync` so the
//!   Mac's WriteWithResponse semantics complete.
//!
//! Sync send/recv go through the embedded tokio runtime via `block_on` —
//! same approach as `mac.rs`. `try_clone` returns a write-only handle
//! (cloned handle's `recv` returns Err); see Decision 4 in the plan.
//!
//! Note: this module compiles only on Windows (`cfg(target_os =
//! "windows")`). The Mac dev box exercises the path only via the stub
//! during `cargo check`. Live verification is Task 15's manual checklist
//! on the real Win11 host.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex as AsyncMutex};
use uuid::Uuid as UuidStr;

use windows::core::GUID;
use windows::Devices::Bluetooth::GenericAttributeProfile::{
    GattCharacteristicProperties, GattLocalCharacteristic, GattLocalCharacteristicParameters,
    GattProtectionLevel, GattServiceProvider, GattServiceProviderAdvertisingParameters,
    GattWriteOption,
};
use windows::Foundation::TypedEventHandler;
use windows::Storage::Streams::{DataReader, DataWriter};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::cobs;
use wiredesk_protocol::packet::Packet;

use super::fragment::{max_chunk_payload, split_packet, Reassembler, DEFAULT_ATT_MTU};
use super::runtime::EmbeddedRuntime;
use super::uuids;
use super::BluetoothFactoryConfig;
use crate::transport::Transport;

const SEND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub struct BluetoothLeTransport {
    inner: Arc<Inner>,
    is_owner: bool,
}

struct Inner {
    rt: EmbeddedRuntime,
    tx_char: GattLocalCharacteristic,
    incoming_rx: AsyncMutex<mpsc::UnboundedReceiver<Result<Packet>>>,
    att_payload: AtomicUsize,
    is_connected: AtomicBool, // tracks subscribed-clients > 0
    next_packet_id: AtomicU16,
    /// Service provider lives for the lifetime of the transport so the
    /// advertisement keeps running. Stopping it explicitly in Drop.
    provider: GattServiceProvider,
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

        let service_uuid = UuidStr::parse_str(&cfg.service_uuid).map_err(|e| {
            WireDeskError::Transport(format!(
                "BLE config: service_uuid '{}' not a valid UUID: {e}",
                cfg.service_uuid
            ))
        })?;

        // mpsc the WriteRequested handler will push assembled Packets into.
        let (tx, rx) = mpsc::unbounded_channel::<Result<Packet>>();
        let is_connected = Arc::new(AtomicBool::new(false));

        // build_service is sync (uses IAsyncOperation::get() for blocking
        // wait) — windows-rs 0.58 IAsyncOperation doesn't implement
        // std::future::Future natively. .get() is the canonical sync-wait
        // pattern; we don't pay an event-loop overhead during init since
        // we're already on a single thread here.
        let (provider, tx_char, _rx_char) =
            build_service(service_uuid, tx, Arc::clone(&is_connected))?;

        let inner = Arc::new(Inner {
            rt,
            tx_char,
            incoming_rx: AsyncMutex::new(rx),
            att_payload: AtomicUsize::new(max_chunk_payload(DEFAULT_ATT_MTU)),
            is_connected: AtomicBool::new(false),
            next_packet_id: AtomicU16::new(0),
            provider,
        });

        // Mirror the shared is_connected into the Arc<Inner> at startup —
        // subsequent updates come from the SubscribedClientsChanged
        // handler we wired in build_service.
        inner
            .is_connected
            .store(is_connected.load(Ordering::Relaxed), Ordering::Relaxed);

        Ok(Self {
            inner,
            is_owner: true,
        })
    }
}

impl Drop for BluetoothLeTransport {
    fn drop(&mut self) {
        if !self.is_owner {
            return;
        }
        if Arc::strong_count(&self.inner) > 1 {
            return;
        }
        // Best-effort stop. Errors here only matter if we're trying to
        // re-open the same service immediately afterwards, which our flow
        // doesn't do.
        let _ = self.inner.provider.StopAdvertising();
        self.inner.is_connected.store(false, Ordering::Relaxed);
    }
}

fn build_service(
    service_uuid: UuidStr,
    tx: mpsc::UnboundedSender<Result<Packet>>,
    is_connected_flag: Arc<AtomicBool>,
) -> Result<(GattServiceProvider, GattLocalCharacteristic, GattLocalCharacteristic)> {
    let svc_guid = uuid_to_guid(service_uuid);
    let result = GattServiceProvider::CreateAsync(svc_guid)
        .map_err(|e| WireDeskError::Transport(format!("BLE CreateAsync: {e}")))?
        .get()
        .map_err(|e| WireDeskError::Transport(format!("BLE CreateAsync get: {e}")))?;
    let provider = result
        .ServiceProvider()
        .map_err(|e| WireDeskError::Transport(format!("BLE ServiceProvider(): {e}")))?;
    let service = provider
        .Service()
        .map_err(|e| WireDeskError::Transport(format!("BLE Service(): {e}")))?;

    // TX: Notify (Win→Mac).
    let tx_params = GattLocalCharacteristicParameters::new()
        .map_err(|e| WireDeskError::Transport(format!("BLE TX params new: {e}")))?;
    tx_params
        .SetCharacteristicProperties(GattCharacteristicProperties::Notify)
        .map_err(|e| WireDeskError::Transport(format!("BLE TX SetProps: {e}")))?;
    tx_params
        .SetReadProtectionLevel(GattProtectionLevel::Plain)
        .ok();
    tx_params
        .SetWriteProtectionLevel(GattProtectionLevel::Plain)
        .ok();
    let tx_char_result = service
        .CreateCharacteristicAsync(uuid_to_guid(uuids::TX_CHAR_UUID), &tx_params)
        .map_err(|e| WireDeskError::Transport(format!("BLE TX CreateChar: {e}")))?
        .get()
        .map_err(|e| WireDeskError::Transport(format!("BLE TX CreateChar get: {e}")))?;
    let tx_char = tx_char_result
        .Characteristic()
        .map_err(|e| WireDeskError::Transport(format!("BLE TX Characteristic(): {e}")))?;

    // RX: Write | WriteWithoutResponse. Setting both flags lets the
    // client (Mac btleplug) pick which ATT op-code to use. We default
    // Mac→Win sends to WriteWithoutResponse for ~10× throughput; the
    // handler below still respects WithResponse semantics if a future
    // client / firmware variant uses it. Drops are caught by the
    // application-layer heartbeat (PR #20), so per-chunk reliability
    // is unnecessary at the BLE level.
    let rx_params = GattLocalCharacteristicParameters::new()
        .map_err(|e| WireDeskError::Transport(format!("BLE RX params new: {e}")))?;
    rx_params
        .SetCharacteristicProperties(
            GattCharacteristicProperties::Write
                | GattCharacteristicProperties::WriteWithoutResponse,
        )
        .map_err(|e| WireDeskError::Transport(format!("BLE RX SetProps: {e}")))?;
    let rx_char_result = service
        .CreateCharacteristicAsync(uuid_to_guid(uuids::RX_CHAR_UUID), &rx_params)
        .map_err(|e| WireDeskError::Transport(format!("BLE RX CreateChar: {e}")))?
        .get()
        .map_err(|e| WireDeskError::Transport(format!("BLE RX CreateChar get: {e}")))?;
    let rx_char = rx_char_result
        .Characteristic()
        .map_err(|e| WireDeskError::Transport(format!("BLE RX Characteristic(): {e}")))?;

    // SubscribedClientsChanged on TX → flip is_connected when at least
    // one Mac is subscribed to notifications. Used by Transport::is_connected.
    //
    // windows-rs 0.58 TypedEventHandler closures take `&Option<TSender>` /
    // `&Option<TResult>` parameters (not `Ref<T>` like older versions), so
    // we destructure via Option::as_ref().
    {
        let flag = Arc::clone(&is_connected_flag);
        let handler = TypedEventHandler::<GattLocalCharacteristic, _>::new(
            move |sender: &Option<GattLocalCharacteristic>, _args: &Option<windows::core::IInspectable>| {
                if let Some(s) = sender.as_ref() {
                    let count = s
                        .SubscribedClients()
                        .ok()
                        .and_then(|v| v.Size().ok())
                        .unwrap_or(0);
                    flag.store(count > 0, Ordering::Relaxed);
                }
                Ok(())
            },
        );
        tx_char
            .SubscribedClientsChanged(&handler)
            .map_err(|e| WireDeskError::Transport(format!("BLE SubscribedClientsChanged: {e}")))?;
    }

    // WriteRequested on RX → drain bytes, feed Reassembler, push Packets
    // into the channel, ack the request. The reassembler is owned by the
    // event handler closure (Mutex so it's Send across the FnMut boundary).
    {
        let reassembler = Arc::new(std::sync::Mutex::new(Reassembler::new()));
        let tx = tx.clone();
        let handler = TypedEventHandler::<
            GattLocalCharacteristic,
            windows::Devices::Bluetooth::GenericAttributeProfile::GattWriteRequestedEventArgs,
        >::new(
            move |_sender: &Option<GattLocalCharacteristic>,
                  args: &Option<
                windows::Devices::Bluetooth::GenericAttributeProfile::GattWriteRequestedEventArgs,
            >| {
                let args = match args.as_ref() {
                    Some(a) => a,
                    None => return Ok(()),
                };
                let deferral = args
                    .GetDeferral()
                    .map_err(|e| windows::core::Error::new(e.code(), format!("Deferral: {e}")))?;
                let request = args.GetRequestAsync()?.get()?;

                let value_buf = request
                    .Value()
                    .map_err(|e| windows::core::Error::new(e.code(), format!("Value: {e}")))?;
                let reader = DataReader::FromBuffer(&value_buf)
                    .map_err(|e| windows::core::Error::new(e.code(), format!("FromBuffer: {e}")))?;
                let len = reader
                    .UnconsumedBufferLength()
                    .map_err(|e| windows::core::Error::new(e.code(), format!("Len: {e}")))?
                    as usize;
                let mut bytes = vec![0u8; len];
                reader.ReadBytes(&mut bytes).ok();

                // Always respond — WriteWithResponse callers wait for the
                // ack regardless of whether assembly succeeded.
                if request.Option().unwrap_or(GattWriteOption::WriteWithResponse)
                    == GattWriteOption::WriteWithResponse
                {
                    let _ = request.Respond();
                }
                let _ = deferral.Complete();

                let mut r = match reassembler.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                let assembled = match r.feed_chunk(&bytes) {
                    Ok(Some(payload)) => payload,
                    Ok(None) => return Ok(()),
                    Err(e) => {
                        let _ = tx.send(Err(WireDeskError::Transport(format!(
                            "BLE reassembly: {e}"
                        ))));
                        return Ok(());
                    }
                };

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
                let _ = tx.send(packet);
                Ok(())
            },
        );
        rx_char
            .WriteRequested(&handler)
            .map_err(|e| WireDeskError::Transport(format!("BLE WriteRequested: {e}")))?;
    }

    // Advertise. IsConnectable + IsDiscoverable so Mac scanners see us
    // and can connect. The peer name lives in the system Bluetooth
    // settings, not in the advertisement payload — we rely on the Mac
    // matching by service-UUID first and confirming the local-name in
    // the scan response.
    let adv = GattServiceProviderAdvertisingParameters::new()
        .map_err(|e| WireDeskError::Transport(format!("BLE adv params new: {e}")))?;
    adv.SetIsConnectable(true)
        .map_err(|e| WireDeskError::Transport(format!("BLE adv SetIsConnectable: {e}")))?;
    adv.SetIsDiscoverable(true)
        .map_err(|e| WireDeskError::Transport(format!("BLE adv SetIsDiscoverable: {e}")))?;
    provider
        .StartAdvertisingWithParameters(&adv)
        .map_err(|e| WireDeskError::Transport(format!("BLE StartAdvertising: {e}")))?;

    Ok((provider, tx_char, rx_char))
}

fn uuid_to_guid(u: UuidStr) -> GUID {
    GUID::from_u128(u.as_u128())
}

fn write_to_buffer(bytes: &[u8]) -> Result<windows::Storage::Streams::IBuffer> {
    let writer = DataWriter::new()
        .map_err(|e| WireDeskError::Transport(format!("BLE DataWriter: {e}")))?;
    writer
        .WriteBytes(bytes)
        .map_err(|e| WireDeskError::Transport(format!("BLE DataWriter::WriteBytes: {e}")))?;
    writer
        .DetachBuffer()
        .map_err(|e| WireDeskError::Transport(format!("BLE DataWriter::DetachBuffer: {e}")))
}

impl Transport for BluetoothLeTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        let raw = packet.to_bytes()?;
        let encoded = cobs::encode(&raw);
        let payload_cap = self.inner.att_payload.load(Ordering::Relaxed);
        let pid = self.inner.next_packet_id.fetch_add(1, Ordering::Relaxed);

        let chunks = split_packet(pid, &encoded, payload_cap)
            .map_err(|e| WireDeskError::Transport(format!("BLE split_packet: {e}")))?;

        // Pipeline notifications with a sliding window so we don't pay
        // a full BLE connection-event roundtrip per chunk.
        //
        // NotifyValueAsync returns IAsyncOperation that completes when
        // the notification has been *delivered* (one BLE connection
        // event, typically 15-30 ms on macOS). Calling .get() per
        // chunk serialised the whole transfer to ~30 ms × N chunks =
        // ~5 KB/s for a 540 KB image (97 s wall-clock observed live).
        //
        // Letting up to WINDOW_SIZE notifications fly before draining
        // the head of the queue keeps the WinRT stack busy across
        // connection events without unbounded queueing (which would
        // risk silent drops on internal buffer overflow). Window=8
        // is conservative — enough to saturate the link, small
        // enough that backpressure surfaces quickly on a stalled peer.
        const WINDOW_SIZE: usize = 8;
        let inner = &self.inner;
        let start = std::time::Instant::now();
        let mut in_flight: std::collections::VecDeque<_> =
            std::collections::VecDeque::with_capacity(WINDOW_SIZE);
        for chunk in &chunks {
            if start.elapsed() > SEND_TIMEOUT {
                return Err(WireDeskError::Transport("BLE send timeout".into()));
            }
            let buf = write_to_buffer(chunk)?;
            let op = inner
                .tx_char
                .NotifyValueAsync(&buf)
                .map_err(|e| WireDeskError::Transport(format!("BLE NotifyValueAsync: {e}")))?;
            in_flight.push_back(op);
            if in_flight.len() >= WINDOW_SIZE {
                in_flight
                    .pop_front()
                    .expect("just-pushed")
                    .get()
                    .map_err(|e| {
                        WireDeskError::Transport(format!("BLE NotifyValueAsync get: {e}"))
                    })?;
            }
        }
        // Drain remaining in-flight ops so we return only after every
        // notification has been delivered to the BLE link layer.
        while let Some(op) = in_flight.pop_front() {
            if start.elapsed() > SEND_TIMEOUT {
                return Err(WireDeskError::Transport("BLE send drain timeout".into()));
            }
            op.get().map_err(|e| {
                WireDeskError::Transport(format!("BLE NotifyValueAsync drain get: {e}"))
            })?;
        }

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
                "BLE WriteRequested channel closed".into(),
            )),
        }
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected.load(Ordering::Relaxed)
    }

    fn name(&self) -> &'static str {
        "bluetooth-le-peripheral"
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

    fn cfg() -> BluetoothFactoryConfig {
        BluetoothFactoryConfig {
            service_uuid: uuids::SERVICE_UUID.to_string(),
            peer_name: "WireDeskHostTest".to_string(),
            mtu: 247,
            connect_timeout_secs: 1,
            reconnect_max_attempts: 0,
        }
    }

    #[test]
    fn open_with_invalid_service_uuid_returns_err_immediately() {
        let mut c = cfg();
        c.service_uuid = "not-a-uuid".to_string();
        let result = BluetoothLeTransport::open(&c);
        let err = match result {
            Ok(_) => panic!("expected err"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("not a valid UUID") || err.contains("service_uuid"),
            "expected uuid-parse error, got: {err}"
        );
    }

    /// Live BLE-stack tests would require a real Win11 device with a
    /// working BLE adapter and elevated rights to advertise — covered by
    /// Task 15's manual checklist. Here we only exercise sync error paths.
    #[test]
    fn name_is_stable_compile_check() {
        // Compile-time check that the type still has the expected method;
        // we don't construct an Inner without windows-rs runtime.
        fn assert_has_name<T: Transport>(_: &T) {}
        let _ = assert_has_name::<BluetoothLeTransport>;
    }
}
