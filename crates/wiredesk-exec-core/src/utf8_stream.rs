//! Incremental UTF-8 decoding for a byte stream that arrives in chunks.
//!
//! The host reads the shell's stdout in fixed-size reads and the wire splits
//! whatever comes out at `MAX_PAYLOAD` boundaries — neither knows where a
//! character ends. Decoding each chunk on its own with `from_utf8_lossy`
//! therefore destroys every character that happens to straddle a boundary:
//! the tail of the first chunk and the head of the next each become
//! `U+FFFD`. With ASCII output it never shows; with Cyrillic it hits roughly
//! once per 4 KB, which is exactly the shape of the long-standing
//! "two bytes go missing in long Cyrillic output" report.
//!
//! [`Utf8Stream`] holds those trailing bytes back until their character is
//! complete. A byte that is genuinely invalid — not merely truncated — still
//! becomes `U+FFFD`, so a binary blob on stdout degrades the same way it did
//! before instead of stalling the stream.

/// Decoder state: at most three bytes of a character still in flight.
#[derive(Debug, Default)]
pub struct Utf8Stream {
    tail: Vec<u8>,
}

impl Utf8Stream {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode everything complete in `chunk`, keeping an unfinished
    /// character for the next call.
    pub fn push(&mut self, chunk: &[u8]) -> String {
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(chunk);

        let mut out = String::with_capacity(buf.len());
        let mut rest: &[u8] = &buf;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // Everything before the bad spot is real text.
                    out.push_str(std::str::from_utf8(&rest[..valid]).unwrap_or_default());
                    match e.error_len() {
                        // A byte that cannot start or continue a character:
                        // replace it and carry on, as lossy decoding would.
                        Some(bad) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &rest[valid + bad..];
                        }
                        // The input simply stops mid-character. Those bytes
                        // are not damaged, they are early — hold them.
                        None => {
                            self.tail.extend_from_slice(&rest[valid..]);
                            break;
                        }
                    }
                }
            }
        }
        out
    }

    /// The stream ended. A character still unfinished here is truncated for
    /// good, so it degrades to `U+FFFD` rather than being dropped silently.
    pub fn finish(&mut self) -> String {
        if self.tail.is_empty() {
            return String::new();
        }
        let tail = std::mem::take(&mut self.tail);
        String::from_utf8_lossy(&tail).into_owned()
    }

    /// Whether a character is currently being held back.
    pub fn is_empty(&self) -> bool {
        self.tail.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passes_through_unchanged() {
        let mut s = Utf8Stream::new();
        assert_eq!(s.push(b"hello "), "hello ");
        assert_eq!(s.push(b"world\n"), "world\n");
        assert!(s.is_empty());
        assert_eq!(s.finish(), "");
    }

    #[test]
    fn a_character_split_across_two_chunks_survives() {
        // "привет" cut in the middle of "и" (2 bytes in UTF-8).
        let text = "привет";
        let bytes = text.as_bytes();
        let cut = "пр".len() + 1; // one byte into "и"
        let mut s = Utf8Stream::new();
        let first = s.push(&bytes[..cut]);
        assert_eq!(first, "пр", "the half character must not be emitted yet");
        assert!(!s.is_empty());
        let second = s.push(&bytes[cut..]);
        assert_eq!(format!("{first}{second}"), text);
        assert!(s.is_empty());
    }

    #[test]
    fn every_split_point_of_a_mixed_string_round_trips() {
        // The wire can cut anywhere, so check that it does not matter where.
        let text = "ascii Кириллица 漢字 🙂 tail";
        let bytes = text.as_bytes();
        for cut in 0..=bytes.len() {
            let mut s = Utf8Stream::new();
            let mut got = s.push(&bytes[..cut]);
            got.push_str(&s.push(&bytes[cut..]));
            got.push_str(&s.finish());
            assert_eq!(got, text, "split at byte {cut}");
        }
    }

    #[test]
    fn a_four_byte_character_arriving_one_byte_at_a_time_survives() {
        let text = "🙂";
        let mut s = Utf8Stream::new();
        let mut got = String::new();
        for b in text.as_bytes() {
            got.push_str(&s.push(&[*b]));
        }
        assert_eq!(got, text);
        assert!(s.is_empty());
    }

    #[test]
    fn a_genuinely_invalid_byte_becomes_a_replacement_and_does_not_stall() {
        let mut s = Utf8Stream::new();
        // 0xFF can neither start nor continue a character.
        let got = s.push(b"a\xFFb");
        assert_eq!(got, "a\u{FFFD}b");
        assert!(s.is_empty(), "an invalid byte must not be held back");
    }

    #[test]
    fn a_truncated_tail_at_end_of_stream_is_reported_not_dropped() {
        let mut s = Utf8Stream::new();
        let head = s.push("привет".as_bytes()[..7].as_ref()); // cuts "е" in half
        assert_eq!(head, "при");
        assert_eq!(s.finish(), "\u{FFFD}");
        assert!(s.is_empty());
    }

    #[test]
    fn the_held_tail_never_grows_past_a_character() {
        let mut s = Utf8Stream::new();
        // Feed lead bytes of 4-byte characters without their continuations:
        // each is invalid as soon as the next lead byte arrives, so nothing
        // accumulates.
        for _ in 0..100 {
            let _ = s.push(b"\xF0");
        }
        assert!(s.tail.len() <= 4, "tail grew to {} bytes", s.tail.len());
    }
}
