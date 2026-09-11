//! macOS RFCOMM client on IOBluetooth.
//!
//! IOBluetooth is a main-run-loop framework: SDP-query and channel-open
//! completions and every incoming data callback arrive on the **main
//! thread's** run loop, no matter which thread issued the call. Verified
//! 2026-09-11 (Swift probe): `openRFCOMMChannelAsync` from a worker thread
//! succeeds and `writeSync` from a worker thread is fine, while the sync
//! open from a worker thread fails outright. The transport therefore:
//! - issues async open / SDP from whatever thread calls `open()` and waits
//!   on a condvar the delegate signals from the main thread;
//! - if `open()` itself runs on the main thread (no NSApp loop yet), pumps
//!   `CFRunLoopRunInMode` while waiting instead of blocking;
//! - collects delegate data into a queue that `recv()` drains.
//!
//! The eframe/winit app runs `NSApplication`'s loop on the main thread, so
//! callbacks flow while the UI is alive.

// Delegate methods keep their Objective-C selector spelling (objc2 convention).
#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::{c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol};
use objc2::{define_class, msg_send, AnyThread, DefinedClass, Message};
use objc2_core_foundation::{kCFRunLoopDefaultMode, CFRunLoop};
use objc2_foundation::{NSObject, NSString, NSThread};
use objc2_io_bluetooth::{
    IOBluetoothDevice, IOBluetoothDeviceAsyncCallbacks, IOBluetoothRFCOMMChannel,
    IOBluetoothRFCOMMChannelDelegate, IOBluetoothSDPUUID,
};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::packet::Packet;

use super::common::{format_bt_address, IdleKeepalive, RecvState, RECV_POLL, WRITE_TIMEOUT};
use super::{RfcommFactoryConfig, RfcommRole};
use crate::framing::encode_frame;
use crate::transport::Transport;

/// `kIOReturnSuccess`.
const IO_RETURN_SUCCESS: c_int = 0;

/// Major device class "Computer" — tried first during auto-discovery.
const DEVICE_CLASS_MAJOR_COMPUTER: u32 = 0x01;

/// Longest one SDP query may take before we move to the next candidate;
/// the actual budget is the remaining connect time split evenly over the
/// candidates still untried, clamped to [`SDP_QUERY_MIN`, this].
const SDP_QUERY_BUDGET: Duration = Duration::from_secs(6);
const SDP_QUERY_MIN: Duration = Duration::from_secs(2);

/// Auto-discovery attempt counter. Each `open()` starts the paired-device
/// walk one position further along, so a dead device at the head of the
/// list cannot eat the whole budget on every reconnect and starve a live
/// host further down.
static DISCOVERY_ROTATION: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Cross-thread wrapper for the three IOBluetooth objects the transport
/// holds. `objc2` marks framework classes `!Send + !Sync` wholesale; the
/// calls we make off the main thread are limited to `writeSync`,
/// `closeChannel`, `getMTU` and retain/release, which IOBluetooth handles
/// under its own locking (the framework itself delivers every callback on
/// the main run loop and expects clients to call in from elsewhere —
/// verified live 2026-09-11 with a worker-thread `writeSync` loop at
/// 10 ms RTT). The impls are per type, not blanket, so nothing else can
/// ride on this exemption.
struct SendCell<T>(T);
// SAFETY: see the type doc.
unsafe impl Send for SendCell<Retained<IOBluetoothRFCOMMChannel>> {}
unsafe impl Sync for SendCell<Retained<IOBluetoothRFCOMMChannel>> {}
// SAFETY: only retained/released off the main thread; every call goes
// through `try_connect`, which runs on the opening thread.
unsafe impl Send for SendCell<Retained<IOBluetoothDevice>> {}
unsafe impl Sync for SendCell<Retained<IOBluetoothDevice>> {}
// SAFETY: the delegate's only state is `Arc<Shared>` (Send + Sync); its
// methods run on the main loop, we merely keep it alive from elsewhere.
unsafe impl Send for SendCell<Retained<Delegate>> {}
unsafe impl Sync for SendCell<Retained<Delegate>> {}
// SAFETY: parked only to be stopped later, and the stop itself is hopped
// onto the main thread; nothing touches the manager from anywhere else.
unsafe impl Send for SendCell<objc2_05::rc::Retained<objc2_core_bluetooth::CBCentralManager>> {}
// SAFETY: a device list handed from the main thread to the opener; see
// `on_main_thread`.
unsafe impl Send for SendCell<Vec<Retained<IOBluetoothDevice>>> {}

// libdispatch — enough to run one closure on the main queue.
#[repr(C)]
struct DispatchQueue {
    _private: [u8; 0],
}
extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_async_f(
        queue: *const DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// Make sure this process may use Bluetooth before the first IOBluetooth
/// call. IOBluetooth is bridged over CoreBluetooth, whose first XPC to
/// `bluetoothd` blocks until the Bluetooth privacy (TCC) decision exists —
/// and IOBluetooth's own bootstrap never triggers the permission prompt, it
/// just hangs its caller (seen live 2026-09-11 in the bundled app; the same
/// binary launched from a terminal, which already holds the grant, ran
/// fine). Creating a `CBCentralManager` is the documented way to ask; the
/// class-level `authorization` tells us the answer without blocking.
/// Set once the process has created its `CBCentralManager`, i.e. asked the
/// system for the Bluetooth privacy grant. The manager is deliberately
/// leaked rather than dropped: releasing it while the prompt is still on
/// screen cancels the request, and one central manager per process is what
/// CoreBluetooth expects anyway.
static PERMISSION_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Ask macOS for the Bluetooth privacy grant, returning immediately.
///
/// Call this from the main thread once the app is up (eframe's creator
/// callback). A GUI app that only touches Bluetooth from a worker thread
/// before `NSApplicationMain` has started never gets the prompt: nothing is
/// shown, the grant stays `NotDetermined`, and the first IOBluetooth call
/// then blocks in `IOBluetoothCoreBluetoothCoordinator` forever. Live
/// 2026-09-11: the bundled app hung there while the TCC database held no
/// row for the bundle at all, neither allow nor deny.
///
/// Safe to call repeatedly and on any transport — it is a no-op once the
/// user has answered.
/// How long the permission scan is allowed to run. Long enough for the
/// prompt to appear and be answered, short enough that a client which
/// fell back to serial is not left scanning the room all day.
const PERMISSION_SCAN: Duration = Duration::from_secs(15);

pub fn request_bluetooth_permission() {
    use objc2_05::ClassType;
    use objc2_core_bluetooth::{CBCentralManager, CBManager, CBManagerAuthorization};

    // SAFETY: class getter, no arguments.
    let status = unsafe { CBManager::authorization_class() };
    if status != CBManagerAuthorization::NotDetermined {
        return;
    }
    if PERMISSION_REQUESTED.swap(true, Ordering::AcqRel) {
        return;
    }
    log::info!("RFCOMM: requesting Bluetooth permission (system prompt)");
    // SAFETY: plain init. With no delegate and no queue the manager binds
    // to the main queue; creating it is what puts the TCC request in front
    // of the user.
    let manager = unsafe { CBCentralManager::init(CBCentralManager::alloc()) };
    // Asking to scan forces CoreBluetooth to actually reach for the radio.
    // A manager that is merely constructed can stay dormant, and a dormant
    // manager never raises the prompt. Scanning before the stack reports
    // `poweredOn` only logs an API-misuse note, which is the price of not
    // carrying a delegate class just for this.
    // SAFETY: both arguments are optional and nil is documented as "every
    // service, no options".
    unsafe { manager.scanForPeripheralsWithServices_options(None, None) };
    // An unfiltered scan keeps the radio listening for as long as it runs,
    // and nothing here would ever stop it - the manager outlives the
    // prompt on purpose (see `PERMISSION_REQUESTED`). Stop it from a timer
    // instead: by then the user has either answered the prompt or it never
    // appeared, and in both cases the scan has done its job. The stop has
    // to happen on the main thread, where the manager is bound.
    let parked = SendCell(manager);
    std::thread::Builder::new()
        .name("wiredesk-bt-permission-scan".into())
        .spawn(move || {
            std::thread::sleep(PERMISSION_SCAN);
            let r = on_main_thread(
                move || {
                    // SAFETY: plain call on a live manager, on its own queue.
                    unsafe { parked.0.stopScan() };
                    std::mem::forget(parked);
                },
                Duration::from_secs(5),
            );
            if let Err(e) = r {
                log::debug!("RFCOMM: could not stop the permission scan: {e}");
            }
        })
        .map_or_else(
            |e| log::debug!("RFCOMM: no thread to stop the permission scan: {e}"),
            |_| (),
        );
    // Note: on this machine the prompt never appears for the bundled app
    // regardless (live 2026-09-11 - no window, and no row in `TCC.db`
    // either; driving btleplug's own central manager did not change it).
    // The suspected cause is the self-signed identity; see
    // `docs/known-limitations.md`. Keeping the request anyway: it is the
    // documented way to ask, and it turns a hang into a fast, logged
    // failure that falls back to serial.
}

fn ensure_bluetooth_authorized() -> Result<()> {
    use objc2_core_bluetooth::{CBManager, CBManagerAuthorization};

    // SAFETY: class getter, no arguments.
    let status = unsafe { CBManager::authorization_class() };
    if status == CBManagerAuthorization::AllowedAlways {
        return Ok(());
    }
    if status == CBManagerAuthorization::NotDetermined {
        // The prompt is the user's to answer, and it can sit on screen for
        // minutes. Ask (once) and fail immediately rather than blocking the
        // open: the link supervisor falls back to serial and retries with
        // backoff, so the Bluetooth link comes up on its own the moment
        // permission is granted. Waiting here instead starved the fallback
        // - the reconnect loop spent its whole liveness budget inside this
        // call and reopened the port over and over (seen live 2026-09-11).
        request_bluetooth_permission();
        return Err(WireDeskError::Transport(
            "RFCOMM: waiting for the Bluetooth permission prompt - allow WireDesk under \
             System Settings > Privacy & Security > Bluetooth"
                .into(),
        ));
    }
    Err(WireDeskError::Transport(format!(
        "RFCOMM: Bluetooth access is {} for WireDesk - enable it in System Settings > \
         Privacy & Security > Bluetooth",
        if status == CBManagerAuthorization::Restricted {
            "restricted"
        } else {
            "denied"
        }
    )))
}

/// Run `f` on the main thread and wait for its result. The first touch of
/// IOBluetooth (`+[IOBluetoothDevice deviceWithAddressString:]`) initialises
/// `IOBluetoothCoreBluetoothCoordinator`, which blocks the calling thread
/// on a semaphore until a CoreBluetooth state callback arrives — inside an
/// AppKit app that callback only ever arrives if the *initialising* call
/// came from the main queue. Seen live 2026-09-11: the link supervisor
/// thread hung forever in that init while the NSApp loop was running fine.
/// If we already are on the main thread, run inline. `timeout` bounds the
/// wait so a stalled main thread surfaces as an error, not a hang.
fn on_main_thread<T, F>(f: F, timeout: Duration) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    if NSThread::isMainThread_class() {
        return Ok(f());
    }
    let (tx, rx) = std::sync::mpsc::channel::<T>();
    let job: Box<dyn FnOnce() + Send> = Box::new(move || {
        let _ = tx.send(f());
    });
    extern "C" fn trampoline(ctx: *mut c_void) {
        // SAFETY: `ctx` is the Box<Box<dyn FnOnce>> leaked below, run once.
        let job: Box<Box<dyn FnOnce() + Send>> = unsafe { Box::from_raw(ctx.cast()) };
        job();
    }
    let ctx = Box::into_raw(Box::new(job)).cast::<c_void>();
    // SAFETY: the main queue is a process-wide static; `ctx` is owned by
    // the trampoline from here on.
    unsafe { dispatch_async_f(&_dispatch_main_q, ctx, trampoline) };
    rx.recv_timeout(timeout).map_err(|_| {
        WireDeskError::Transport(
            "RFCOMM: main thread did not service the Bluetooth lookup in time".into(),
        )
    })
}

/// State the delegate writes and the transport reads.
struct Shared {
    /// Incoming stream bytes, appended by `rfcommChannelData`.
    rx: Mutex<VecDeque<u8>>,
    rx_cv: Condvar,
    /// Completion codes of the two async operations we wait on.
    events: Mutex<Events>,
    events_cv: Condvar,
    /// Set by `rfcommChannelClosed` (peer hung up, radio off, sleep).
    closed: AtomicBool,
    /// Async operations issued against this delegate whose completion has
    /// not arrived yet. IOBluetooth holds the delegate *unretained*, so a
    /// delegate must outlive every pending completion — see [`park`].
    pending: std::sync::atomic::AtomicUsize,
}

/// Delegates (plus their half-open channels) whose async operation we
/// gave up waiting for. IOBluetooth still holds them unretained and will
/// deliver the late completion on the main loop; releasing them would
/// hand it a dangling object. They stay here until their `pending` count
/// drops to zero, and the list is pruned on every insert.
static PARKED: Mutex<Vec<Parked>> = Mutex::new(Vec::new());

struct Parked {
    shared: Arc<Shared>,
    _delegate: SendCell<Retained<Delegate>>,
    _channel: Option<SendCell<Retained<IOBluetoothRFCOMMChannel>>>,
}

fn park(
    shared: Arc<Shared>,
    delegate: Retained<Delegate>,
    channel: Option<Retained<IOBluetoothRFCOMMChannel>>,
) {
    if shared.pending.load(Ordering::Acquire) == 0 && channel.is_none() {
        return;
    }
    let mut list = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    list.retain(|p| p.shared.pending.load(Ordering::Acquire) > 0);
    list.push(Parked {
        shared,
        _delegate: SendCell(delegate),
        _channel: channel.map(SendCell),
    });
}

#[derive(Default)]
struct Events {
    sdp_status: Option<c_int>,
    open_status: Option<c_int>,
}

impl Shared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            rx: Mutex::new(VecDeque::with_capacity(16 * 1024)),
            rx_cv: Condvar::new(),
            events: Mutex::new(Events::default()),
            events_cv: Condvar::new(),
            closed: AtomicBool::new(false),
            pending: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    fn signal_events(&self) {
        self.events_cv.notify_all();
    }
}

struct Ivars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; Delegate has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "WireDeskRfcommDelegate"]
    #[ivars = Ivars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl IOBluetoothDeviceAsyncCallbacks for Delegate {
        #[unsafe(method(remoteNameRequestComplete:status:))]
        unsafe fn remoteNameRequestComplete_status(
            &self,
            _device: Option<&IOBluetoothDevice>,
            _status: c_int,
        ) {
        }

        #[unsafe(method(connectionComplete:status:))]
        unsafe fn connectionComplete_status(
            &self,
            _device: Option<&IOBluetoothDevice>,
            _status: c_int,
        ) {
        }

        #[unsafe(method(sdpQueryComplete:status:))]
        unsafe fn sdpQueryComplete_status(
            &self,
            _device: Option<&IOBluetoothDevice>,
            status: c_int,
        ) {
            // Keep both the delegate and its state alive for the whole
            // callback: once `pending` hits zero, `park()` on another
            // thread may free the parked entry that owned them.
            let _keep = self.retain();
            let shared = Arc::clone(&self.ivars().shared);
            if let Ok(mut ev) = shared.events.lock() {
                ev.sdp_status = Some(status);
            }
            shared.signal_events();
            shared.pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    unsafe impl IOBluetoothRFCOMMChannelDelegate for Delegate {
        #[unsafe(method(rfcommChannelData:data:length:))]
        unsafe fn rfcommChannelData_data_length(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
            data_pointer: *mut c_void,
            data_length: usize,
        ) {
            if data_pointer.is_null() || data_length == 0 {
                return;
            }
            // SAFETY: IOBluetooth guarantees `data_length` readable bytes at
            // `data_pointer` for the duration of the callback.
            let bytes =
                unsafe { std::slice::from_raw_parts(data_pointer as *const u8, data_length) };
            let shared = &self.ivars().shared;
            if let Ok(mut q) = shared.rx.lock() {
                q.extend(bytes);
            }
            shared.rx_cv.notify_all();
        }

        #[unsafe(method(rfcommChannelOpenComplete:status:))]
        unsafe fn rfcommChannelOpenComplete_status(
            &self,
            _channel: Option<&IOBluetoothRFCOMMChannel>,
            error: c_int,
        ) {
            // Same lifetime rule as `sdpQueryComplete_status`.
            let _keep = self.retain();
            let shared = Arc::clone(&self.ivars().shared);
            if let Ok(mut ev) = shared.events.lock() {
                ev.open_status = Some(error);
            }
            shared.signal_events();
            shared.pending.fetch_sub(1, Ordering::AcqRel);
        }

        #[unsafe(method(rfcommChannelClosed:))]
        unsafe fn rfcommChannelClosed(&self, _channel: Option<&IOBluetoothRFCOMMChannel>) {
            let shared = &self.ivars().shared;
            shared.closed.store(true, Ordering::Release);
            shared.rx_cv.notify_all();
            shared.signal_events();
        }
    }
);

impl Delegate {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Ivars { shared });
        // SAFETY: plain NSObject init.
        unsafe { msg_send![super(this), init] }
    }
}

/// Wait until `ready(&events)` or `deadline`. On the main thread the
/// IOBluetooth callbacks can only be delivered by *this* thread, so pump the
/// run loop; elsewhere block on the events condvar the delegate signals.
/// `ready` receives the locked `Events` — it must not lock anything itself.
fn wait_until(shared: &Shared, deadline: Instant, ready: impl Fn(&Events) -> bool) -> bool {
    let check = |shared: &Shared| {
        shared.events.lock().map(|e| ready(&e)).unwrap_or(true)
            || shared.closed.load(Ordering::Acquire)
    };
    if NSThread::isMainThread_class() {
        while !check(shared) {
            if Instant::now() >= deadline {
                return false;
            }
            // SAFETY: plain run-loop pump on the current (main) thread.
            unsafe {
                CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.02, true);
            }
        }
        return true;
    }
    let mut guard = match shared.events.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    loop {
        if ready(&guard) || shared.closed.load(Ordering::Acquire) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        guard = match shared.events_cv.wait_timeout(guard, deadline - now) {
            Ok((g, _)) => g,
            Err(p) => p.into_inner().0,
        };
    }
}

struct Inner {
    channel: SendCell<Retained<IOBluetoothRFCOMMChannel>>,
    /// Kept alive for the channel's lifetime — IOBluetooth holds the
    /// delegate unretained.
    _delegate: SendCell<Retained<Delegate>>,
    _device: SendCell<Retained<IOBluetoothDevice>>,
    shared: Arc<Shared>,
    /// Bytes per `writeSync` — the negotiated RFCOMM MTU.
    mtu: usize,
    /// Serialises `send` and the keepalive so a `0x00` never lands inside
    /// a frame.
    write_lock: Mutex<()>,
    keepalive: Mutex<Option<IdleKeepalive>>,
    /// Set while a `writeSync` is in flight; the write watchdog closes the
    /// channel if it outlives `WRITE_TIMEOUT` (a peer that stopped reading
    /// starves RFCOMM credits and `writeSync` has no timeout of its own —
    /// closing the channel from another thread is what unblocks it).
    /// Verified live 2026-09-11: with the host accepting and never reading,
    /// `writeSync` stalled after ~93 KB; `closeChannel` from a worker
    /// thread returned 0, the stalled call returned `0xe00002e7` at once
    /// and `rfcommChannelClosed` followed on the main loop.
    write_deadline: Mutex<Option<Instant>>,
    peer: String,
}

/// How often the write watchdog looks at `write_deadline`.
const WRITE_WATCHDOG_POLL: Duration = Duration::from_millis(500);

fn spawn_write_watchdog(weak: std::sync::Weak<Inner>) {
    std::thread::Builder::new()
        .name("wiredesk-rfcomm-write-watchdog".into())
        .spawn(move || loop {
            std::thread::sleep(WRITE_WATCHDOG_POLL);
            let Some(inner) = weak.upgrade() else {
                break;
            };
            let stuck = inner
                .write_deadline
                .lock()
                .map(|d| d.is_some_and(|dl| Instant::now() >= dl))
                .unwrap_or(false);
            if stuck {
                log::warn!(
                    "RFCOMM: write to {} stuck for {WRITE_TIMEOUT:?} — closing channel",
                    inner.peer
                );
                inner.shared.closed.store(true, Ordering::Release);
                inner.shared.rx_cv.notify_all();
                // SAFETY: closing our own channel from a worker thread —
                // the one IOBluetooth call verified safe off the main loop.
                unsafe {
                    inner.channel.0.closeChannel();
                }
                break;
            }
        })
        .expect("spawn write watchdog");
}

impl Inner {
    fn write_raw(&self, bytes: &[u8]) -> Result<()> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(WireDeskError::Transport("RFCOMM channel closed".into()));
        }
        let _g = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        let result = (|| {
            for chunk in bytes.chunks(self.mtu.max(1)) {
                let mut buf = chunk.to_vec();
                *self
                    .write_deadline
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) = Some(Instant::now() + WRITE_TIMEOUT);
                // SAFETY: `buf` outlives the synchronous write; length ≤ MTU ≤ u16.
                let rc = unsafe {
                    self.channel
                        .0
                        .writeSync_length(buf.as_mut_ptr() as *mut c_void, buf.len() as u16)
                };
                if rc != IO_RETURN_SUCCESS {
                    return Err(WireDeskError::Transport(format!(
                        "RFCOMM write to {}: IOReturn {rc:#x}",
                        self.peer
                    )));
                }
            }
            Ok(())
        })();
        *self
            .write_deadline
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        result
    }
}

#[derive(Debug)]
pub struct RfcommTransport {
    inner: Arc<Inner>,
    recv: RecvState,
    /// Only the handle returned by `open` may `recv`; clones are write-only.
    is_owner: bool,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("peer", &self.peer)
            .field("mtu", &self.mtu)
            .field("closed", &self.shared.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl RfcommTransport {
    pub fn open(cfg: &RfcommFactoryConfig) -> Result<Self> {
        if cfg.role != RfcommRole::Connect {
            return Err(WireDeskError::Transport(
                "RFCOMM: macOS side is always the client (role=Listen not supported)".into(),
            ));
        }
        let uuid = uuid::Uuid::parse_str(&cfg.service_uuid).map_err(|e| {
            WireDeskError::Transport(format!(
                "RFCOMM config: service_uuid '{}' not a valid UUID: {e}",
                cfg.service_uuid
            ))
        })?;
        let timeout = if cfg.connect_timeout_secs == 0 {
            Duration::from_secs(15)
        } else {
            Duration::from_secs(u64::from(cfg.connect_timeout_secs))
        };
        let deadline = Instant::now() + timeout;

        let peer_address = cfg.peer_address.clone();
        if !peer_address.trim().is_empty() {
            // Validate before anything touches the radio, so a typo in
            // `peer_address` reports the typo rather than whatever the TCC
            // state happens to be - and so this check stays testable on a
            // Mac that has never granted Bluetooth to the test binary.
            super::common::parse_bt_address(&peer_address)?;
        }
        ensure_bluetooth_authorized()?;
        let mut candidates = on_main_thread(
            move || candidate_devices(&peer_address).map(SendCell),
            timeout,
        )?
        .map(|c| c.0)?;
        if cfg.peer_address.trim().is_empty() && candidates.len() > 1 {
            let start = DISCOVERY_ROTATION.fetch_add(1, Ordering::Relaxed) % candidates.len();
            candidates.rotate_left(start);
        }
        let total = candidates.len();
        let mut failures: Vec<String> = Vec::new();
        for (idx, device) in candidates.into_iter().enumerate() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let untried = (total - idx) as u32;
            let sdp_budget = (remaining / untried).clamp(SDP_QUERY_MIN, SDP_QUERY_BUDGET);
            // SAFETY: plain getters on a live device object.
            let name = unsafe { device.nameOrAddress() }
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".into());
            let addr = unsafe { device.addressString() }
                .map(|s| s.to_string())
                .unwrap_or_default();
            let peer = format!("{name} [{addr}]");
            match try_connect(&device, cfg, uuid, deadline, sdp_budget) {
                Ok((channel, delegate, shared)) => {
                    // SAFETY: getter on the open channel.
                    let mtu = unsafe { channel.getMTU() } as usize;
                    log::info!("RFCOMM: connected to {peer}, mtu {mtu}");
                    let inner = Arc::new(Inner {
                        channel: SendCell(channel),
                        _delegate: SendCell(delegate),
                        _device: SendCell(device),
                        shared,
                        mtu: if mtu == 0 { 127 } else { mtu },
                        write_lock: Mutex::new(()),
                        keepalive: Mutex::new(None),
                        write_deadline: Mutex::new(None),
                        peer,
                    });
                    spawn_write_watchdog(Arc::downgrade(&inner));
                    let period = Duration::from_millis(u64::from(cfg.keepalive_ms));
                    let weak = Arc::downgrade(&inner);
                    let ka = IdleKeepalive::spawn(period, move || match weak.upgrade() {
                        Some(i) => i.write_raw(&[0x00]).is_ok(),
                        None => false,
                    });
                    *inner.keepalive.lock().unwrap_or_else(|p| p.into_inner()) = Some(ka);
                    return Ok(Self {
                        inner,
                        recv: RecvState::new(),
                        is_owner: true,
                    });
                }
                Err(e) => {
                    log::info!("RFCOMM: {peer}: {e}");
                    failures.push(format!("{peer}: {e}"));
                }
            }
        }
        if failures.is_empty() {
            return Err(WireDeskError::Transport(
                "RFCOMM: no paired Bluetooth devices — pair the host in System Settings → Bluetooth first"
                    .into(),
            ));
        }
        Err(WireDeskError::Transport(format!(
            "RFCOMM: no host reachable within {timeout:?} — {}",
            failures.join("; ")
        )))
    }
}

/// Devices to try, in order: the configured address alone, or every paired
/// device with computers first.
fn candidate_devices(peer_address: &str) -> Result<Vec<Retained<IOBluetoothDevice>>> {
    if !peer_address.trim().is_empty() {
        let addr = format_bt_address(super::common::parse_bt_address(peer_address)?);
        let ns = NSString::from_str(&addr);
        // SAFETY: class method with a valid NSString.
        return match unsafe { IOBluetoothDevice::deviceWithAddressString(Some(&ns)) } {
            Some(d) => Ok(vec![d]),
            None => Err(WireDeskError::Transport(format!(
                "RFCOMM: no Bluetooth device with address {addr}"
            ))),
        };
    }
    // SAFETY: class method; returns the OS's paired-device list.
    let Some(paired) = (unsafe { IOBluetoothDevice::pairedDevices() }) else {
        return Ok(Vec::new());
    };
    let mut computers = Vec::new();
    let mut others = Vec::new();
    for obj in paired.iter() {
        let Some(dev) = obj.downcast_ref::<IOBluetoothDevice>() else {
            continue;
        };
        let dev = dev.retain();
        // SAFETY: getter on a live device.
        if unsafe { dev.deviceClassMajor() } == DEVICE_CLASS_MAJOR_COMPUTER {
            computers.push(dev);
        } else {
            others.push(dev);
        }
    }
    computers.extend(others);
    Ok(computers)
}

/// SDP lookup (unless a fixed channel is configured) and async channel open
/// against one device.
fn try_connect(
    device: &IOBluetoothDevice,
    cfg: &RfcommFactoryConfig,
    uuid: uuid::Uuid,
    deadline: Instant,
    sdp_budget: Duration,
) -> Result<(
    Retained<IOBluetoothRFCOMMChannel>,
    Retained<Delegate>,
    Arc<Shared>,
)> {
    let shared = Shared::new();
    let delegate = Delegate::new(Arc::clone(&shared));
    match try_connect_inner(device, cfg, uuid, deadline, sdp_budget, &shared, &delegate) {
        Ok(channel) => Ok((channel, delegate, shared)),
        Err((e, half_open)) => {
            // A completion may still be on its way — keep the delegate
            // (and a half-open channel) alive for it rather than freeing
            // an object IOBluetooth holds unretained.
            park(shared, delegate, half_open);
            Err(e)
        }
    }
}

/// The failure carries a channel object when the open was issued but did
/// not complete in time.
type ConnectErr = (WireDeskError, Option<Retained<IOBluetoothRFCOMMChannel>>);

fn try_connect_inner(
    device: &IOBluetoothDevice,
    cfg: &RfcommFactoryConfig,
    uuid: uuid::Uuid,
    deadline: Instant,
    sdp_budget: Duration,
    shared: &Arc<Shared>,
    delegate: &Retained<Delegate>,
) -> std::result::Result<Retained<IOBluetoothRFCOMMChannel>, ConnectErr> {
    let delegate_any: &AnyObject = delegate;
    let fail = |e: WireDeskError| (e, None);

    let channel_id: u8 = if cfg.channel != 0 {
        cfg.channel
    } else {
        shared.pending.fetch_add(1, Ordering::AcqRel);
        // SAFETY: async SDP query; completion lands in the delegate.
        let rc = unsafe { device.performSDPQuery(Some(delegate_any)) };
        if rc != IO_RETURN_SUCCESS {
            shared.pending.fetch_sub(1, Ordering::AcqRel);
            return Err(fail(WireDeskError::Transport(format!(
                "SDP query refused: IOReturn {rc:#x}"
            ))));
        }
        let sdp_deadline = deadline.min(Instant::now() + sdp_budget);
        let done = wait_until(shared, sdp_deadline, |e| e.sdp_status.is_some());
        if !done {
            return Err(fail(WireDeskError::Transport("SDP query timed out".into())));
        }
        let status = shared
            .events
            .lock()
            .map(|e| e.sdp_status.unwrap_or(-1))
            .unwrap_or(-1);
        if status != IO_RETURN_SUCCESS {
            return Err(fail(WireDeskError::Transport(format!(
                "SDP query failed: IOReturn {status:#x}"
            ))));
        }
        let bytes = uuid.as_bytes();
        // SAFETY: 16 readable bytes.
        let sdp_uuid = unsafe {
            IOBluetoothSDPUUID::uuidWithBytes_length(bytes.as_ptr() as *const c_void, 16)
        }
        .ok_or_else(|| {
            fail(WireDeskError::Transport(
                "IOBluetoothSDPUUID alloc failed".into(),
            ))
        })?;
        // SAFETY: lookup in the device's freshly refreshed SDP cache.
        let record =
            unsafe { device.getServiceRecordForUUID(Some(&sdp_uuid)) }.ok_or_else(|| {
                fail(WireDeskError::Transport(format!(
                "no SDP record for service {uuid} — is the host running with transport=\"rfcomm\"?"
            )))
            })?;
        let mut id: u8 = 0;
        // SAFETY: out-pointer to a local.
        let rc = unsafe { record.getRFCOMMChannelID(&mut id) };
        if rc != IO_RETURN_SUCCESS || id == 0 {
            return Err(fail(WireDeskError::Transport(
                "SDP record carries no RFCOMM channel".into(),
            )));
        }
        id
    };

    let mut channel: Option<Retained<IOBluetoothRFCOMMChannel>> = None;
    shared.pending.fetch_add(1, Ordering::AcqRel);
    // SAFETY: async open; the delegate reports completion on the main loop.
    let rc = unsafe {
        device.openRFCOMMChannelAsync_withChannelID_delegate(
            Some(&mut channel),
            channel_id,
            Some(delegate_any),
        )
    };
    if rc != IO_RETURN_SUCCESS {
        shared.pending.fetch_sub(1, Ordering::AcqRel);
        return Err((
            WireDeskError::Transport(format!(
                "open channel {channel_id} refused: IOReturn {rc:#x}"
            )),
            channel,
        ));
    }
    let done = wait_until(shared, deadline, |e| e.open_status.is_some());
    let status = shared.events.lock().map(|e| e.open_status).unwrap_or(None);
    match (done, status, channel) {
        (true, Some(IO_RETURN_SUCCESS), Some(ch)) => Ok(ch),
        (false, _, ch) => {
            if let Some(ch) = ch.as_ref() {
                // SAFETY: best-effort teardown of a half-open channel; the
                // object itself is parked until its completion arrives.
                unsafe { ch.closeChannel() };
            }
            Err((
                WireDeskError::Transport(format!("open channel {channel_id} timed out")),
                ch,
            ))
        }
        (_, status, ch) => {
            if let Some(ch) = ch.as_ref() {
                // SAFETY: as above.
                unsafe { ch.closeChannel() };
            }
            Err((
                WireDeskError::Transport(format!(
                    "open channel {channel_id} failed: IOReturn {:#x}",
                    status.unwrap_or(-1)
                )),
                ch,
            ))
        }
    }
}

impl Drop for RfcommTransport {
    fn drop(&mut self) {
        // The owner going away ends the link for everyone: stop the
        // keepalive and flag the channel closed so write-only clones fail
        // fast. The channel itself is closed by `Inner::drop`, whenever the
        // last handle (clone or a keepalive mid-write) lets go.
        if !self.is_owner {
            return;
        }
        if let Ok(mut ka) = self.inner.keepalive.lock() {
            ka.take();
        }
        self.inner.shared.closed.store(true, Ordering::Release);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        // SAFETY: closing our own channel; idempotent if already closed.
        unsafe {
            self.channel.0.closeChannel();
        }
    }
}

impl Transport for RfcommTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        let frame = encode_frame(packet)?;
        self.inner.write_raw(&frame)?;
        if let Ok(ka) = self.inner.keepalive.lock() {
            if let Some(ka) = ka.as_ref() {
                ka.note_write();
            }
        }
        Ok(())
    }

    fn recv(&mut self) -> Result<Packet> {
        if !self.is_owner {
            return Err(WireDeskError::Transport(
                "RFCOMM recv on cloned (write-only) handle".into(),
            ));
        }
        // One call waits at most RECV_POLL for a *packet*; keepalive zeros
        // and partial frames must not extend the wait (see win.rs).
        let deadline = Instant::now() + RECV_POLL;
        loop {
            if let Some(p) = self.recv.next_packet()? {
                return Ok(p);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.recv.on_timeout());
            }
            let shared = &self.inner.shared;
            let mut q = shared.rx.lock().unwrap_or_else(|p| p.into_inner());
            if q.is_empty() {
                if shared.closed.load(Ordering::Acquire) {
                    return Err(WireDeskError::Transport(format!(
                        "RFCOMM channel to {} closed",
                        self.inner.peer
                    )));
                }
                q = match shared.rx_cv.wait_timeout(q, remaining) {
                    Ok((g, _)) => g,
                    Err(p) => p.into_inner().0,
                };
                if q.is_empty() {
                    drop(q);
                    if shared.closed.load(Ordering::Acquire) {
                        return Err(WireDeskError::Transport(format!(
                            "RFCOMM channel to {} closed",
                            self.inner.peer
                        )));
                    }
                    // Spurious wakeup → loop and re-check the deadline.
                    continue;
                }
            }
            let bytes: Vec<u8> = q.drain(..).collect();
            drop(q);
            self.recv.reader.feed(&bytes);
        }
    }

    fn is_connected(&self) -> bool {
        !self.inner.shared.closed.load(Ordering::Acquire)
    }

    fn name(&self) -> &'static str {
        "rfcomm-client"
    }

    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        Ok(Box::new(RfcommTransport {
            inner: Arc::clone(&self.inner),
            recv: RecvState::new(),
            is_owner: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RfcommFactoryConfig {
        RfcommFactoryConfig {
            service_uuid: "00000000-0000-4000-8000-000000000002".to_string(),
            peer_address: String::new(),
            channel: 0,
            connect_timeout_secs: 1,
            keepalive_ms: 0,
            require_encryption: true,
            role: RfcommRole::Connect,
        }
    }

    #[test]
    fn open_with_invalid_service_uuid_errors() {
        let mut c = cfg();
        c.service_uuid = "nope".into();
        let err = RfcommTransport::open(&c).unwrap_err().to_string();
        assert!(err.contains("service_uuid"), "{err}");
    }

    #[test]
    fn open_with_bad_peer_address_errors() {
        let mut c = cfg();
        c.peer_address = "not-an-address".into();
        let err = RfcommTransport::open(&c).unwrap_err().to_string();
        assert!(err.contains("bad Bluetooth address"), "{err}");
    }

    #[test]
    fn listen_role_is_rejected_on_mac() {
        let mut c = cfg();
        c.role = RfcommRole::Listen;
        let err = RfcommTransport::open(&c).unwrap_err().to_string();
        assert!(err.contains("client"), "{err}");
    }
}
