use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::net::{Shutdown, TcpListener};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use wiredesk_browser::Hub;
use wiredesk_exec_core::ipc::{
    default_socket_path, read_packet_frame, write_connect, write_packet_frame, IpcConnect,
};
use wiredesk_protocol::{
    message::{browser_supported, Message},
    packet::Packet,
};

/// GUI-owned listeners and reconnecting IPC worker, without a child process
/// or direct transport. An idle service sends no remote browser requests.
pub struct Service {
    alive: Arc<AtomicBool>,
    status: Arc<Mutex<String>>,
    control: Arc<Mutex<Option<UnixStream>>>,
    hub: Arc<Mutex<Hub>>,
    worker: Option<JoinHandle<()>>,
    listeners: Vec<JoinHandle<()>>,
    token: String,
    pretty_address: bool,
    host_version: Arc<AtomicU8>,
}

impl Service {
    pub fn start() -> io::Result<Self> {
        let path = default_socket_path();
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("missing IPC directory"))?;
        let token = load_token(dir)?;
        let cdp = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 19222))?;
        let panel = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 19223))?;
        let mut service = Self::start_with(path, token, [(cdp, 19222), (panel, 19223)])?;
        service.pretty_address = crate::web_forwarder::is_available();
        Ok(service)
    }

    fn start_with(
        path: PathBuf,
        token: String,
        sockets: [(TcpListener, u16); 2],
    ) -> io::Result<Self> {
        let mut service = Self {
            alive: Arc::new(AtomicBool::new(true)),
            status: Arc::new(Mutex::new("Ожидание подключения к Windows".to_owned())),
            control: Arc::new(Mutex::new(None)),
            hub: Arc::new(Mutex::new(Hub::default())),
            worker: None,
            listeners: Vec::new(),
            token,
            pretty_address: false,
            host_version: Arc::new(AtomicU8::new(0)),
        };
        service.listeners = crate::http::start(
            service.hub.clone(),
            service.alive.clone(),
            service.token.clone(),
            service.host_version.clone(),
            sockets,
        )?;
        let (alive, status, control, hub) = (
            service.alive.clone(),
            service.status.clone(),
            service.control.clone(),
            service.hub.clone(),
        );
        let host_version = service.host_version.clone();
        service.worker = Some(
            thread::Builder::new()
                .name("browser-service".into())
                .spawn(move || {
                    while alive.load(Ordering::Acquire) {
                        let result = relay(&path, &alive, &status, &control, &hub, &host_version);
                        host_version.store(0, Ordering::Release);
                        *control.lock().unwrap() = None;
                        hub.lock().unwrap().reset();
                        if !alive.load(Ordering::Acquire) {
                            break;
                        }
                        *status.lock().unwrap() = match result {
                            Ok(()) => "Ожидание переподключения к Windows".to_owned(),
                            Err(e) => format!("Ожидание подключения: {e}"),
                        };
                        thread::park_timeout(Duration::from_secs(2));
                    }
                })?,
        );
        Ok(service)
    }

    pub fn is_running(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
    pub fn status(&self) -> String {
        if !self.is_running() {
            return "Браузерный сервис остановлен. Перезапустите WireDesk.".to_owned();
        }
        self.status.lock().unwrap().clone()
    }
    /// Persistent owner-only key is passed in the fragment, never in the
    /// HTTP request or Referer. The caller must explicitly open/copy it.
    pub fn access_url(&self) -> String {
        format!("{}#token={}", self.viewer_url(), self.token)
    }
    pub fn viewer_url(&self) -> &'static str {
        if self.pretty_address {
            crate::VIEWER_URL
        } else {
            crate::VIEWER_URL_WITH_PORT
        }
    }
    pub fn desktop_client(&self) -> crate::desktop_client::DesktopClient {
        crate::desktop_client::DesktopClient::new(self.token.clone())
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Some(socket) = self.control.lock().unwrap().as_ref() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
        self.hub.lock().unwrap().reset();
        for listener in self.listeners.drain(..) {
            let _ = listener.join();
        }
    }
}

fn relay(
    path: &Path,
    alive: &AtomicBool,
    status: &Mutex<String>,
    control: &Mutex<Option<UnixStream>>,
    hub: &Mutex<Hub>,
    host_version: &AtomicU8,
) -> io::Result<()> {
    let mut ipc = UnixStream::connect(path)?;
    ipc.set_read_timeout(Some(Duration::from_secs(3)))?;
    ipc.set_write_timeout(Some(Duration::from_secs(2)))?;
    // Stop-check and control installation share the lock: Drop cannot miss
    // a socket that was still being connected and block joining the worker.
    {
        let mut socket = control.lock().unwrap();
        if !alive.load(Ordering::Acquire) {
            return Ok(());
        }
        *socket = Some(ipc.try_clone()?);
    }
    write_connect(&mut ipc, &IpcConnect::Browser)?;
    match read_packet_frame(&mut ipc)?.message {
        Message::HelloAck { version, .. } if browser_supported(version) => {
            host_version.store(version, Ordering::Release);
        }
        Message::Error { msg, .. } => return Err(io::Error::other(msg)),
        _ => return Err(io::Error::other("unexpected browser handshake")),
    }
    ipc.set_read_timeout(None)?;
    *status.lock().unwrap() = "Браузерный доступ готов".to_owned();
    let connected = Arc::new(AtomicBool::new(true));
    let reader_connected = connected.clone();
    let mut reader = ipc.try_clone()?;
    let (tx, rx) = mpsc::sync_channel(64);
    let reader = thread::Builder::new()
        .name("browser-ipc-reader".into())
        .spawn(move || {
            while let Ok(packet) = read_packet_frame(&mut reader) {
                if !matches!(packet.message, Message::Browser { .. })
                    || tx.try_send(packet.message).is_err()
                {
                    break;
                }
            }
            reader_connected.store(false, Ordering::Release);
        })?;
    while alive.load(Ordering::Acquire) && connected.load(Ordering::Acquire) {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(message) => hub.lock().unwrap().dispatch(message, false),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        while let Ok(message) = rx.try_recv() {
            hub.lock().unwrap().dispatch(message, false);
        }
        for _ in 0..16 {
            let Some(message) = hub.lock().unwrap().poll() else {
                break;
            };
            if write_packet_frame(&mut ipc, &Packet::new(message, 0)).is_err() {
                connected.store(false, Ordering::Release);
                break;
            }
        }
    }
    let _ = ipc.shutdown(Shutdown::Both);
    let _ = reader.join();
    Ok(())
}

fn load_token(dir: &Path) -> io::Result<String> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    let path = dir.join("browser-token");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            let token = uuid::Uuid::new_v4().to_string();
            file.write_all(token.as_bytes())?;
            Ok(token)
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::other(
                    "browser token must be a regular owner-only file",
                ));
            }
            let token = fs::read_to_string(path)?;
            uuid::Uuid::parse_str(&token).map_err(io::Error::other)?;
            Ok(token)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::time::Instant;
    use wiredesk_exec_core::ipc::read_connect;

    fn listeners() -> [(TcpListener, u16); 2] {
        std::array::from_fn(|index| {
            let socket = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            (socket, [19222, 19223][index])
        })
    }

    fn ack(socket: &mut UnixStream) {
        assert!(matches!(read_connect(socket).unwrap(), IpcConnect::Browser));
        write_packet_frame(
            socket,
            &Packet::new(
                Message::HelloAck {
                    version: 4,
                    host_name: "fixture".into(),
                    screen_w: 1280,
                    screen_h: 800,
                },
                0,
            ),
        )
        .unwrap();
    }

    #[test]
    fn access_key_is_stable_private_and_does_not_accept_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let key = load_token(dir.path()).unwrap();
        assert_eq!(load_token(dir.path()).unwrap(), key);
        let path = dir.path().join("browser-token");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_token(dir.path()).is_err());
        fs::remove_file(&path).unwrap();
        let other = dir.path().join("other");
        fs::write(&other, &key).unwrap();
        std::os::unix::fs::symlink(&other, &path).unwrap();
        assert!(load_token(dir.path()).is_err());
    }

    #[test]
    fn idle_service_reconnects_without_remote_requests_and_drop_releases_ports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let ipc = UnixListener::bind(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        let mock = thread::spawn(move || {
            let (mut first, _) = ipc.accept().unwrap();
            ack(&mut first);
            first
                .set_read_timeout(Some(Duration::from_millis(250)))
                .unwrap();
            let e = first.read(&mut [0u8; 1]).unwrap_err();
            assert!(matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ));
            tx.send(()).unwrap();
            first.shutdown(Shutdown::Both).unwrap();
            let (mut second, _) = ipc.accept().unwrap();
            ack(&mut second);
            tx.send(()).unwrap();
            assert_eq!(second.read(&mut [0u8; 1]).unwrap(), 0);
        });
        let sockets = listeners();
        let ports = [
            sockets[0].0.local_addr().unwrap().port(),
            sockets[1].0.local_addr().unwrap().port(),
        ];
        let service = Service::start_with(path, "test-key".into(), sockets).unwrap();
        assert_eq!(
            service.access_url(),
            format!("{}#token=test-key", crate::VIEWER_URL_WITH_PORT)
        );
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(service.is_running());
        // Serving the local panel itself creates no remote request.
        let mut panel =
            std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, ports[1])).unwrap();
        panel
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        panel
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut html = String::new();
        panel.read_to_string(&mut html).unwrap();
        assert!(html.starts_with("HTTP/1.1 200"));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(service);
        mock.join().unwrap();
        for port in ports {
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).unwrap();
        }
    }

    #[test]
    fn drop_interrupts_a_stalled_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ipc.sock");
        let ipc = UnixListener::bind(&path).unwrap();
        let (tx, rx) = mpsc::channel();
        let mock = thread::spawn(move || {
            let (mut stream, _) = ipc.accept().unwrap();
            read_connect(&mut stream).unwrap();
            tx.send(()).unwrap();
            assert_eq!(stream.read(&mut [0u8; 1]).unwrap(), 0);
        });
        let service = Service::start_with(path, "test-key".into(), listeners()).unwrap();
        rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let start = Instant::now();
        drop(service);
        assert!(start.elapsed() < Duration::from_secs(1));
        mock.join().unwrap();
    }
}
