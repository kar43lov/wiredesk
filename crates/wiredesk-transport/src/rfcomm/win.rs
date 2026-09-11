//! Windows RFCOMM transport over Winsock `AF_BTH` sockets.
//!
//! - `RfcommRole::Listen` (the host): bind an RFCOMM socket, publish an SDP
//!   record under `service_uuid` via `WSASetService` so clients can look
//!   the channel up, then accept one client at a time. `open()` returns as
//!   soon as the service is published; the accept happens inside `recv()`,
//!   which keeps returning `"recv timeout"` until a client arrives — the
//!   host's tick loop treats that as idle, exactly like an unplugged serial
//!   port.
//! - `RfcommRole::Connect` (a Windows client): resolve the channel from the
//!   host's SDP server via `WSALookupService` (or use the fixed one) and
//!   `connect` with a timeout.
//!
//! Winsock sockets are plain blocking handles, so reads and writes are
//! ordinary `std::io` calls on `socket2::Socket`; the writer thread and the
//! keepalive get their own duplicated handle (`try_clone`) so a blocking
//! read on the reader never holds up a write.
//!
//! Reads never use `SO_RCVTIMEO`: the Bluetooth provider answers a timed
//! `recv` with `ERROR_IO_PENDING` (997, "Overlapped I/O operation is in
//! progress") under load instead of `WSAETIMEDOUT`, which the host saw live
//! (2026-09-11) as a fatal read error and a dropped link every ~80 s of a
//! large Mac→host transfer. A dedicated reader thread blocks in `recv`
//! with no timeout and hands bytes over through a condvar; `recv()` on the
//! transport waits on that condvar for at most `RECV_POLL`.

use std::io::{Read, Write};
use std::mem::size_of;
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use windows::core::{GUID, PWSTR};
use windows::Win32::Devices::Bluetooth::{
    AF_BTH, BTHPROTO_RFCOMM, NS_BTH, SOCKADDR_BTH, SOL_RFCOMM, SO_BTH_AUTHENTICATE, SO_BTH_ENCRYPT,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::WinSock::{
    setsockopt, WSAGetLastError, WSALookupServiceBeginW, WSALookupServiceEnd,
    WSALookupServiceNextW, WSASetServiceW, CSADDR_INFO, LUP_FLUSHCACHE, LUP_RETURN_ADDR,
    RNRSERVICE_DELETE, RNRSERVICE_REGISTER, SOCKADDR, SOCKET, SOCKET_ADDRESS, SOCK_STREAM,
    WSAESETSERVICEOP, WSAQUERYSETW,
};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::packet::Packet;

use super::common::{
    format_bt_address, parse_bt_address, IdleKeepalive, RecvState, RECV_POLL, WRITE_TIMEOUT,
};
use super::{RfcommFactoryConfig, RfcommRole};
use crate::framing::encode_frame;
use crate::transport::Transport;

/// `BT_PORT_ANY` — let the stack pick a free RFCOMM channel.
const BT_PORT_ANY: u32 = u32::MAX;

/// `sizeof(SOCKADDR_BTH)` — the struct is `#pragma pack(1)`.
const SOCKADDR_BTH_LEN: usize = 30;

/// Read block size; RFCOMM frames are ≤ 1 KB, a burst at 120 KB/s fills
/// this in ~70 ms.
const READ_BLOCK: usize = 8192;

/// Poll cadence while waiting for a client to connect (Listen role).
const ACCEPT_POLL: Duration = Duration::from_millis(25);

/// Listen role: a connected client that has sent nothing — not even a
/// keepalive — for this long while another client is knocking is
/// considered gone. `recv` then fails the transport, the host's reopen
/// loop builds a fresh listener *and a fresh Session* (shell closed,
/// clipboard state dropped, back to WaitingForHello), and the newcomer's
/// own reconnect backoff brings it in a second later. Covers a frozen
/// client process whose ACL link the radio keeps alive; without this the
/// host would sit on the dead socket and never `accept` again.
const CLIENT_IDLE_TAKEOVER: Duration = Duration::from_secs(8);

/// Only one SDP lookup thread at a time (Connect role). `WSALookupService`
/// has no timeout; a lookup we gave up on may still be running, and
/// spawning another on every reconnect would pile them up.
///
/// Holds the generation and start time of the lookup currently in flight.
/// A plain flag would be a trap: `WSALookupService` can hang for good (the
/// peer sleeping mid-transaction is enough), and a flag its thread never
/// clears turns every later `channel = 0` connect into "previous SDP
/// lookup still running" until the process restarts. After
/// [`SDP_LOOKUP_ABANDON`] the slot is handed to the next caller instead;
/// the generation is what keeps the abandoned thread from clearing the
/// newcomer's claim when it finally wakes up.
static SDP_LOOKUP_BUSY: Mutex<Option<(u64, Instant)>> = Mutex::new(None);
static SDP_LOOKUP_GEN: AtomicU64 = AtomicU64::new(0);

/// How long an in-flight SDP lookup keeps the slot before the next caller
/// is allowed to start its own. Well past any lookup that is merely slow -
/// the budget a connect gives one is seconds.
const SDP_LOOKUP_ABANDON: Duration = Duration::from_secs(60);

/// Claim the lookup slot, or report why not. `Some(generation)` on success.
fn claim_sdp_slot() -> Option<u64> {
    let mut busy = SDP_LOOKUP_BUSY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, started)) = *busy {
        if started.elapsed() < SDP_LOOKUP_ABANDON {
            return None;
        }
        log::warn!(
            "RFCOMM: abandoning an SDP lookup that has been running for {:?}",
            started.elapsed()
        );
    }
    let gen = SDP_LOOKUP_GEN.fetch_add(1, Ordering::AcqRel) + 1;
    *busy = Some((gen, Instant::now()));
    Some(gen)
}

/// Release the slot, but only if it is still ours - an abandoned lookup
/// waking up late must not clear a claim that belongs to someone else.
fn release_sdp_slot(gen: u64) {
    let mut busy = SDP_LOOKUP_BUSY.lock().unwrap_or_else(|e| e.into_inner());
    if busy.map(|(g, _)| g) == Some(gen) {
        *busy = None;
    }
}

fn last_wsa_error(what: &str) -> WireDeskError {
    // SAFETY: plain thread-local error read.
    let code = unsafe { WSAGetLastError() };
    WireDeskError::Transport(format!("RFCOMM {what}: WSA error {}", code.0))
}

fn bth_sockaddr(bt_addr: u64, port: u32) -> Result<SockAddr> {
    let sa = SOCKADDR_BTH {
        addressFamily: AF_BTH,
        btAddr: bt_addr,
        serviceClassId: GUID::zeroed(),
        port,
    };
    // SAFETY: we copy exactly SOCKADDR_BTH_LEN bytes of a packed struct
    // into the zeroed storage and report that length.
    let (_, addr) = unsafe {
        SockAddr::try_init(|storage, len| {
            std::ptr::copy_nonoverlapping(
                &sa as *const SOCKADDR_BTH as *const u8,
                storage as *mut u8,
                SOCKADDR_BTH_LEN,
            );
            *len = SOCKADDR_BTH_LEN as _;
            Ok(())
        })
    }
    .map_err(|e| WireDeskError::Transport(format!("RFCOMM sockaddr: {e}")))?;
    Ok(addr)
}

/// Copy a `SockAddr` back into the packed struct (unaligned-safe).
fn sockaddr_to_bth(addr: &SockAddr) -> SOCKADDR_BTH {
    let mut sa = SOCKADDR_BTH::default();
    let n = (addr.len() as usize).min(SOCKADDR_BTH_LEN);
    // SAFETY: both buffers are at least `n` bytes; byte copy has no
    // alignment requirement.
    unsafe {
        std::ptr::copy_nonoverlapping(
            addr.as_ptr() as *const u8,
            &mut sa as *mut SOCKADDR_BTH as *mut u8,
            n,
        );
    }
    sa
}

fn new_rfcomm_socket() -> Result<Socket> {
    Socket::new(
        Domain::from(i32::from(AF_BTH)),
        Type::STREAM,
        Some(Protocol::from(BTHPROTO_RFCOMM as i32)),
    )
    .map_err(|e| WireDeskError::Transport(format!("RFCOMM socket: {e}")))
}

/// Demand pairing (authentication) and encryption on the link.
fn require_secure_link(sock: &Socket) -> Result<()> {
    let one = 1u32.to_ne_bytes();
    let s = SOCKET(sock.as_raw_socket() as usize);
    for (name, opt) in [
        ("SO_BTH_AUTHENTICATE", SO_BTH_AUTHENTICATE as i32),
        ("SO_BTH_ENCRYPT", SO_BTH_ENCRYPT as i32),
    ] {
        // SAFETY: valid socket handle and a 4-byte option value.
        if unsafe { setsockopt(s, SOL_RFCOMM as i32, opt, Some(&one)) } != 0 {
            return Err(last_wsa_error(name));
        }
    }
    Ok(())
}

/// Owns the buffers `WSASetService` points into so the same record can be
/// deregistered on drop.
struct SdpRecord {
    name: Vec<u16>,
    guid: GUID,
    local: SOCKADDR_BTH,
}

impl SdpRecord {
    fn apply(&mut self, op: WSAESETSERVICEOP) -> Result<()> {
        let mut csa = CSADDR_INFO {
            LocalAddr: SOCKET_ADDRESS {
                lpSockaddr: &mut self.local as *mut SOCKADDR_BTH as *mut SOCKADDR,
                iSockaddrLength: SOCKADDR_BTH_LEN as i32,
            },
            RemoteAddr: SOCKET_ADDRESS {
                lpSockaddr: &mut self.local as *mut SOCKADDR_BTH as *mut SOCKADDR,
                iSockaddrLength: SOCKADDR_BTH_LEN as i32,
            },
            iSocketType: SOCK_STREAM.0,
            iProtocol: BTHPROTO_RFCOMM as i32,
        };
        let qs = WSAQUERYSETW {
            dwSize: size_of::<WSAQUERYSETW>() as u32,
            lpszServiceInstanceName: PWSTR(self.name.as_mut_ptr()),
            lpServiceClassId: &mut self.guid,
            dwNameSpace: NS_BTH,
            dwNumberOfCsAddrs: 1,
            lpcsaBuffer: &mut csa,
            ..Default::default()
        };
        // SAFETY: every pointer in `qs` refers to memory that outlives the call.
        if unsafe { WSASetServiceW(&qs, op, 0) } != 0 {
            let what = if op == RNRSERVICE_REGISTER {
                "SDP register"
            } else {
                "SDP deregister"
            };
            return Err(last_wsa_error(what));
        }
        Ok(())
    }
}

/// Ask the host's SDP server which RFCOMM channel carries `service_uuid`.
fn sdp_lookup_channel(bt_addr: u64, service_uuid: uuid::Uuid) -> Result<u8> {
    let mut context: Vec<u16> = format!("({})\0", format_bt_address(bt_addr))
        .encode_utf16()
        .collect();
    let mut guid = GUID::from_u128(service_uuid.as_u128());
    let restrictions = WSAQUERYSETW {
        dwSize: size_of::<WSAQUERYSETW>() as u32,
        lpServiceClassId: &mut guid,
        dwNameSpace: NS_BTH,
        lpszContext: PWSTR(context.as_mut_ptr()),
        ..Default::default()
    };
    let mut handle = HANDLE::default();
    // SAFETY: valid query set; handle out-pointer.
    if unsafe {
        WSALookupServiceBeginW(&restrictions, LUP_FLUSHCACHE | LUP_RETURN_ADDR, &mut handle)
    } != 0
    {
        return Err(last_wsa_error("SDP lookup begin"));
    }
    // u64 storage so the WSAQUERYSETW the stack writes here is 8-byte
    // aligned (a Vec<u8> gives no such guarantee).
    let mut buf = vec![0u64; 1024];
    let mut len = (buf.len() * size_of::<u64>()) as u32;
    // SAFETY: `buf` is `len` bytes, suitably aligned; the result is read
    // only while `buf` is alive.
    let rc = unsafe {
        WSALookupServiceNextW(
            handle,
            LUP_RETURN_ADDR,
            &mut len,
            Some(buf.as_mut_ptr() as *mut WSAQUERYSETW),
        )
    };
    let result = if rc != 0 {
        Err(WireDeskError::Transport(format!(
            "RFCOMM: host {} has no SDP record for {service_uuid} (WSA error {}) — is it running with transport=\"rfcomm\"?",
            format_bt_address(bt_addr),
            // SAFETY: plain error read.
            unsafe { WSAGetLastError() }.0
        )))
    } else {
        // SAFETY: on success the buffer holds a WSAQUERYSETW whose
        // lpcsaBuffer points inside `buf`.
        let qs = unsafe { &*(buf.as_ptr() as *const WSAQUERYSETW) };
        if qs.dwNumberOfCsAddrs == 0 || qs.lpcsaBuffer.is_null() {
            Err(WireDeskError::Transport(
                "RFCOMM: SDP record carries no address".into(),
            ))
        } else {
            // SAFETY: as above; RemoteAddr is a SOCKADDR_BTH per NS_BTH.
            let sa = unsafe {
                let csa = &*qs.lpcsaBuffer;
                std::ptr::read_unaligned(csa.RemoteAddr.lpSockaddr as *const SOCKADDR_BTH)
            };
            let port = sa.port;
            u8::try_from(port)
                .ok()
                .filter(|p| (1..=30).contains(p))
                .ok_or_else(|| {
                    WireDeskError::Transport(format!("RFCOMM: SDP record has bad channel {port}"))
                })
        }
    };
    // SAFETY: closing the lookup handle we opened.
    unsafe {
        let _ = WSALookupServiceEnd(handle);
    }
    result
}

/// `WSALookupService` is synchronous and has no timeout of its own, so run
/// it on a helper thread and give up at `deadline`. An abandoned lookup
/// finishes on its own later and drops its result.
fn sdp_lookup_channel_within(bt_addr: u64, uuid: uuid::Uuid, deadline: Instant) -> Result<u8> {
    let Some(gen) = claim_sdp_slot() else {
        return Err(WireDeskError::Transport(
            "RFCOMM: previous SDP lookup still running — retry later".into(),
        ));
    };
    let (tx, rx) = std::sync::mpsc::channel();
    if let Err(e) = std::thread::Builder::new()
        .name("wiredesk-rfcomm-sdp".into())
        .spawn(move || {
            let r = sdp_lookup_channel(bt_addr, uuid);
            release_sdp_slot(gen);
            let _ = tx.send(r);
        })
    {
        release_sdp_slot(gen);
        return Err(WireDeskError::Transport(format!("RFCOMM SDP thread: {e}")));
    }
    let budget = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(budget) {
        Ok(r) => r,
        Err(_) => Err(WireDeskError::Transport(format!(
            "RFCOMM: SDP lookup on {} timed out after {budget:?}",
            format_bt_address(bt_addr)
        ))),
    }
}

struct Inner {
    role: RfcommRole,
    /// Listen role: the bound+listening socket (non-blocking).
    listener: Option<Socket>,
    /// The connected peer socket (blocking, write timeout = `WRITE_TIMEOUT`,
    /// no read timeout — see the module docs).
    client: Mutex<Option<Socket>>,
    sdp: Mutex<Option<SdpRecord>>,
    closed: AtomicBool,
    write_lock: Mutex<()>,
    keepalive: Mutex<Option<IdleKeepalive>>,
    peer: Mutex<String>,
    /// Bytes the reader thread pulled off the radio, waiting for `recv`.
    rx: Mutex<Vec<u8>>,
    rx_cv: Condvar,
    /// Why the reader thread stopped (peer closed, read error); `recv`
    /// surfaces it once `rx` is drained.
    rx_error: Mutex<Option<String>>,
    /// Last time any byte arrived from the connected client (Listen role
    /// idle-takeover, see `CLIENT_IDLE_TAKEOVER`).
    last_rx: Mutex<Instant>,
    /// Bumped by every `set_client`; a reader thread from an earlier client
    /// sees the mismatch and drops out instead of feeding stale bytes.
    generation: AtomicU64,
}

impl Inner {
    fn new(role: RfcommRole, listener: Option<Socket>, sdp: Option<SdpRecord>) -> Self {
        Self {
            role,
            listener,
            client: Mutex::new(None),
            sdp: Mutex::new(sdp),
            closed: AtomicBool::new(false),
            write_lock: Mutex::new(()),
            keepalive: Mutex::new(None),
            peer: Mutex::new(String::new()),
            rx: Mutex::new(Vec::new()),
            rx_cv: Condvar::new(),
            rx_error: Mutex::new(None),
            last_rx: Mutex::new(Instant::now()),
            generation: AtomicU64::new(0),
        }
    }

    fn last_rx(&self) -> Instant {
        *self.last_rx.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Tear the client socket down (unblocking the reader thread) and flag
    /// the link closed; the session loop then reopens the transport.
    fn drop_client(&self) {
        self.closed.store(true, Ordering::Release);
        if let Ok(mut c) = self.client.lock() {
            if let Some(s) = c.take() {
                let _ = s.shutdown(std::net::Shutdown::Both);
            }
        }
        self.rx_cv.notify_all();
    }

    /// Blocking reader for one client socket. Exits when the socket is
    /// shut down (peer gone, takeover, `Inner::drop`), when `Inner` itself
    /// is gone, or when a newer client replaced this one.
    fn reader_loop(weak: std::sync::Weak<Inner>, mut sock: Socket, generation: u64) {
        let mut buf = [0u8; READ_BLOCK];
        loop {
            let read = sock.read(&mut buf);
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if inner.generation.load(Ordering::Acquire) != generation {
                return;
            }
            match read {
                Ok(0) => {
                    *inner.rx_error.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some("peer disconnected".into());
                    inner.rx_cv.notify_all();
                    return;
                }
                Ok(n) => {
                    *inner.last_rx.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
                    inner
                        .rx
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend_from_slice(&buf[..n]);
                    inner.rx_cv.notify_all();
                }
                Err(e) => {
                    *inner.rx_error.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some(format!("read: {e}"));
                    inner.rx_cv.notify_all();
                    return;
                }
            }
        }
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("role", &self.role)
            .field(
                "peer",
                &self.peer.lock().map(|p| p.clone()).unwrap_or_default(),
            )
            .field("closed", &self.closed.load(Ordering::Relaxed))
            .finish()
    }
}

impl Inner {
    fn connected_clone(&self) -> Result<Socket> {
        let guard = self.client.lock().unwrap_or_else(|p| p.into_inner());
        match guard.as_ref() {
            Some(s) => s
                .try_clone()
                .map_err(|e| WireDeskError::Transport(format!("RFCOMM socket clone: {e}"))),
            None => Err(WireDeskError::Transport(
                "RFCOMM: no client connected".into(),
            )),
        }
    }

    /// Write on a caller-owned duplicate handle; serialised with the
    /// keepalive so a `0x00` never lands inside a frame.
    fn write_raw(&self, sock: &Socket, bytes: &[u8]) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(WireDeskError::Transport("RFCOMM link closed".into()));
        }
        let _g = self.write_lock.lock().unwrap_or_else(|p| p.into_inner());
        (&*sock).write_all(bytes).map_err(|e| {
            self.closed.store(true, Ordering::Release);
            WireDeskError::Transport(format!("RFCOMM write: {e}"))
        })
    }

    fn set_client(self: &Arc<Self>, sock: Socket, peer: String) -> Result<()> {
        sock.set_nonblocking(false)
            .and_then(|_| sock.set_write_timeout(Some(WRITE_TIMEOUT)))
            .map_err(|e| WireDeskError::Transport(format!("RFCOMM socket setup: {e}")))?;
        let reader_sock = sock
            .try_clone()
            .map_err(|e| WireDeskError::Transport(format!("RFCOMM socket clone: {e}")))?;
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        {
            let mut rx = self.rx.lock().unwrap_or_else(|p| p.into_inner());
            rx.clear();
        }
        *self.rx_error.lock().unwrap_or_else(|p| p.into_inner()) = None;
        *self.last_rx.lock().unwrap_or_else(|p| p.into_inner()) = Instant::now();
        *self.client.lock().unwrap_or_else(|p| p.into_inner()) = Some(sock);
        *self.peer.lock().unwrap_or_else(|p| p.into_inner()) = peer;
        self.closed.store(false, Ordering::Release);
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("wiredesk-rfcomm-reader".into())
            .spawn(move || Inner::reader_loop(weak, reader_sock, generation))
            .map_err(|e| WireDeskError::Transport(format!("RFCOMM reader thread: {e}")))?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct RfcommTransport {
    inner: Arc<Inner>,
    recv: RecvState,
    /// This handle's own duplicate of the client socket for writes; made
    /// lazily on the first `send` after a client is connected.
    write_sock: Option<Socket>,
    is_owner: bool,
}

impl RfcommTransport {
    pub fn open(cfg: &RfcommFactoryConfig) -> Result<Self> {
        let uuid = uuid::Uuid::parse_str(&cfg.service_uuid).map_err(|e| {
            WireDeskError::Transport(format!(
                "RFCOMM config: service_uuid '{}' not a valid UUID: {e}",
                cfg.service_uuid
            ))
        })?;
        let inner = match cfg.role {
            RfcommRole::Listen => open_listen(cfg, uuid)?,
            RfcommRole::Connect => open_connect(cfg, uuid)?,
        };
        let period = Duration::from_millis(u64::from(cfg.keepalive_ms));
        let weak = Arc::downgrade(&inner);
        let ka = IdleKeepalive::spawn(period, move || {
            let Some(i) = weak.upgrade() else {
                return false;
            };
            // A failed write marks the link closed; the session loop then
            // reopens the transport, which drops `Inner` and ends this
            // thread through the failed upgrade above. No client yet
            // (Listen role) simply means "keep waiting".
            if let Ok(sock) = i.connected_clone() {
                let _ = i.write_raw(&sock, &[0x00]);
            }
            true
        });
        *inner.keepalive.lock().unwrap_or_else(|p| p.into_inner()) = Some(ka);
        Ok(Self {
            inner,
            recv: RecvState::new(),
            write_sock: None,
            is_owner: true,
        })
    }

    fn write_socket(&mut self) -> Result<&Socket> {
        if self.write_sock.is_none() {
            self.write_sock = Some(self.inner.connected_clone()?);
        }
        Ok(self.write_sock.as_ref().expect("just set"))
    }

    /// The reader thread's exit reason, if it has stopped.
    fn reader_stopped(&self) -> Option<String> {
        self.inner
            .rx_error
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    /// The client is gone: release it so the listener accepts the next one
    /// (Listen role) and report the reason to the session loop.
    fn disconnected(&mut self, why: String) -> WireDeskError {
        let peer = self
            .inner
            .peer
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();
        self.inner.drop_client();
        self.write_sock = None;
        WireDeskError::Transport(format!("RFCOMM peer {peer}: {why}"))
    }

    /// Listen role: poll `accept` for up to `RECV_POLL`.
    /// Listen role: a silent client plus a newcomer waiting in the backlog
    /// → tear the link down so the host reopens with a clean Session. The
    /// newcomer is accepted and dropped on purpose: it reconnects to the
    /// fresh listener through its own backoff, and the transport never
    /// swaps peers underneath a live `Session`.
    fn superseded_by_new_client(&mut self) -> Result<()> {
        if self.inner.role != RfcommRole::Listen
            || self.inner.last_rx().elapsed() < CLIENT_IDLE_TAKEOVER
        {
            return Ok(());
        }
        let Some(listener) = self.inner.listener.as_ref() else {
            return Ok(());
        };
        let Ok((newcomer, addr)) = listener.accept() else {
            return Ok(());
        };
        let peer = format_bt_address(sockaddr_to_bth(&addr).btAddr);
        let old = self
            .inner
            .peer
            .lock()
            .map(|p| p.clone())
            .unwrap_or_default();
        log::warn!(
            "RFCOMM: client {old} silent for {:?} while {peer} is connecting — dropping the link, \
             reopening for a fresh session",
            self.inner.last_rx().elapsed()
        );
        let _ = newcomer.shutdown(std::net::Shutdown::Both);
        self.inner.drop_client();
        self.write_sock = None;
        Err(WireDeskError::Transport(format!(
            "RFCOMM client {old} silent, superseded by {peer}"
        )))
    }

    fn try_accept(&mut self) -> Result<bool> {
        let Some(listener) = self.inner.listener.as_ref() else {
            return Ok(false);
        };
        let start = Instant::now();
        loop {
            match listener.accept() {
                Ok((sock, addr)) => {
                    let peer = format_bt_address(sockaddr_to_bth(&addr).btAddr);
                    log::info!("RFCOMM: client connected from {peer}");
                    self.inner.set_client(sock, peer)?;
                    self.write_sock = None;
                    return Ok(true);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed() >= RECV_POLL {
                        return Ok(false);
                    }
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(e) => {
                    return Err(WireDeskError::Transport(format!("RFCOMM accept: {e}")));
                }
            }
        }
    }
}

fn open_listen(cfg: &RfcommFactoryConfig, uuid: uuid::Uuid) -> Result<Arc<Inner>> {
    let listener = new_rfcomm_socket()?;
    if cfg.require_encryption {
        require_secure_link(&listener)?;
    } else {
        log::warn!(
            "RFCOMM: accepting unauthenticated clients (require_encryption = false) — \
             any Bluetooth device in range that knows the service UUID can drive this host"
        );
    }
    let port = if cfg.channel == 0 {
        BT_PORT_ANY
    } else {
        u32::from(cfg.channel)
    };
    listener
        .bind(&bth_sockaddr(0, port)?)
        .map_err(|e| WireDeskError::Transport(format!("RFCOMM bind channel {port}: {e}")))?;
    listener
        .listen(1)
        .map_err(|e| WireDeskError::Transport(format!("RFCOMM listen: {e}")))?;
    let local = listener
        .local_addr()
        .map_err(|e| WireDeskError::Transport(format!("RFCOMM getsockname: {e}")))?;
    let local_bth = sockaddr_to_bth(&local);
    let channel = local_bth.port;
    let mut sdp = SdpRecord {
        name: format!("{}\0", wiredesk_core::rfcomm_config::DEFAULT_SERVICE_NAME)
            .encode_utf16()
            .collect(),
        guid: GUID::from_u128(uuid.as_u128()),
        local: local_bth,
    };
    sdp.apply(RNRSERVICE_REGISTER)?;
    listener
        .set_nonblocking(true)
        .map_err(|e| WireDeskError::Transport(format!("RFCOMM listener nonblocking: {e}")))?;
    log::info!("RFCOMM: listening on channel {channel}, SDP record {uuid} published");
    Ok(Arc::new(Inner::new(
        RfcommRole::Listen,
        Some(listener),
        Some(sdp),
    )))
}

fn open_connect(cfg: &RfcommFactoryConfig, uuid: uuid::Uuid) -> Result<Arc<Inner>> {
    if cfg.peer_address.trim().is_empty() {
        return Err(WireDeskError::Transport(
            "RFCOMM: rfcomm.peer_address (the host's Bluetooth address) is required on a Windows client"
                .into(),
        ));
    }
    let bt_addr = parse_bt_address(&cfg.peer_address)?;
    let timeout = if cfg.connect_timeout_secs == 0 {
        Duration::from_secs(15)
    } else {
        Duration::from_secs(u64::from(cfg.connect_timeout_secs))
    };
    let deadline = Instant::now() + timeout;
    // Create the socket first: socket2 initialises Winsock on the first
    // `Socket::new`, and `WSALookupService*` does not — an SDP query before
    // any socket exists fails with WSANOTINITIALISED.
    let sock = new_rfcomm_socket()?;
    let channel = if cfg.channel != 0 {
        cfg.channel
    } else {
        sdp_lookup_channel_within(bt_addr, uuid, deadline)?
    };
    let timeout = deadline.saturating_duration_since(Instant::now());
    if timeout.is_zero() {
        return Err(WireDeskError::Transport(format!(
            "RFCOMM: SDP lookup on {} used up the connect budget",
            format_bt_address(bt_addr)
        )));
    }
    if cfg.require_encryption {
        // Demand an authenticated, encrypted link from our side too, so a
        // device spoofing the host's address without its link key gets
        // nothing — the host enforces the same on accept.
        require_secure_link(&sock)?;
    }
    sock.connect_timeout(&bth_sockaddr(bt_addr, u32::from(channel))?, timeout)
        .map_err(|e| {
            WireDeskError::Transport(format!(
                "RFCOMM connect to {} channel {channel}: {e}",
                format_bt_address(bt_addr)
            ))
        })?;
    let peer = format_bt_address(bt_addr);
    log::info!("RFCOMM: connected to {peer} channel {channel}");
    let inner = Arc::new(Inner::new(RfcommRole::Connect, None, None));
    inner.set_client(sock, peer)?;
    Ok(inner)
}

impl Drop for RfcommTransport {
    fn drop(&mut self) {
        // The owner going away ends the link: stop the keepalive and flag
        // the link closed so write-only clones fail fast. Sockets and the
        // SDP record are released by `Inner::drop`, whenever the last
        // handle (a clone, or the keepalive mid-write) lets go.
        if !self.is_owner {
            return;
        }
        if let Ok(mut ka) = self.inner.keepalive.lock() {
            ka.take();
        }
        self.inner.closed.store(true, Ordering::Release);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(mut sdp) = self.sdp.lock() {
            if let Some(mut rec) = sdp.take() {
                if let Err(e) = rec.apply(RNRSERVICE_DELETE) {
                    log::debug!("RFCOMM: {e}");
                }
            }
        }
        // Shutting the socket down ends the reader thread's blocking read.
        self.drop_client();
        // `listener` closes with the struct.
    }
}

impl Transport for RfcommTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        let frame = encode_frame(packet)?;
        let inner = Arc::clone(&self.inner);
        let sock = self.write_socket()?;
        inner.write_raw(sock, &frame)?;
        if let Ok(ka) = inner.keepalive.lock() {
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
        // One call waits at most ~RECV_POLL for a *packet*; bytes that
        // don't complete a frame (keepalive zeros, a partial frame) must
        // not extend the wait, or the session loops on both sides would
        // never get their timer tick while the peer keeps the link warm.
        let started = Instant::now();
        loop {
            if let Some(p) = self.recv.next_packet()? {
                return Ok(p);
            }
            let remaining = RECV_POLL.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(self.recv.on_timeout());
            }
            let has_client = self
                .inner
                .client
                .lock()
                .map(|c| c.is_some())
                .unwrap_or(false);
            if !has_client {
                if self.inner.role == RfcommRole::Connect
                    || self.inner.closed.load(Ordering::Acquire)
                {
                    return Err(WireDeskError::Transport("RFCOMM link closed".into()));
                }
                if !self.try_accept()? {
                    return Err(self.recv.on_timeout());
                }
                continue;
            }
            self.superseded_by_new_client()?;
            // Take whatever the reader thread has queued, waiting on the
            // condvar for the rest of this poll if the queue is empty.
            let mut rx = self.inner.rx.lock().unwrap_or_else(|p| p.into_inner());
            if rx.is_empty() {
                if let Some(why) = self.reader_stopped() {
                    drop(rx);
                    return Err(self.disconnected(why));
                }
                let (guard, _) = self
                    .inner
                    .rx_cv
                    .wait_timeout(rx, remaining)
                    .unwrap_or_else(|p| p.into_inner());
                rx = guard;
            }
            if !rx.is_empty() {
                self.recv.reader.feed(&rx);
                rx.clear();
            }
        }
    }

    fn is_connected(&self) -> bool {
        !self.inner.closed.load(Ordering::Acquire)
            && self
                .inner
                .client
                .lock()
                .map(|c| c.is_some())
                .unwrap_or(false)
    }

    fn name(&self) -> &'static str {
        match self.inner.role {
            RfcommRole::Listen => "rfcomm-server",
            RfcommRole::Connect => "rfcomm-client",
        }
    }

    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        Ok(Box::new(RfcommTransport {
            inner: Arc::clone(&self.inner),
            recv: RecvState::new(),
            write_sock: None,
            is_owner: false,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(role: RfcommRole) -> RfcommFactoryConfig {
        RfcommFactoryConfig {
            service_uuid: "00000000-0000-4000-8000-000000000002".to_string(),
            peer_address: String::new(),
            channel: 0,
            connect_timeout_secs: 1,
            keepalive_ms: 0,
            require_encryption: true,
            role,
        }
    }

    #[test]
    fn open_with_invalid_service_uuid_errors() {
        let mut c = cfg(RfcommRole::Listen);
        c.service_uuid = "nope".into();
        let err = RfcommTransport::open(&c).unwrap_err().to_string();
        assert!(err.contains("service_uuid"), "{err}");
    }

    #[test]
    fn connect_role_requires_peer_address() {
        let err = RfcommTransport::open(&cfg(RfcommRole::Connect))
            .unwrap_err()
            .to_string();
        assert!(err.contains("peer_address"), "{err}");
    }

    #[test]
    fn sockaddr_roundtrip_keeps_address_and_port() {
        let addr = bth_sockaddr(0xA0B1_C2D3_E4F5, 20).unwrap();
        assert_eq!(addr.len() as usize, SOCKADDR_BTH_LEN);
        let back = sockaddr_to_bth(&addr);
        assert_eq!({ back.btAddr }, 0xA0B1_C2D3_E4F5);
        assert_eq!({ back.port }, 20);
        assert_eq!({ back.addressFamily }, AF_BTH);
    }
}
