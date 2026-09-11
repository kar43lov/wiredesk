//! Platform-independent pieces of the RFCOMM transport: the idle keepalive
//! and the receive-side partial-frame bookkeeping.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_protocol::packet::Packet;

use crate::framing::FrameReader;

/// How long a single `recv` blocks before reporting `"recv timeout"`. The
/// session loops on both sides treat that error as "nothing arrived, carry
/// on" — and the host forwards shell output / clipboard chunks only between
/// `recv` calls, so this is also the outbound pacing: at 250 ms the link
/// carried ~24 KB/s live (2026-09-11), at 10 ms — the same value
/// `SerialTransport` uses — the loop keeps up with the radio.
pub const RECV_POLL: Duration = Duration::from_millis(10);

/// A frame whose tail hasn't arrived within this many `RECV_POLL` timeouts
/// is abandoned — same budget as `SerialTransport` (~5 s).
pub const MAX_PARTIAL_TIMEOUTS: u32 = 500;

/// Upper bound on one blocking write. A peer that stops reading exhausts
/// the RFCOMM credits and the write blocks forever; without a bound that
/// stalls the host's tick loop (heartbeats, key release) or the client's
/// writer join on reconnect. One 8 KB frame takes ~70 ms at 120 KB/s, so
/// 10 s only ever trips on a dead peer.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Parse an address like `A0:B1:C2:D3:E4:F5` (or with `-`) into the 48-bit
/// value with the first octet in the high bits — the layout both Winsock's
/// `BTH_ADDR` and IOBluetooth's `BluetoothDeviceAddress` agree on.
pub fn parse_bt_address(s: &str) -> Result<u64> {
    let octets: Vec<&str> = s.trim().split([':', '-']).collect();
    if octets.len() != 6 {
        return Err(WireDeskError::Transport(format!(
            "RFCOMM: bad Bluetooth address '{s}' (want XX:XX:XX:XX:XX:XX)"
        )));
    }
    let mut v: u64 = 0;
    for o in octets {
        let b = u8::from_str_radix(o, 16).map_err(|_| {
            WireDeskError::Transport(format!("RFCOMM: bad Bluetooth address '{s}'"))
        })?;
        v = (v << 8) | u64::from(b);
    }
    Ok(v)
}

/// Canonical `XX:XX:XX:XX:XX:XX` (upper-case, colon-separated) form of a
/// 48-bit address — the spelling IOBluetooth and the logs use.
pub fn format_bt_address(addr: u64) -> String {
    (0..6)
        .rev()
        .map(|i| format!("{:02X}", (addr >> (8 * i)) & 0xFF))
        .collect::<Vec<_>>()
        .join(":")
}

/// Receive-side state shared by both platform implementations: the frame
/// extractor plus the "partial frame abandoned" timeout counter.
#[derive(Debug)]
pub struct RecvState {
    pub reader: FrameReader,
    partial_timeouts: u32,
}

impl Default for RecvState {
    fn default() -> Self {
        Self::new()
    }
}

impl RecvState {
    pub fn new() -> Self {
        Self {
            reader: FrameReader::new(),
            partial_timeouts: 0,
        }
    }

    /// A `RECV_POLL` elapsed with nothing arriving. Returns the error the
    /// caller should surface.
    pub fn on_timeout(&mut self) -> WireDeskError {
        if self.reader.partial_len() == 0 {
            return WireDeskError::Transport("recv timeout".into());
        }
        self.partial_timeouts += 1;
        if self.partial_timeouts > MAX_PARTIAL_TIMEOUTS {
            log::warn!(
                "RFCOMM: partial frame abandoned after {} timeouts ({} bytes)",
                self.partial_timeouts,
                self.reader.partial_len()
            );
            self.reader.abandon_partial();
            self.partial_timeouts = 0;
            return WireDeskError::Transport("recv timeout (partial frame abandoned)".into());
        }
        WireDeskError::Transport("recv timeout".into())
    }

    /// Pop the next decoded packet, resetting the partial counter on
    /// success.
    pub fn next_packet(&mut self) -> Result<Option<Packet>> {
        let r = self.reader.next_packet();
        if matches!(r, Ok(Some(_))) {
            self.partial_timeouts = 0;
        }
        r
    }
}

/// Background thread that writes a single `0x00` (an empty COBS frame the
/// receiver skips) whenever nothing has been sent for one period. Keeps a
/// Bluetooth Classic ACL link from entering sniff mode, which would add
/// ~80 ms to the first packet after a pause.
pub struct IdleKeepalive {
    stop: Arc<AtomicBool>,
    last_write: Arc<Mutex<Instant>>,
}

impl IdleKeepalive {
    /// Start the thread. `write_zero` sends the keepalive byte; returning
    /// `false` (link gone) ends the thread. A zero `period` disables the
    /// keepalive entirely — the returned handle is then inert.
    pub fn spawn<F>(period: Duration, write_zero: F) -> Self
    where
        F: Fn() -> bool + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(period.is_zero()));
        let last_write = Arc::new(Mutex::new(Instant::now()));
        if !period.is_zero() {
            let stop_t = Arc::clone(&stop);
            let last_t = Arc::clone(&last_write);
            thread::Builder::new()
                .name("wiredesk-rfcomm-keepalive".into())
                .spawn(move || {
                    while !stop_t.load(Ordering::Acquire) {
                        thread::sleep(period / 2);
                        if stop_t.load(Ordering::Acquire) {
                            break;
                        }
                        let idle = last_t.lock().map(|t| t.elapsed()).unwrap_or(period);
                        if idle >= period {
                            if !write_zero() {
                                break;
                            }
                            if let Ok(mut t) = last_t.lock() {
                                *t = Instant::now();
                            }
                        }
                    }
                })
                .expect("spawn keepalive thread");
        }
        Self { stop, last_write }
    }

    /// Record that real traffic went out — postpones the next keepalive.
    pub fn note_write(&self) {
        if let Ok(mut t) = self.last_write.lock() {
            *t = Instant::now();
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Drop for IdleKeepalive {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn parse_and_format_address_roundtrip() {
        let a = parse_bt_address("A0:B1:C2:D3:E4:F5").unwrap();
        assert_eq!(a, 0xA0B1_C2D3_E4F5);
        assert_eq!(format_bt_address(a), "A0:B1:C2:D3:E4:F5");
        assert_eq!(parse_bt_address("a0-b1-c2-d3-e4-f5").unwrap(), a);
        assert!(parse_bt_address("A0:B1:C2:D3:E4").is_err());
        assert!(parse_bt_address("zz:B1:C2:D3:E4:F5").is_err());
    }

    #[test]
    fn recv_state_reports_timeout_and_abandons_partial() {
        let mut st = RecvState::new();
        assert_eq!(st.on_timeout().to_string(), "transport: recv timeout");
        st.reader.feed(&[0x00, 0x11]);
        for _ in 0..MAX_PARTIAL_TIMEOUTS {
            assert!(!st.on_timeout().to_string().contains("abandoned"));
        }
        assert!(st.on_timeout().to_string().contains("abandoned"));
        assert_eq!(st.reader.partial_len(), 0);
    }

    #[test]
    fn keepalive_fires_when_idle_and_stops() {
        let hits = Arc::new(AtomicU32::new(0));
        let h = Arc::clone(&hits);
        let ka = IdleKeepalive::spawn(Duration::from_millis(40), move || {
            h.fetch_add(1, Ordering::SeqCst);
            true
        });
        thread::sleep(Duration::from_millis(200));
        assert!(hits.load(Ordering::SeqCst) >= 2, "keepalive never fired");
        ka.stop();
        thread::sleep(Duration::from_millis(60));
        let n = hits.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(120));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            n,
            "keepalive kept firing after stop"
        );
    }

    #[test]
    fn keepalive_note_write_postpones() {
        let hits = Arc::new(AtomicU32::new(0));
        let h = Arc::clone(&hits);
        let ka = IdleKeepalive::spawn(Duration::from_millis(80), move || {
            h.fetch_add(1, Ordering::SeqCst);
            true
        });
        for _ in 0..10 {
            thread::sleep(Duration::from_millis(20));
            ka.note_write();
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "fired despite steady traffic"
        );
    }

    #[test]
    fn keepalive_zero_period_is_inert() {
        let hits = Arc::new(AtomicU32::new(0));
        let h = Arc::clone(&hits);
        let _ka = IdleKeepalive::spawn(Duration::ZERO, move || {
            h.fetch_add(1, Ordering::SeqCst);
            true
        });
        thread::sleep(Duration::from_millis(50));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }
}
