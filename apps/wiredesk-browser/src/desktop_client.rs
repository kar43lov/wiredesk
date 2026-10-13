use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpStream};
use std::time::Duration;

#[derive(Clone)]
pub struct DesktopClient {
    token: String,
}
impl DesktopClient {
    pub(crate) fn new(token: String) -> Self {
        Self { token }
    }
    pub fn frame(&self) -> io::Result<Vec<u8>> {
        let mut stream = TcpStream::connect_timeout(
            &(Ipv4Addr::LOCALHOST, 19223).into(),
            Duration::from_millis(250),
        )?;
        stream.set_read_timeout(Some(Duration::from_secs(8)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        write!(stream,"GET /desktop/frame HTTP/1.1\r\nHost: 127.0.0.1:19223\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n",self.token)?;
        let mut response = Vec::new();
        stream
            .take(2 * 1024 * 1024 + 1)
            .read_to_end(&mut response)?;
        if response.len() > 2 * 1024 * 1024 {
            return Err(io::Error::other("desktop response too large"));
        }
        let end = response
            .windows(4)
            .position(|p| p == b"\r\n\r\n")
            .map(|p| p + 4)
            .ok_or_else(|| io::Error::other("invalid desktop response"))?;
        if response.starts_with(b"HTTP/1.1 412 ") {
            return Err(io::Error::other("Обновите хост Windows для режима экрана"));
        }
        if !response.starts_with(b"HTTP/1.1 200 ") {
            return Err(io::Error::other("Экран Windows пока недоступен"));
        }
        let headers = std::str::from_utf8(&response[..end]).map_err(io::Error::other)?;
        let lengths: Vec<_> = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, v)| v.trim())
            .collect();
        if lengths.len() != 1
            || lengths[0].parse::<usize>().map_err(io::Error::other)? != response.len() - end
        {
            return Err(io::Error::other("truncated desktop image"));
        }
        Ok(response[end..].to_vec())
    }
}
