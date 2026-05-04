//! Unix-socket IPC protocol between the GUI's IPC handler and the
//! `wd --exec` client. Length-prefixed (u32 BE + bincode payload),
//! reject any frame larger than 16 MB to keep a malformed peer from
//! exhausting memory on a stray big-length read.
//!
//! We use bincode (not the COBS+CRC framing from `wiredesk-protocol`)
//! because Unix sockets already give us a reliable byte-stream — the
//! extra framing layer would just be overhead. Length-prefix is the
//! canonical "split-the-stream" answer here.

use std::io::{self, Read, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One client → server message: the command to run + its parameters.
/// One per connection — the IPC handler enforces a single-in-flight
/// queue at the acceptor level, so there's no per-connection
/// multiplexing to worry about.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpcRequest {
    pub cmd: String,
    pub ssh: Option<String>,
    pub timeout_secs: u64,
}

/// Server → client stream: zero or more `Stdout` frames followed by
/// exactly one terminal frame (`Exit` on success, `Error` on a failure
/// the handler decided to surface to the caller — e.g. transport drop
/// mid-run, runner returning Closed).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum IpcResponse {
    Stdout(Vec<u8>),
    Exit(i32),
    Error(String),
}

/// Defensive cap so a malformed peer can't trick us into allocating a
/// 4 GB buffer on a single bad u32 length-prefix. 16 MB is generous
/// even for the largest realistic stdout chunk (typical `wd --exec`
/// docker-logs output is ~200 KB).
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Write a length-prefixed frame: u32 BE byte count, then `payload`.
pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame payload {} exceeds u32::MAX", payload.len()),
        )
    })?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    Ok(())
}

/// Read a length-prefixed frame. Reject lengths > MAX_FRAME_BYTES so
/// we don't allocate gigabytes on a malformed peer.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds {MAX_FRAME_BYTES}-byte cap"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn bincode_to_io(e: bincode::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("bincode: {e}"))
}

/// Encode + frame an `IpcRequest`.
pub fn write_request<W: Write>(w: &mut W, req: &IpcRequest) -> io::Result<()> {
    let bytes = bincode::serialize(req).map_err(bincode_to_io)?;
    write_frame(w, &bytes)
}

/// Decode a framed `IpcRequest` from the stream.
pub fn read_request<R: Read>(r: &mut R) -> io::Result<IpcRequest> {
    let bytes = read_frame(r)?;
    bincode::deserialize(&bytes).map_err(bincode_to_io)
}

/// Encode + frame an `IpcResponse`.
pub fn write_response<W: Write>(w: &mut W, resp: &IpcResponse) -> io::Result<()> {
    let bytes = bincode::serialize(resp).map_err(bincode_to_io)?;
    write_frame(w, &bytes)
}

/// Decode a framed `IpcResponse` from the stream.
pub fn read_response<R: Read>(r: &mut R) -> io::Result<IpcResponse> {
    let bytes = read_frame(r)?;
    bincode::deserialize(&bytes).map_err(bincode_to_io)
}

/// Default socket path used by GUI (acceptor) and `wd --exec` (client).
/// Single source of truth so one side renaming the path can't drift
/// from the other.
///
/// On macOS we use `~/Library/Application Support/WireDesk/wd-exec.sock`
/// — same dir as `config.toml`, GUI already maintains it. On other
/// platforms (non-Mac builds of the term binary) we fall back to
/// `$TMPDIR/wd-exec.sock` so cross-compilation doesn't break; in
/// practice IPC is Mac-only and the term's `try_socket_first` is
/// cfg-gated to skip the connect altogether on non-Mac.
pub fn default_socket_path() -> PathBuf {
    if cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut p = PathBuf::from(home);
            p.push("Library");
            p.push("Application Support");
            p.push("WireDesk");
            p.push("wd-exec.sock");
            return p;
        }
    }
    let mut p = std::env::temp_dir();
    p.push("wd-exec.sock");
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_round_trip_empty_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"").unwrap();
        let mut r = Cursor::new(buf);
        let read = read_frame(&mut r).unwrap();
        assert!(read.is_empty());
    }

    #[test]
    fn frame_round_trip_small_payload() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"hello world").unwrap();
        let mut r = Cursor::new(buf);
        let read = read_frame(&mut r).unwrap();
        assert_eq!(read, b"hello world");
    }

    #[test]
    fn frame_round_trip_large_payload() {
        // 1 MB chunk — within cap, common for big docker-logs output.
        let payload = vec![0xABu8; 1_000_000];
        let mut buf = Vec::new();
        write_frame(&mut buf, &payload).unwrap();
        let mut r = Cursor::new(buf);
        let read = read_frame(&mut r).unwrap();
        assert_eq!(read.len(), 1_000_000);
        assert_eq!(read[0], 0xAB);
    }

    #[test]
    fn frame_rejects_oversize_length_prefix() {
        // Forge a length-prefix above the 16 MB cap. read_frame must
        // refuse before allocating the buffer.
        let mut bad = Vec::new();
        bad.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_be_bytes());
        let mut r = Cursor::new(bad);
        let err = read_frame(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_round_trip() {
        let req = IpcRequest {
            cmd: "docker ps".into(),
            ssh: Some("prod-mup".into()),
            timeout_secs: 90,
        };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        let mut r = Cursor::new(buf);
        let decoded = read_request(&mut r).unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn response_round_trip_all_variants() {
        for resp in [
            IpcResponse::Stdout(b"row1\nrow2\n".to_vec()),
            IpcResponse::Stdout(Vec::new()),
            IpcResponse::Exit(0),
            IpcResponse::Exit(124),
            IpcResponse::Exit(-1),
            IpcResponse::Error("transport closed".into()),
        ] {
            let mut buf = Vec::new();
            write_response(&mut buf, &resp).unwrap();
            let mut r = Cursor::new(buf);
            let decoded = read_response(&mut r).unwrap();
            assert_eq!(decoded, resp);
        }
    }

    #[test]
    fn default_socket_path_contains_wd_exec_sock() {
        let p = default_socket_path();
        let s = p.to_string_lossy();
        assert!(s.ends_with("wd-exec.sock"), "socket path must end with the canonical name: {s}");
    }

    #[cfg(unix)]
    #[test]
    fn unix_stream_pair_request_response_streaming() {
        // End-to-end: spawn a thread that writes a Request and a few
        // Stdout responses, read them on the other half. Exercises
        // the actual UnixStream/SocketPair path the IPC handler will use.
        use std::os::unix::net::UnixStream;
        use std::thread;

        let (mut a, mut b) = UnixStream::pair().expect("UnixStream::pair");

        let writer = thread::spawn(move || {
            let req = IpcRequest {
                cmd: "echo hi".into(),
                ssh: None,
                timeout_secs: 5,
            };
            write_request(&mut a, &req).unwrap();
            write_response(&mut a, &IpcResponse::Stdout(b"hi\n".to_vec())).unwrap();
            write_response(&mut a, &IpcResponse::Exit(0)).unwrap();
        });

        let req = read_request(&mut b).unwrap();
        assert_eq!(req.cmd, "echo hi");
        match read_response(&mut b).unwrap() {
            IpcResponse::Stdout(bytes) => assert_eq!(bytes, b"hi\n"),
            other => panic!("expected Stdout, got {other:?}"),
        }
        assert!(matches!(read_response(&mut b).unwrap(), IpcResponse::Exit(0)));

        writer.join().expect("writer thread");
    }
}
