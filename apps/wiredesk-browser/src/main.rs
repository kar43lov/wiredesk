#[cfg(any(target_os = "macos", test))]
mod http;

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = run() {
        eprintln!("wd-browser: {e}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("wd-browser currently requires the macOS WireDesk GUI IPC relay");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn run() -> std::io::Result<()> {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;
    use std::time::Duration;
    use wiredesk_exec_core::ipc::{
        default_socket_path, read_packet_frame, write_connect, write_packet_frame, IpcConnect,
    };
    use wiredesk_protocol::{message::Message, packet::Packet};

    let mut ipc = UnixStream::connect(default_socket_path())?;
    ipc.set_read_timeout(Some(Duration::from_secs(3)))?;
    ipc.set_write_timeout(Some(Duration::from_secs(2)))?;
    write_connect(&mut ipc, &IpcConnect::Browser)?;
    match read_packet_frame(&mut ipc)?.message {
        Message::HelloAck { version, .. }
            if wiredesk_protocol::message::browser_supported(version) => {}
        Message::Error { msg, .. } => return Err(std::io::Error::other(msg)),
        _ => return Err(std::io::Error::other("unexpected browser handshake")),
    }
    ipc.set_read_timeout(None)?;
    let dir = default_socket_path().parent().unwrap().to_path_buf();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
    let path = dir.join("browser-token");
    let token = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            let token = uuid::Uuid::new_v4().to_string();
            file.write_all(token.as_bytes())?;
            token
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
                return Err(std::io::Error::other(
                    "browser token must be a regular owner-only file",
                ));
            }
            let token = fs::read_to_string(&path)?;
            uuid::Uuid::parse_str(&token).map_err(std::io::Error::other)?;
            token
        }
        Err(e) => return Err(e),
    };
    let hub = Arc::new(Mutex::new(wiredesk_browser::Hub::default()));
    let alive = Arc::new(AtomicBool::new(true));
    http::start(hub.clone(), alive.clone(), token)?;
    eprintln!(
        "Viewer: http://127.0.0.1:19223/\nCDP: http://127.0.0.1:19222/\nToken file: {}",
        path.display()
    );
    let (tx, rx) = mpsc::sync_channel(64);
    let mut reader = ipc.try_clone()?;
    let reader_alive = alive.clone();
    let reader_thread = thread::spawn(move || {
        while let Ok(packet) = read_packet_frame(&mut reader) {
            if !matches!(packet.message, Message::Browser { .. })
                || tx.try_send(packet.message).is_err()
            {
                break;
            }
        }
        reader_alive.store(false, Ordering::Release);
    });
    while alive.load(Ordering::Acquire) {
        while let Ok(msg) = rx.try_recv() {
            hub.lock().unwrap().dispatch(msg, false);
        }
        for _ in 0..16 {
            let msg = hub.lock().unwrap().poll();
            let Some(msg) = msg else { break };
            if write_packet_frame(&mut ipc, &Packet::new(msg, 0)).is_err() {
                alive.store(false, Ordering::Release);
                break;
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    hub.lock().unwrap().reset();
    let _ = ipc.shutdown(std::net::Shutdown::Both);
    let _ = reader_thread.join();
    Err(std::io::Error::other(
        "browser link closed; restart wd-browser after reconnect",
    ))
}
