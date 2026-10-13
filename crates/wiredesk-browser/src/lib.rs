//! Bounded reliable byte streams over browser opcodes, separate from shells.
//! Each stream has one packet in flight. Missing data/acks are retried;
//! duplicate data is never written twice. All socket I/O runs off the link
//! thread. The only host destination is IPv4 loopback, port 19222.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::MAX_PAYLOAD;

mod request;
use request::RequestSocket;
pub use request::{request_body, RequestHandler};

pub const PORT: u16 = 19222;
pub const MAX_STREAMS: usize = 8;
pub const CHUNK: usize = MAX_PAYLOAD - 13;
pub const OPEN: u8 = 1;
pub const READY: u8 = 2;
pub const DATA: u8 = 3;
pub const ACK: u8 = 4;
pub const FIN: u8 = 5;
pub const RESET: u8 = 6;
pub const FIN_ACK: u8 = 7;
pub const OPEN_DESKTOP: u8 = 8;
const RETRY: Duration = Duration::from_secs(2);
const STALL: Duration = Duration::from_secs(30);

pub fn frame(stream: u32, kind: u8, offset: u64, data: Vec<u8>) -> Message {
    Message::Browser {
        stream,
        kind,
        offset,
        data,
    }
}

struct Endpoint {
    input: mpsc::SyncSender<Message>,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

/// No I/O or blocking sends in dispatch/poll, so shell ticks keep running.
pub struct Hub {
    streams: BTreeMap<u32, Endpoint>,
    output: mpsc::Receiver<(u64, Message)>,
    sender: mpsc::SyncSender<(u64, Message)>,
    epoch: u64,
    target: SocketAddr,
    credit: Arc<AtomicUsize>,
    desktop: Option<RequestHandler>,
}

impl Default for Hub {
    fn default() -> Self {
        let (sender, output) = mpsc::sync_channel(32);
        Self {
            streams: BTreeMap::new(),
            output,
            sender,
            epoch: 0,
            target: SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)),
            credit: Arc::new(AtomicUsize::new(0)),
            desktop: None,
        }
    }
}

impl Hub {
    /// In-process endpoint only: desktop access never opens a host TCP port.
    pub fn with_desktop(handler: RequestHandler) -> Self {
        let mut hub = Self::default();
        hub.desktop = Some(handler);
        hub
    }
    pub fn reset(&mut self) {
        for endpoint in self.streams.values() {
            endpoint.stop.store(true, Ordering::Release);
        }
        self.streams.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.credit = Arc::new(AtomicUsize::new(0));
        while self.output.try_recv().is_ok() {}
    }

    pub fn active(&mut self) -> usize {
        self.streams.retain(|_, e| !e.done.load(Ordering::Acquire));
        self.streams.len()
    }

    /// Local accepted connection. Caller allocates nonzero ids without reuse.
    pub fn attach(&mut self, id: u32, stream: TcpStream, initial: Vec<u8>) -> bool {
        if id == 0 || self.active() >= MAX_STREAMS || self.streams.contains_key(&id) {
            return false;
        }
        self.spawn(id, Some(stream), initial, OPEN);
        true
    }

    pub fn attach_desktop(&mut self, id: u32, stream: TcpStream, initial: Vec<u8>) -> bool {
        if id == 0 || self.active() >= MAX_STREAMS || self.streams.contains_key(&id) {
            return false;
        }
        self.spawn(id, Some(stream), initial, OPEN_DESKTOP);
        true
    }

    fn spawn(&mut self, id: u32, socket: Option<TcpStream>, initial: Vec<u8>, open_kind: u8) {
        let (input, rx) = mpsc::sync_channel(16);
        let stop = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let epoch = self.epoch;
        let tx = self.sender.clone();
        let source = match socket {
            Some(socket) => SocketSource::Accepted(socket),
            None if open_kind == OPEN_DESKTOP => {
                let Some(handler) = self.desktop.clone() else {
                    let _ = tx.try_send((epoch, frame(id, RESET, 0, vec![])));
                    return;
                };
                SocketSource::Request(RequestSocket::new(handler))
            }
            None => SocketSource::Connect(self.target),
        };
        let link = WorkerLink {
            output: tx,
            epoch,
            stop: stop.clone(),
            credit: self.credit.clone(),
        };
        self.streams.insert(
            id,
            Endpoint {
                input,
                stop: stop.clone(),
                done: done.clone(),
            },
        );
        thread::spawn(move || {
            if let Err(e) = worker(id, source, initial, &rx, &link, open_kind) {
                log::debug!("browser stream {id}: {e}");
                let _ = link.output.try_send((epoch, frame(id, RESET, 0, vec![])));
            }
            done.store(true, Ordering::Release);
        });
    }

    pub fn dispatch(&mut self, msg: Message, host: bool) {
        let Message::Browser { stream, kind, .. } = &msg else {
            return;
        };
        let id = *stream;
        let kind = *kind;
        if id == 0 {
            if kind == RESET {
                self.reset();
            }
            return;
        }
        if host && matches!(kind, OPEN | OPEN_DESKTOP) {
            if let Some(e) = self.streams.get(&id) {
                let _ = e.input.try_send(msg);
            } else if self.active() < MAX_STREAMS {
                self.spawn(id, None, vec![], kind);
            } else {
                let _ = self
                    .sender
                    .try_send((self.epoch, frame(id, RESET, 0, vec![])));
            }
            return;
        }
        if let Some(e) = self.streams.get(&id) {
            if e.input.try_send(msg).is_err() {
                e.stop.store(true, Ordering::Release);
                let _ = self
                    .sender
                    .try_send((self.epoch, frame(id, RESET, 0, vec![])));
            }
        } else if kind != RESET {
            let _ = self
                .sender
                .try_send((self.epoch, frame(id, RESET, 0, vec![])));
        }
    }

    pub fn poll(&mut self) -> Option<Message> {
        self.active();
        for _ in 0..32 {
            let (epoch, msg) = self.output.try_recv().ok()?;
            if epoch == self.epoch {
                return Some(msg);
            }
        }
        None
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.reset();
    }
}

/// Receiver rejects gaps and suppresses duplicates before writing to TCP.
#[derive(Default)]
struct Receive {
    offset: u64,
    pending: Option<(Vec<u8>, usize)>,
}

impl Receive {
    fn data(&mut self, offset: u64, data: Vec<u8>) -> io::Result<bool> {
        if data.len() > CHUNK || data.is_empty() {
            return Err(io::Error::other("invalid browser data size"));
        }
        if offset != self.offset {
            return Ok(false);
        }
        if let Some((old, _)) = &self.pending {
            if old != &data {
                return Err(io::Error::other("changed retransmission"));
            }
        } else {
            self.pending = Some((data, 0));
        }
        Ok(true)
    }
}

enum SocketSource {
    Accepted(TcpStream),
    Connect(SocketAddr),
    Request(RequestSocket),
}

trait WireSocket: Read + Write {
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
}
impl WireSocket for TcpStream {
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        TcpStream::shutdown(self, how)
    }
}
impl WireSocket for RequestSocket {
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        if matches!(how, Shutdown::Write) {
            self.shutdown_write()
        } else {
            Ok(())
        }
    }
}

struct WorkerLink {
    output: mpsc::SyncSender<(u64, Message)>,
    epoch: u64,
    stop: Arc<AtomicBool>,
    credit: Arc<AtomicUsize>,
}

/// Across ALL browser connections, at most two chunks await acks. Credits
/// are released by ACK or RAII on disconnect, including socket errors.
struct Credit(Arc<AtomicUsize>);
impl Credit {
    fn claim(budget: &Arc<AtomicUsize>) -> Option<Self> {
        budget
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < 2).then_some(used + 1)
            })
            .ok()
            .map(|_| Self(budget.clone()))
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn worker(
    id: u32,
    socket: SocketSource,
    initial: Vec<u8>,
    input: &mpsc::Receiver<Message>,
    link: &WorkerLink,
    open_kind: u8,
) -> io::Result<()> {
    let output = &link.output;
    let epoch = link.epoch;
    let stop = &link.stop;
    let host = !matches!(socket, SocketSource::Accepted(_));
    let mut socket: Box<dyn WireSocket> = match socket {
        SocketSource::Accepted(s) => {
            s.set_nonblocking(true)?;
            s.set_nodelay(true)?;
            Box::new(s)
        }
        SocketSource::Connect(target) => {
            let s = TcpStream::connect_timeout(&target, Duration::from_millis(500))?;
            s.set_nonblocking(true)?;
            s.set_nodelay(true)?;
            Box::new(s)
        }
        SocketSource::Request(s) => Box::new(s),
    };
    let mut ready = host;
    let mut queued = VecDeque::from([frame(id, if host { READY } else { open_kind }, 0, vec![])]);
    let mut initial = initial.into_iter();
    let mut send_offset = 0u64;
    let mut flight: Option<(Vec<u8>, Instant, Credit)> = None;
    let mut receive = Receive::default();
    let mut local_eof = false;
    let mut remote_eof = false;
    let mut fin_acked = false;
    let mut fin_sent: Option<Instant> = None;
    let mut progress = Instant::now();
    let mut open_sent = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        // Never block waiting on the link. Queue stays bounded; stop reading
        // sockets/input when the link cannot drain it.
        while let Some(msg) = queued.pop_front() {
            match output.try_send((epoch, msg)) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full((_, msg))) => {
                    queued.push_front(msg);
                    break;
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return Ok(()),
            }
        }
        if queued.is_empty() {
            for _ in 0..16 {
                let msg = match input.try_recv() {
                    Ok(m) => m,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
                };
                let Message::Browser {
                    kind, offset, data, ..
                } = msg
                else {
                    continue;
                };
                match kind {
                    RESET => return Ok(()),
                    OPEN | OPEN_DESKTOP if host => queued.push_back(frame(id, READY, 0, vec![])),
                    READY if !host => {
                        ready = true;
                        progress = Instant::now();
                    }
                    DATA if ready && !remote_eof => {
                        if !receive.data(offset, data)? {
                            queued.push_back(frame(id, ACK, receive.offset, vec![]));
                        }
                    }
                    ACK => {
                        if offset > send_offset {
                            return Err(io::Error::other("ack beyond sent bytes"));
                        }
                        if flight
                            .as_ref()
                            .is_some_and(|(data, _, _)| offset == send_offset && !data.is_empty())
                        {
                            flight = None;
                            progress = Instant::now();
                        }
                    }
                    FIN if offset == receive.offset && receive.pending.is_none() => {
                        if !remote_eof {
                            socket.shutdown(Shutdown::Write)?;
                            remote_eof = true;
                        }
                        queued.push_back(frame(id, FIN_ACK, offset, vec![]));
                    }
                    FIN => queued.push_back(frame(id, ACK, receive.offset, vec![])),
                    FIN_ACK if fin_sent.is_some() && offset == send_offset => {
                        fin_acked = true;
                        progress = Instant::now();
                    }
                    _ => {}
                }
                if queued.len() >= 4 {
                    break;
                }
            }
        }
        if queued.is_empty() {
            if let Some((data, written)) = &mut receive.pending {
                match socket.write(&data[*written..]) {
                    Ok(0) => return Err(io::Error::other("TCP write zero")),
                    Ok(n) => {
                        *written += n;
                        if *written == data.len() {
                            receive.offset = receive
                                .offset
                                .checked_add(data.len() as u64)
                                .ok_or_else(|| io::Error::other("offset overflow"))?;
                            receive.pending = None;
                            progress = Instant::now();
                            queued.push_back(frame(id, ACK, receive.offset, vec![]));
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e),
                }
            }
            if ready && flight.is_none() && !local_eof {
                if let Some(credit) = Credit::claim(&link.credit) {
                    let mut data: Vec<u8> = initial.by_ref().take(CHUNK).collect();
                    if data.is_empty() {
                        let mut buffer = vec![0; CHUNK];
                        match socket.read(&mut buffer) {
                            Ok(0) => local_eof = true,
                            Ok(n) => {
                                buffer.truncate(n);
                                data = buffer;
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                            Err(e) => return Err(e),
                        }
                    }
                    if !data.is_empty() {
                        queued.push_back(frame(id, DATA, send_offset, data.clone()));
                        send_offset = send_offset
                            .checked_add(data.len() as u64)
                            .ok_or_else(|| io::Error::other("offset overflow"))?;
                        flight = Some((data, Instant::now(), credit));
                        progress = Instant::now();
                    }
                }
            }
            if let Some((data, sent, _)) = &mut flight {
                if sent.elapsed() >= RETRY {
                    queued.push_back(frame(
                        id,
                        DATA,
                        send_offset - data.len() as u64,
                        data.clone(),
                    ));
                    *sent = Instant::now();
                }
            } else if local_eof && !fin_acked && fin_sent.is_none_or(|t| t.elapsed() >= RETRY) {
                queued.push_back(frame(id, FIN, send_offset, vec![]));
                fin_sent = Some(Instant::now());
            }
            if !ready && open_sent.elapsed() >= RETRY {
                queued.push_back(frame(id, open_kind, 0, vec![]));
                open_sent = Instant::now();
            }
        }
        if local_eof && remote_eof && fin_acked && queued.is_empty() {
            break;
        }
        if (!ready || flight.is_some() || receive.pending.is_some() || (local_eof && !fin_acked))
            && progress.elapsed() >= STALL
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "browser stream stalled",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
    let _ = socket.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn desktop_stream_uses_memory_endpoint_and_recovers_a_lost_open() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut host = Hub::with_desktop(Arc::new(|raw| {
            assert_eq!(request_body(raw)?.unwrap().0, "GET /desktop/frame HTTP/1.1");
            Ok(vec![7; 64000])
        }));
        let mut client = Hub::default();
        let address = listener.local_addr().unwrap();
        let user = thread::spawn(move || {
            let mut s = TcpStream::connect(address).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
            s.write_all(b"GET /desktop/frame HTTP/1.1\r\n\r\n").unwrap();
            s.shutdown(Shutdown::Write).unwrap();
            let mut bytes = Vec::new();
            s.read_to_end(&mut bytes).unwrap();
            assert_eq!(bytes, vec![7; 64000]);
        });
        assert!(client.attach_desktop(1, listener.accept().unwrap().0, vec![]));
        let start = Instant::now();
        let mut lost = false;
        while !user.is_finished() && start.elapsed() < Duration::from_secs(7) {
            for _ in 0..16 {
                if let Some(m) = client.poll() {
                    if matches!(
                        m,
                        Message::Browser {
                            kind: OPEN_DESKTOP,
                            ..
                        }
                    ) && !lost
                    {
                        lost = true;
                    } else {
                        host.dispatch(m, true);
                    }
                }
                if let Some(m) = host.poll() {
                    client.dispatch(m, false);
                }
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(lost);
        assert!(user.is_finished());
        user.join().unwrap();
        assert_eq!(host.target, SocketAddr::from((Ipv4Addr::LOCALHOST, PORT)));
    }

    #[test]
    fn incomplete_desktop_request_releases_stream_after_peer_eof() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut user = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        user.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut host = Hub::with_desktop(Arc::new(|_| panic!("incomplete request must not run")));
        let mut client = Hub::default();
        let request = b"POST /desktop/input HTTP/1.1\r\nContent-Length: 20\r\n\r\n{";
        assert!(client.attach_desktop(1, listener.accept().unwrap().0, request.to_vec()));
        user.shutdown(Shutdown::Write).unwrap();
        let start = Instant::now();
        let mut opened = false;
        while start.elapsed() < Duration::from_secs(1) {
            if let Some(m) = client.poll() {
                opened = true;
                host.dispatch(m, true);
            }
            if let Some(m) = host.poll() {
                client.dispatch(m, false);
            }
            if opened && host.active() == 0 && client.active() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(opened);
        assert_eq!(host.active(), 0);
        assert_eq!(client.active(), 0);
        match user.read(&mut [0u8; 1]) {
            Ok(0) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {}
            result => panic!("closed desktop stream expected, got {result:?}"),
        }
    }

    #[test]
    fn bidirectional_tcp_recovers_lost_packets_and_acks_without_duplicates() {
        let destination = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let local = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut host = Hub::default();
        host.target = destination.local_addr().unwrap();
        let mut client = Hub::default();
        let payload: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
        let expected = payload.clone();
        let server = thread::spawn(move || {
            let (mut socket, _) = destination.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            let mut received = Vec::new();
            socket.read_to_end(&mut received).unwrap();
            assert_eq!(received, expected);
            socket.write_all(&received).unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
        });
        let local_address = local.local_addr().unwrap();
        let user = thread::spawn(move || {
            let mut socket = TcpStream::connect(local_address).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(8)))
                .unwrap();
            socket.write_all(&payload).unwrap();
            socket.shutdown(Shutdown::Write).unwrap();
            let mut received = Vec::new();
            socket.read_to_end(&mut received).unwrap();
            assert_eq!(received, payload);
        });
        let (accepted, _) = local.accept().unwrap();
        assert!(client.attach(1, accepted, vec![]));
        let start = Instant::now();
        let mut dropped_data = false;
        let mut dropped_ack = false;
        while !user.is_finished() && start.elapsed() < Duration::from_secs(7) {
            for _ in 0..16 {
                if let Some(msg) = client.poll() {
                    if matches!(msg, Message::Browser { kind: DATA, .. }) && !dropped_data {
                        dropped_data = true;
                    } else {
                        host.dispatch(msg, true);
                    }
                }
                if let Some(msg) = host.poll() {
                    if matches!(msg, Message::Browser { kind: ACK, .. }) && !dropped_ack {
                        dropped_ack = true;
                    } else {
                        if matches!(msg, Message::Browser { kind: DATA, .. }) {
                            client.dispatch(msg.clone(), false);
                        }
                        client.dispatch(msg, false);
                    }
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(user.is_finished(), "transfer stalled");
        assert!(dropped_data && dropped_ack);
        user.join().unwrap();
        server.join().unwrap();
    }

    #[test]
    fn gap_and_duplicate_do_not_write_to_tcp_twice() {
        let mut r = Receive::default();
        assert!(!r.data(10, vec![1]).unwrap());
        assert!(r.pending.is_none());
        assert!(r.data(0, vec![1, 2]).unwrap());
        assert!(r.data(0, vec![1, 2]).unwrap());
        assert!(r.data(0, vec![2, 1]).is_err());
        r.pending = None;
        r.offset = 2;
        assert!(!r.data(0, vec![1, 2]).unwrap());
        assert!(r.pending.is_none());
        assert!(r.data(2, vec![3]).unwrap());
    }

    #[test]
    fn aggregate_credit_is_released_on_error_or_ack() {
        let budget = Arc::new(AtomicUsize::new(0));
        let first = Credit::claim(&budget).unwrap();
        let second = Credit::claim(&budget).unwrap();
        assert!(Credit::claim(&budget).is_none());
        drop(first);
        let third = Credit::claim(&budget).unwrap();
        drop(second);
        drop(third);
        assert_eq!(budget.load(Ordering::Acquire), 0);
    }

    #[test]
    fn resetting_hub_filters_old_worker_output() {
        let mut hub = Hub::default();
        let old = hub.epoch;
        hub.reset();
        hub.sender.send((old, frame(1, DATA, 0, vec![1]))).unwrap();
        hub.sender
            .send((hub.epoch, frame(2, ACK, 0, vec![])))
            .unwrap();
        assert!(matches!(
            hub.poll(),
            Some(Message::Browser { stream: 2, .. })
        ));
        assert!(hub.poll().is_none());
    }
}
