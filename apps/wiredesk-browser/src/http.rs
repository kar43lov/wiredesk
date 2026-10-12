//! Authenticated loopback CDP gateway. Never exposes a general TCP forward.
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use wiredesk_browser::Hub;

const MAX_HEADER: usize = 16 * 1024;
const PANEL: &str = include_str!("panel.html");

fn header(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    read_header(stream)
}

fn read_header<R: Read>(stream: &mut R) -> io::Result<Vec<u8>> {
    let mut data = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let n = stream.read(&mut buffer)?;
        if n == 0 {
            return Err(io::Error::other("HTTP header EOF"));
        }
        data.extend_from_slice(&buffer[..n]);
        if data.len() > MAX_HEADER {
            return Err(io::Error::other("HTTP header too large"));
        }
        if data.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(data);
        }
    }
}

/// Token is accepted as Bearer header (Playwright) or query parameter
/// (browser WebSocket). Remove it before forwarding to the host. Validate
/// Host and Origin to reject DNS rebinding and cross-site WebSocket access.
fn authenticate(raw: &[u8], token: &str, port: u16) -> io::Result<Vec<u8>> {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("missing header end"))?
        + 4;
    let text = std::str::from_utf8(&raw[..end]).map_err(io::Error::other)?;
    let mut lines = text.split("\r\n");
    let request = lines.next().unwrap_or("");
    let parts: Vec<_> = request.split_whitespace().collect();
    if parts.len() != 3 || !parts[1].starts_with('/') || parts[2] != "HTTP/1.1" {
        return Err(io::Error::other("invalid request line"));
    }
    let (path, query) = parts[1].split_once('?').unwrap_or((parts[1], ""));
    let mut authorized = false;
    let mut remaining = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        if let Some(value) = pair.strip_prefix("token=") {
            if authorized || value != token {
                return Err(io::Error::other("invalid token"));
            }
            authorized = true;
        } else {
            remaining.push(pair);
        }
    }
    let path = if remaining.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{}", remaining.join("&"))
    };
    let mut forwarded = format!("{} {path} HTTP/1.1\r\n", parts[0]);
    let mut hosts = 0;
    let mut auth_headers = 0;
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("invalid header"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") {
            hosts += 1;
            if value != format!("127.0.0.1:{port}") && value != format!("localhost:{port}") {
                return Err(io::Error::other("invalid Host"));
            }
        }
        if name.eq_ignore_ascii_case("authorization") {
            auth_headers += 1;
            if auth_headers > 1 || value != format!("Bearer {token}") {
                return Err(io::Error::other("invalid authorization"));
            }
            authorized = true;
            continue;
        }
        if name.eq_ignore_ascii_case("origin") {
            if value != "http://127.0.0.1:19223" && value != "http://localhost:19223" {
                return Err(io::Error::other("invalid Origin"));
            }
            // Chrome need not enable remote-allow-origins. Our token check
            // and allowlisted local viewer origin have already authorized it.
            continue;
        }
        forwarded.push_str(line);
        forwarded.push_str("\r\n");
    }
    if hosts != 1 || !authorized {
        return Err(io::Error::other("unauthorized"));
    }
    forwarded.push_str("\r\n");
    let mut result = forwarded.into_bytes();
    result.extend_from_slice(&raw[end..]);
    Ok(result)
}

fn reply(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) -> io::Result<()> {
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n", body.len())?;
    stream.write_all(body)
}

fn targets(token: &str) -> io::Result<Vec<u8>> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, 19222))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    write!(stream, "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1:19222\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n")?;
    read_discovery(&mut stream)
}

fn read_discovery<R: Read>(stream: &mut R) -> io::Result<Vec<u8>> {
    // Chrome's DevTools HTTP server keeps sockets open even when asked for
    // Connection: close. EOF is not the response boundary: Content-Length is.
    let raw = read_header(stream)?;
    if !raw.starts_with(b"HTTP/1.1 200") {
        return Err(io::Error::other("CDP discovery failed"));
    }
    let start = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("invalid CDP response"))?
        + 4;
    let headers = std::str::from_utf8(&raw[..start]).map_err(io::Error::other)?;
    let lengths: Vec<_> = headers
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then_some(value.trim())
        })
        .collect();
    if lengths.len() != 1
        || headers.lines().any(|line| {
            line.split_once(':')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
        })
    {
        return Err(io::Error::other("invalid CDP response framing"));
    }
    let length: usize = lengths[0].parse().map_err(io::Error::other)?;
    if length > 2 * 1024 * 1024 || raw.len() - start > length {
        return Err(io::Error::other("CDP response too large or malformed"));
    }
    let mut body = raw[start..].to_vec();
    let received = body.len();
    body.resize(length, 0);
    stream.read_exact(&mut body[received..])?;
    let value: serde_json::Value = serde_json::from_slice(&body).map_err(io::Error::other)?;
    serde_json::to_vec(&value).map_err(io::Error::other)
}

/// Listeners are acquired before starting either thread: partial startup
/// must not leave one port alive after a bind failure.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn start(hub: Arc<Mutex<Hub>>, alive: Arc<AtomicBool>, token: String) -> io::Result<()> {
    let cdp = TcpListener::bind((Ipv4Addr::LOCALHOST, 19222))?;
    let panel = TcpListener::bind((Ipv4Addr::LOCALHOST, 19223))?;
    cdp.set_nonblocking(true)?;
    panel.set_nonblocking(true)?;
    let ids = Arc::new(AtomicU32::new(1));
    let active_headers = Arc::new(AtomicUsize::new(0));
    for (listener, port) in [(cdp, 19222), (panel, 19223)] {
        let hub = hub.clone();
        let alive = alive.clone();
        let token = token.clone();
        let ids = ids.clone();
        let active_headers = active_headers.clone();
        thread::spawn(move || {
            while alive.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if active_headers.fetch_add(1, Ordering::AcqRel) >= 16 {
                            active_headers.fetch_sub(1, Ordering::AcqRel);
                            continue;
                        }
                        let hub = hub.clone();
                        let alive = alive.clone();
                        let token = token.clone();
                        let ids = ids.clone();
                        let active_headers = active_headers.clone();
                        thread::spawn(move || {
                            let result = header(&mut stream).and_then(|raw| {
                                if port == 19223 && raw.starts_with(b"GET / HTTP/1.1\r\n") {
                                    return reply(
                                        &mut stream,
                                        "200 OK",
                                        "text/html; charset=utf-8",
                                        PANEL.as_bytes(),
                                    );
                                }
                                let initial = authenticate(&raw, &token, port)?;
                                if !alive.load(Ordering::Acquire) {
                                    return Ok(());
                                }
                                if port == 19223 {
                                    if !initial.starts_with(b"GET /targets HTTP/1.1\r\n") {
                                        return Err(io::Error::other("unknown viewer path"));
                                    }
                                    let body = targets(&token)?;
                                    reply(&mut stream, "200 OK", "application/json", &body)
                                } else {
                                    let id = ids.fetch_add(1, Ordering::Relaxed);
                                    if id == 0 {
                                        return Err(io::Error::other("stream id exhausted"));
                                    }
                                    // Timeout settings are superseded by the
                                    // hub's nonblocking socket worker.
                                    if !hub.lock().unwrap().attach(id, stream.try_clone()?, initial)
                                    {
                                        return Err(io::Error::other(
                                            "browser connection limit reached",
                                        ));
                                    }
                                    Ok(())
                                }
                            });
                            if result.is_err() {
                                let _ = reply(
                                    &mut stream,
                                    "403 Forbidden",
                                    "text/plain",
                                    b"Browser request rejected",
                                );
                            }
                            active_headers.fetch_sub(1, Ordering::AcqRel);
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => {
                        alive.store(false, Ordering::Release);
                        break;
                    }
                }
            }
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_uses_content_length_without_waiting_for_eof() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let (done, completed) = std::sync::mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]")
                .unwrap();
            // Keep the connection OPEN until the client parsed the body.
            completed.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let mut client = TcpStream::connect(addr).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        assert_eq!(read_discovery(&mut client).unwrap(), b"[]");
        done.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn discovery_rejects_ambiguous_oversize_and_truncated_responses() {
        for response in [
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n[]",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        ] {
            assert!(read_discovery(&mut std::io::Cursor::new(response.as_bytes())).is_err());
        }
    }
    #[test]
    fn token_never_reaches_remote_chrome() {
        let request = b"GET /devtools/page/1?token=secret HTTP/1.1\r\nHost: 127.0.0.1:19222\r\nOrigin: http://127.0.0.1:19223\r\n\r\n";
        let clean = authenticate(request, "secret", 19222).unwrap();
        assert_eq!(
            clean,
            b"GET /devtools/page/1 HTTP/1.1\r\nHost: 127.0.0.1:19222\r\n\r\n"
        );
        let request = b"GET /json/version HTTP/1.1\r\nHost: localhost:19222\r\nAuthorization: Bearer secret\r\n\r\n";
        assert!(
            !String::from_utf8(authenticate(request, "secret", 19222).unwrap())
                .unwrap()
                .contains("secret")
        );
    }
    #[test]
    fn rejects_unauthorized_and_rebinding() {
        for request in [
            "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1:19222\r\n\r\n",
            "GET /?token=wrong HTTP/1.1\r\nHost: 127.0.0.1:19222\r\n\r\n",
            "GET /?token=secret HTTP/1.1\r\nHost: evil.example:19222\r\n\r\n",
            "GET /?token=secret HTTP/1.1\r\nHost: 127.0.0.1:19222\r\nOrigin: https://evil.example\r\n\r\n",
        ] { assert!(authenticate(request.as_bytes(), "secret", 19222).is_err()); }
    }
}
