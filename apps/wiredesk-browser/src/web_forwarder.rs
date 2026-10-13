//! A launchd-owned loopback socket, forwarded by an unprivileged process.
//! Destination and accepted authority are fixed; no credentials or files.
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(target_os = "macos")]
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const HEALTH: &[u8] = b"wiredesk-web-forwarder-v1";

pub fn is_available() -> bool {
    let result = (|| -> io::Result<bool> {
        let mut socket =
            TcpStream::connect_timeout(&([127, 0, 0, 1], 80).into(), Duration::from_millis(150))?;
        socket.set_read_timeout(Some(Duration::from_secs(2)))?;
        socket.set_write_timeout(Some(Duration::from_secs(1)))?;
        socket.write_all(b"GET /__wiredesk__/health HTTP/1.1\r\nHost: wiredesk.localhost\r\nConnection: close\r\n\r\n")?;
        let mut response = Vec::new();
        socket.take(1024).read_to_end(&mut response)?;
        Ok(response.starts_with(b"HTTP/1.1 200 ") && response.ends_with(HEALTH))
    })();
    result.unwrap_or(false)
}

#[cfg(target_os = "macos")]
pub fn run_launchd() -> io::Result<()> {
    use std::ffi::{c_char, c_int, c_void};
    use std::os::fd::FromRawFd;
    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const c_char,
            fds: *mut *mut c_int,
            count: *mut usize,
        ) -> c_int;
        fn free(ptr: *mut c_void);
    }
    let mut fds = std::ptr::null_mut();
    let mut count = 0;
    // launchd binds port 80 as root and passes this descriptor to nobody.
    // https://developer.apple.com/documentation/xpc/launch_activate_socket
    let error = unsafe { launch_activate_socket(c"HttpListener".as_ptr(), &mut fds, &mut count) };
    if error != 0 {
        return Err(io::Error::from_raw_os_error(error));
    }
    if fds.is_null() {
        return Err(io::Error::other("launchd returned no socket"));
    }
    let sockets: Vec<_> = unsafe { std::slice::from_raw_parts(fds, count) }
        .iter()
        .map(|fd| unsafe { TcpListener::from_raw_fd(*fd) })
        .collect();
    unsafe { free(fds.cast()) };
    if sockets.len() != 1 {
        return Err(io::Error::other("expected one launchd socket"));
    }
    let socket = sockets.into_iter().next().unwrap();
    if socket.local_addr()? != ([127, 0, 0, 1], 80).into() {
        return Err(io::Error::other("helper must listen only on 127.0.0.1:80"));
    }
    socket.set_nonblocking(false)?;
    let active = Arc::new(AtomicUsize::new(0));
    for incoming in socket.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if active.fetch_add(1, Ordering::AcqRel) >= 16 {
            active.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let active = active.clone();
        thread::spawn(move || {
            let _ = serve(stream, 19223);
            active.fetch_sub(1, Ordering::AcqRel);
        });
    }
    Ok(())
}

fn response(socket: &mut TcpStream, status: &str, body: &[u8]) -> io::Result<()> {
    write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n", body.len())?;
    socket.write_all(body)
}

fn serve(mut client: TcpStream, destination_port: u16) -> io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(2)))?;
    client.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut header = Vec::new();
    let mut buffer = [0u8; 2048];
    let end = loop {
        let n = client.read(&mut buffer)?;
        if n == 0 || header.len() + n > 16 * 1024 {
            return Err(io::Error::other("invalid header size"));
        }
        header.extend_from_slice(&buffer[..n]);
        if let Some(at) = header.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let text = std::str::from_utf8(&header[..end]).map_err(io::Error::other)?;
    let mut lines = text.split("\r\n");
    let request = lines.next().unwrap_or("");
    let hosts: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .filter(|(name, _)| name.eq_ignore_ascii_case("Host"))
        .map(|(_, value)| value.trim())
        .collect();
    if hosts.len() != 1 || !matches!(hosts[0], "wiredesk.localhost" | "wiredesk.localhost:80") {
        return response(&mut client, "403 Forbidden", b"Invalid browser address");
    }
    if request == "GET /__wiredesk__/health HTTP/1.1" {
        return response(&mut client, "200 OK", HEALTH);
    }
    let mut target = match TcpStream::connect_timeout(
        &([127, 0, 0, 1], destination_port).into(),
        Duration::from_millis(500),
    ) {
        Ok(socket) => socket,
        Err(_) => {
            return response(
                &mut client,
                "503 Service Unavailable",
                b"Start WireDesk on this Mac",
            )
        }
    };
    // An idle viewer WebSocket must remain usable after a long pause.
    client.set_read_timeout(None)?;
    target.set_read_timeout(None)?;
    target.set_write_timeout(Some(Duration::from_secs(2)))?;
    target.write_all(&header)?;
    let mut incoming = client.try_clone()?;
    let mut outgoing = target.try_clone()?;
    let upload = thread::spawn(move || {
        let result = io::copy(&mut incoming, &mut outgoing);
        let _ = outgoing.shutdown(Shutdown::Write);
        result
    });
    let result = io::copy(&mut target, &mut client);
    let _ = client.shutdown(Shutdown::Both);
    let _ = target.shutdown(Shutdown::Both);
    let _ = upload.join();
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    fn request(raw: &[u8], port: u16) -> Vec<u8> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let task = thread::spawn(move || {
            serve(listener.accept().unwrap().0, port).unwrap();
        });
        let mut socket = TcpStream::connect(addr).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket.write_all(raw).unwrap();
        socket.shutdown(Shutdown::Write).unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).unwrap();
        task.join().unwrap();
        bytes
    }
    #[test]
    fn health_and_invalid_authorities_do_not_contact_backend() {
        let health = request(
            b"GET /__wiredesk__/health HTTP/1.1\r\nHost: wiredesk.localhost\r\n\r\n",
            0,
        );
        assert!(health.ends_with(HEALTH));
        for host in [
            "evil.example",
            "wiredesk.localhost.evil",
            "wiredesk.localhost:19223",
        ] {
            let raw = format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n");
            assert!(request(raw.as_bytes(), 0).starts_with(b"HTTP/1.1 403"));
        }
    }
    #[test]
    fn preserves_authorization_and_response_bytes_to_fixed_loopback_backend() {
        let backend = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = backend.local_addr().unwrap().port();
        let expected=b"GET /targets HTTP/1.1\r\nHost: wiredesk.localhost\r\nAuthorization: Bearer fixture\r\n\r\n";
        let worker = thread::spawn(move || {
            let (mut s, _) = backend.accept().unwrap();
            let mut body = Vec::new();
            s.read_to_end(&mut body).unwrap();
            assert_eq!(body, expected);
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]")
                .unwrap();
        });
        assert_eq!(
            request(expected, port),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]"
        );
        worker.join().unwrap();
    }
}
