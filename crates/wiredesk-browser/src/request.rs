use std::io::{self, Read, Write};
use std::sync::Arc;

pub type RequestHandler = Arc<dyn Fn(&[u8]) -> io::Result<Vec<u8>> + Send + Sync>;
pub const MAX_REQUEST: usize = 16 * 1024;
const MAX_RESPONSE: usize = 2 * 1024 * 1024;

/// Strict, single-request HTTP framing for the internal desktop endpoint.
pub fn request_body(raw: &[u8]) -> io::Result<Option<(&str, &[u8])>> {
    if raw.len() > MAX_REQUEST {
        return Err(io::Error::other("desktop request too large"));
    }
    let Some(end) = raw.windows(4).position(|s| s == b"\r\n\r\n").map(|p| p + 4) else {
        return Ok(None);
    };
    let header = std::str::from_utf8(&raw[..end]).map_err(io::Error::other)?;
    let mut lines = header.split("\r\n");
    let request = lines.next().unwrap_or("");
    let mut length = None;
    for line in lines.filter(|l| !l.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| io::Error::other("invalid header"))?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(io::Error::other("chunked desktop request rejected"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(io::Error::other("duplicate content length"));
            }
            length = Some(value.trim().parse::<usize>().map_err(io::Error::other)?);
        }
    }
    let total = end
        .checked_add(length.unwrap_or(0))
        .filter(|n| *n <= MAX_REQUEST)
        .ok_or_else(|| io::Error::other("desktop body too large"))?;
    if raw.len() > total {
        return Err(io::Error::other("pipelined desktop request rejected"));
    }
    if raw.len() < total {
        return Ok(None);
    }
    Ok(Some((request, &raw[end..total])))
}

/// A nonblocking, memory-only socket. Its handler runs on the stream worker,
/// never in the host link loop. At most MAX_STREAMS workers exist.
pub(crate) struct RequestSocket {
    handler: RequestHandler,
    request: Vec<u8>,
    response: Option<std::io::Cursor<Vec<u8>>>,
}
impl RequestSocket {
    pub fn new(handler: RequestHandler) -> Self {
        Self {
            handler,
            request: Vec::new(),
            response: None,
        }
    }
    pub fn shutdown_write(&self) -> io::Result<()> {
        if self.response.is_some() {
            Ok(())
        } else {
            Err(io::ErrorKind::UnexpectedEof.into())
        }
    }
}
impl Write for RequestSocket {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.response.is_some() || self.request.len() + data.len() > MAX_REQUEST {
            return Err(io::Error::other("invalid desktop request size/state"));
        }
        self.request.extend_from_slice(data);
        if request_body(&self.request)?.is_some() {
            let response = (self.handler)(&self.request)?;
            if response.len() > MAX_RESPONSE {
                return Err(io::Error::other("desktop response too large"));
            }
            self.response = Some(std::io::Cursor::new(response));
            self.request.clear();
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Read for RequestSocket {
    fn read(&mut self, data: &mut [u8]) -> io::Result<usize> {
        match &mut self.response {
            Some(response) => response.read(data),
            None => Err(io::ErrorKind::WouldBlock.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_body_is_processed_once_and_ambiguous_framing_is_rejected() {
        let mut s = RequestSocket::new(Arc::new(|r| {
            assert_eq!(request_body(r)?.unwrap().1, b"{}");
            Ok(b"response".to_vec())
        }));
        s.write_all(b"POST /desktop/input HTTP/1.1\r\nContent-Length: 2\r\n\r\n{")
            .unwrap();
        assert_eq!(
            s.read(&mut [0; 8]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        s.write_all(b"}").unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"response");
        assert!(s.write_all(b"x").is_err());
        for r in [
            "GET / HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
            "POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\nextra",
        ] {
            assert!(request_body(r.as_bytes()).is_err());
        }
    }
}
