use std::io::{Read, Write};
use std::time::Duration;

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::cobs;
use wiredesk_protocol::packet::Packet;

use crate::transport::Transport;

pub struct SerialTransport {
    port: Box<dyn serialport::SerialPort>,
    read_buf: Vec<u8>,
    partial_timeouts: u32,
    inbox: ByteSource,
}

/// How many bytes the reader asks the port for in one go.
const READ_BLOCK: usize = 8192;

/// Bytes pulled off the port but not yet consumed by the frame loop.
///
/// `recv` has to walk the stream one byte at a time: the COBS delimiter is
/// what ends a frame, and one read routinely spans several frames, so the
/// leftovers must survive until the next call. Asking the port for each of
/// those bytes separately is what this used to do — a 4 KB frame cost about
/// 4300 read calls, and a megabyte of `wd --exec` output cost a million. The
/// bytes now arrive in blocks and the loop walks memory.
#[derive(Debug)]
struct ByteSource {
    buf: Vec<u8>,
    /// Bytes the last read actually handed over.
    len: usize,
    pos: usize,
}

impl Default for ByteSource {
    fn default() -> Self {
        // Allocated once per handle and reused: refilling through
        // `Vec::resize` would re-zero the whole block on every read, which on
        // a megabyte of traffic is a megabyte of pointless memset.
        Self {
            buf: vec![0; READ_BLOCK],
            len: 0,
            pos: 0,
        }
    }
}

impl ByteSource {
    /// Next byte, refilling from `src` when the buffer runs dry.
    ///
    /// `Ok(None)` means the port handed back nothing at all. That is not the
    /// same as a timeout to `std::io`, but it is the same thing to us, and the
    /// caller must treat it that way — see `recv`.
    fn next<R: Read + ?Sized>(&mut self, src: &mut R) -> std::io::Result<Option<u8>> {
        if self.pos == self.len {
            match src.read(&mut self.buf) {
                Ok(n) => {
                    self.len = n;
                    self.pos = 0;
                    if n == 0 {
                        return Ok(None);
                    }
                }
                Err(e) => {
                    // Nothing was handed over, so nothing is buffered.
                    self.len = 0;
                    self.pos = 0;
                    return Err(e);
                }
            }
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(Some(b))
    }
}

const MAX_PARTIAL_TIMEOUTS: u32 = 500; // ~5 sec at 10ms timeout

/// Hard frame-size limit before the reader discards and resyncs.
/// Must accommodate `MAX_PAYLOAD` (4096) + header (8) + CRC (2) + COBS
/// overhead (~16) with margin. Bumped from hardcoded 1024 along with
/// MAX_PAYLOAD 512→4096 in feat/wd-exec-fixes.
const MAX_FRAME_SIZE: usize = 8192;

/// Assemble one COBS frame off `port` and decode it.
///
/// A free function over any `Read` rather than a method, so the frame loop —
/// the hottest path in the project — can be driven by a scripted reader in
/// tests. A `Box<dyn SerialPort>` needs hardware; this needs nothing.
fn read_frame<R: Read + ?Sized>(
    port: &mut R,
    inbox: &mut ByteSource,
    read_buf: &mut Vec<u8>,
    partial_timeouts: &mut u32,
) -> Result<Packet> {
    // Read until we find a 0x00 delimiter (COBS frame boundary).
    // Note: read_buf may contain a partial frame from a previous timeout.
    loop {
        let got = match inbox.next(port) {
            Ok(Some(b)) => Some(b),
            // Nothing arrived. A silent line reports a timeout; a port that
            // has gone away reports zero bytes, because a tty hangup makes
            // read(2) return 0 rather than fail. Both mean the line is not
            // talking, and both have to reach the bookkeeping below — a bare
            // `continue` on the zero case spins at 100% CPU and never
            // abandons the half frame it is holding.
            Ok(None) => None,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => None,
            Err(e) => return Err(WireDeskError::Transport(format!("serial read: {e}"))),
        };

        let Some(byte) = got else {
            if read_buf.is_empty() {
                return Err(WireDeskError::Transport("recv timeout".into()));
            }
            // Partial frame in buffer — retry, but not forever.
            *partial_timeouts += 1;
            if *partial_timeouts > MAX_PARTIAL_TIMEOUTS {
                log::warn!(
                    "partial frame abandoned after {} timeouts ({} bytes)",
                    partial_timeouts,
                    read_buf.len()
                );
                read_buf.clear();
                *partial_timeouts = 0;
                return Err(WireDeskError::Transport(
                    "recv timeout (partial frame abandoned)".into(),
                ));
            }
            continue;
        };

        if byte == 0x00 {
            if read_buf.is_empty() {
                // Skip leading delimiters
                continue;
            }
            // Add delimiter back for COBS decode
            read_buf.push(0x00);
            break;
        }
        read_buf.push(byte);

        if read_buf.len() > MAX_FRAME_SIZE {
            // Discard and skip to next delimiter
            read_buf.clear();
            loop {
                match inbox.next(port) {
                    Ok(Some(0x00)) => break,
                    Ok(Some(_)) => continue,
                    // Nothing more to skip right now: the next call resumes
                    // from wherever the line is.
                    Ok(None) | Err(_) => break,
                }
            }
            return Err(WireDeskError::Protocol("frame too large".into()));
        }
    }

    *partial_timeouts = 0;
    let raw =
        cobs::decode(read_buf).map_err(|e| WireDeskError::Protocol(format!("COBS decode: {e}")))?;
    read_buf.clear();
    Packet::from_bytes(&raw)
}

impl SerialTransport {
    pub fn open(port_name: &str, baud_rate: u32) -> Result<Self> {
        let mut port = serialport::new(port_name, baud_rate)
            .timeout(Duration::from_millis(10))
            .open()
            .map_err(|e| WireDeskError::Transport(format!("serial open {port_name}: {e}")))?;

        // Many USB-UART chips (CH340, FTDI, etc.) emit a stray byte when DTR
        // toggles on open. Wait briefly for the line to settle, then drain
        // anything that arrived during that window so it doesn't get glued to
        // the first real frame and produce "bad magic" errors.
        std::thread::sleep(Duration::from_millis(100));
        let mut scratch = [0u8; 256];
        loop {
            match port.read(&mut scratch) {
                Ok(n) if n > 0 => {
                    log::debug!("serial open: drained {n} byte(s) of startup junk");
                    continue;
                }
                _ => break,
            }
        }

        Ok(Self {
            port,
            read_buf: Vec::with_capacity(MAX_FRAME_SIZE),
            partial_timeouts: 0,
            inbox: ByteSource::default(),
        })
    }
}

impl Transport for SerialTransport {
    fn send(&mut self, packet: &Packet) -> Result<()> {
        let raw = packet.to_bytes()?;
        let encoded = cobs::encode(&raw);
        // Single write: [0x00 leading delimiter] + encoded packet. The leading
        // 0x00 forces a frame boundary so any line noise preceding this packet
        // ends up in its own (invalid, ignored) frame. Combining into one
        // write_all is one syscall instead of two.
        let mut buf = Vec::with_capacity(encoded.len() + 1);
        buf.push(0x00);
        buf.extend_from_slice(&encoded);
        self.port
            .write_all(&buf)
            .map_err(|e| WireDeskError::Transport(format!("serial write: {e}")))?;
        self.port
            .flush()
            .map_err(|e| WireDeskError::Transport(format!("serial flush: {e}")))?;
        Ok(())
    }

    fn recv(&mut self) -> Result<Packet> {
        read_frame(
            &mut *self.port,
            &mut self.inbox,
            &mut self.read_buf,
            &mut self.partial_timeouts,
        )
    }

    fn is_connected(&self) -> bool {
        true // Serial port is connected if open
    }

    fn name(&self) -> &'static str {
        "serial"
    }

    fn try_clone(&self) -> Result<Box<dyn Transport>> {
        let cloned = self
            .port
            .try_clone()
            .map_err(|e| WireDeskError::Transport(format!("serial try_clone: {e}")))?;
        Ok(Box::new(SerialTransport {
            port: cloned,
            read_buf: Vec::with_capacity(MAX_FRAME_SIZE),
            partial_timeouts: 0,
            // A fresh buffer.
            //
            // 🔴 **Exactly one of the two handles may call `recv`.** Which one
            // differs by call site — `link.rs` keeps the original as the
            // reader and hands the clone to the writer thread, while
            // `wiredesk-term` does it the other way round — so the rule cannot
            // be enforced by marking the clone read-only. Read from both and
            // the stream tears in a way that produces no error at all: bytes
            // already pulled into one handle's buffer are invisible to the
            // other, and new bytes go to whoever asks the kernel first.
            inbox: ByteSource::default(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    /// A `Read` that hands out a scripted stream and counts how often it was
    /// asked. The count is the whole point: the frame loop walks the stream
    /// byte by byte, and what changed is how many of those bytes cost a call
    /// to the port.
    struct CountingReader {
        data: Vec<u8>,
        pos: usize,
        pub reads: usize,
        /// Bytes handed over per call, to imitate a port that returns what it
        /// has rather than filling the buffer.
        per_read: usize,
        /// Error to return once the data runs out, instead of `Ok(0)`.
        then: Option<ErrorKind>,
    }

    impl CountingReader {
        fn new(data: Vec<u8>, per_read: usize) -> Self {
            Self {
                data,
                pos: 0,
                reads: 0,
                per_read,
                then: None,
            }
        }

        fn erroring_after(data: Vec<u8>, per_read: usize, kind: ErrorKind) -> Self {
            let mut r = Self::new(data, per_read);
            r.then = Some(kind);
            r
        }
    }

    impl Read for CountingReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.pos >= self.data.len() {
                return match self.then {
                    Some(kind) => Err(Error::new(kind, "scripted")),
                    None => Ok(0),
                };
            }
            let n = self.per_read.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn drain(src: &mut CountingReader) -> Vec<u8> {
        let mut inbox = ByteSource::default();
        let mut out = Vec::new();
        while let Ok(Some(b)) = inbox.next(src) {
            out.push(b);
        }
        out
    }

    fn frame_of(msg: wiredesk_protocol::message::Message) -> Vec<u8> {
        crate::framing::encode_frame(&Packet::new(msg, 0)).expect("encode")
    }

    fn read_one(src: &mut CountingReader, inbox: &mut ByteSource) -> Result<Packet> {
        let mut read_buf = Vec::new();
        let mut timeouts = 0u32;
        read_frame(src, inbox, &mut read_buf, &mut timeouts)
    }

    #[test]
    fn a_frame_off_the_wire_decodes() {
        use wiredesk_protocol::message::Message;
        let mut src = CountingReader::new(frame_of(Message::Heartbeat), READ_BLOCK);
        let mut inbox = ByteSource::default();
        let pkt = read_one(&mut src, &mut inbox).expect("frame");
        assert!(matches!(pkt.message, Message::Heartbeat));
    }

    #[test]
    fn two_frames_in_one_block_cost_one_read() {
        use wiredesk_protocol::message::Message;
        let mut wire = frame_of(Message::Heartbeat);
        wire.extend(frame_of(Message::Disconnect));
        let mut src = CountingReader::new(wire, READ_BLOCK);
        let mut inbox = ByteSource::default();

        assert!(matches!(
            read_one(&mut src, &mut inbox).expect("first").message,
            Message::Heartbeat
        ));
        assert_eq!(src.reads, 1);
        // The tail of the block has to survive between calls — this is the
        // invariant the byte-at-a-time reader got for free.
        assert!(matches!(
            read_one(&mut src, &mut inbox).expect("second").message,
            Message::Disconnect
        ));
        assert_eq!(src.reads, 1, "the second frame was already in hand");
    }

    #[test]
    fn a_silent_line_reports_a_timeout_rather_than_spinning() {
        let mut src = CountingReader::erroring_after(Vec::new(), 1, ErrorKind::TimedOut);
        let mut inbox = ByteSource::default();
        let err = read_one(&mut src, &mut inbox).expect_err("must not block");
        assert!(err.to_string().contains("recv timeout"), "err: {err}");
    }

    #[test]
    fn a_port_that_returns_zero_bytes_does_not_spin() {
        // A tty hangup makes read(2) return 0 instead of failing. Treating
        // that as "carry on" is an endless loop at 100% CPU; it has to land
        // in the same bookkeeping a timeout does.
        let mut src = CountingReader::new(Vec::new(), 1); // always Ok(0)
        let mut inbox = ByteSource::default();
        let err = read_one(&mut src, &mut inbox).expect_err("must not loop forever");
        assert!(err.to_string().contains("recv timeout"), "err: {err}");
        assert_eq!(src.reads, 1, "one look at a dead port is enough");
    }

    #[test]
    fn a_half_frame_then_silence_is_abandoned_not_held_forever() {
        // Half a frame arrives and the line goes quiet: the reader counts
        // timeouts and gives the frame up rather than waiting out the session.
        let mut src =
            CountingReader::erroring_after(vec![0x00, 0x11, 0x22], 3, ErrorKind::TimedOut);
        let mut inbox = ByteSource::default();
        let mut read_buf = Vec::new();
        let mut timeouts = MAX_PARTIAL_TIMEOUTS; // one short of the limit
        let err =
            read_frame(&mut src, &mut inbox, &mut read_buf, &mut timeouts).expect_err("abandon");
        assert!(
            err.to_string().contains("partial frame abandoned"),
            "err: {err}"
        );
        assert!(read_buf.is_empty(), "the half frame must be dropped");
        assert_eq!(timeouts, 0, "the counter restarts after abandoning");
    }

    #[test]
    fn an_oversize_frame_is_reported_and_the_reader_resyncs() {
        use wiredesk_protocol::message::Message;
        // Garbage with no delimiter, past the frame limit, then a real frame.
        let mut wire = vec![0x01; MAX_FRAME_SIZE + 16];
        wire.extend(frame_of(Message::Heartbeat));
        let mut src = CountingReader::new(wire, READ_BLOCK);
        let mut inbox = ByteSource::default();

        let err = read_one(&mut src, &mut inbox).expect_err("oversize");
        assert!(err.to_string().contains("frame too large"), "err: {err}");
        // Resync consumed the junk up to the next delimiter, so the frame
        // that followed it still decodes.
        assert!(matches!(
            read_one(&mut src, &mut inbox)
                .expect("frame after resync")
                .message,
            Message::Heartbeat
        ));
    }

    #[test]
    fn a_corrupt_frame_is_a_protocol_error_so_the_storm_detector_sees_it() {
        // StormCounter only counts Protocol errors; a bit-flipped frame has
        // to keep arriving as one.
        let mut wire = frame_of(wiredesk_protocol::message::Message::Heartbeat);
        let mid = wire.len() / 2;
        wire[mid] ^= 0xFF;
        let mut src = CountingReader::new(wire, READ_BLOCK);
        let mut inbox = ByteSource::default();
        let err = read_one(&mut src, &mut inbox).expect_err("corrupt");
        assert!(
            matches!(err, WireDeskError::Protocol(_)),
            "must be Protocol, got {err:?}"
        );
    }

    #[test]
    fn the_stream_arrives_byte_for_byte_in_order() {
        let data: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
        let mut src = CountingReader::new(data.clone(), 1000);
        assert_eq!(drain(&mut src), data, "buffering must not reorder or lose");
    }

    #[test]
    fn one_read_per_block_not_one_per_byte() {
        let data: Vec<u8> = vec![0x41; 4096];
        let mut src = CountingReader::new(data, READ_BLOCK);
        let got = drain(&mut src);
        assert_eq!(got.len(), 4096);
        // One read that returns everything, one more that reports the end.
        assert_eq!(
            src.reads, 2,
            "4096 bytes must not cost 4096 reads — that was the bug"
        );
    }

    #[test]
    fn a_port_that_dribbles_still_costs_one_read_per_piece() {
        let mut src = CountingReader::new(vec![0x42; 300], 100);
        assert_eq!(drain(&mut src).len(), 300);
        assert_eq!(src.reads, 4, "three pieces plus the end-of-stream read");
    }

    #[test]
    fn an_empty_read_reports_nothing_rather_than_blocking() {
        let mut src = CountingReader::new(Vec::new(), 100);
        let mut inbox = ByteSource::default();
        assert!(matches!(inbox.next(&mut src), Ok(None)));
    }

    #[test]
    fn an_error_is_surfaced_and_leaves_nothing_buffered() {
        let mut src = CountingReader::erroring_after(vec![0x01, 0x02], 2, ErrorKind::TimedOut);
        let mut inbox = ByteSource::default();
        assert!(matches!(inbox.next(&mut src), Ok(Some(0x01))));
        assert!(matches!(inbox.next(&mut src), Ok(Some(0x02))));
        let err = inbox.next(&mut src).expect_err("timeout must surface");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        // And the next call asks the port again rather than replaying bytes.
        let err = inbox.next(&mut src).expect_err("still timing out");
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert_eq!(src.reads, 3, "one refill, then one call per timeout");
    }

    #[test]
    fn buffered_bytes_survive_across_calls() {
        // The frame loop stops at a delimiter mid-block; whatever followed it
        // has to still be there for the next frame.
        let mut src = CountingReader::new(vec![b'a', 0x00, b'b', b'c'], 4);
        let mut inbox = ByteSource::default();
        assert!(matches!(inbox.next(&mut src), Ok(Some(b'a'))));
        assert!(matches!(inbox.next(&mut src), Ok(Some(0x00))));
        assert_eq!(src.reads, 1, "still inside the first block");
        assert!(matches!(inbox.next(&mut src), Ok(Some(b'b'))));
        assert!(matches!(inbox.next(&mut src), Ok(Some(b'c'))));
        assert_eq!(src.reads, 1, "the tail of the block was not re-read");
    }
}
