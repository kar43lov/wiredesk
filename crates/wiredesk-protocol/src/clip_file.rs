//! Filename packing/unpacking + path sanitization for `FORMAT_FILE`
//! clipboard transfers.
//!
//! First-chunk layout:
//! ```text
//! [name_len: u16 LE][name: UTF-8 bytes][content bytes...]
//! ```
//!
//! `name_len` is the **byte-length** of the UTF-8 filename (not char count).
//! Content begins immediately after the name and flows across subsequent
//! chunks transparently (mod CHUNK_SIZE handled at transport layer).
//!
//! Sanitization (`sanitize_basename`) is applied on the **receive** side
//! before writing to disk. It strips path components, Windows drive
//! letters, `..` traversal segments, and prefixes NTFS reserved device
//! names with `_`. Empty results fall back to `clipboard.bin`.

use thiserror::Error;

/// Maximum file payload (excluding filename header). Uniform with
/// `MAX_IMAGE_BYTES` so the size cap UX matches across formats.
pub const MAX_FILE_BYTES: usize = 20 * 1024 * 1024;

/// Safety cap on filename byte length. Real OS limits are ~255-1024;
/// this is a defense-in-depth guard against pathological inputs.
pub const MAX_FILENAME_LEN: usize = 4096;

/// Total wire-payload cap for a `FORMAT_FILE` transfer: max content + max
/// filename + 2-byte name_len header. Used by both sides' `on_offer` size
/// gate to reject peer-supplied `total_len` values that would force an
/// oversize `Vec::with_capacity` allocation during reassembly.
pub const MAX_FILE_PAYLOAD_BYTES: usize = MAX_FILE_BYTES + MAX_FILENAME_LEN + 2;

/// Fallback basename when sanitization strips everything.
pub const FALLBACK_BASENAME: &str = "clipboard.bin";

/// Errors from packing/unpacking the first-chunk header.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ClipFileError {
    /// Payload is shorter than required header / name bytes.
    #[error("clip file payload truncated")]
    Truncated,
    /// Name bytes are not valid UTF-8.
    #[error("clip file name is not valid UTF-8")]
    BadUtf8,
    /// `name.as_bytes().len()` exceeds `MAX_FILENAME_LEN`.
    #[error("clip file name exceeds {MAX_FILENAME_LEN} bytes")]
    NameTooLong,
    /// Caller supplied an empty name.
    #[error("clip file name is empty")]
    EmptyName,
}

/// Pack first chunk of a `FORMAT_FILE` transfer.
///
/// Layout: `[u16 LE name_len][name UTF-8 bytes][content bytes]`.
///
/// # Errors
/// - `EmptyName` if `name` is empty.
/// - `NameTooLong` if `name.as_bytes().len() > MAX_FILENAME_LEN`.
pub fn pack_first_chunk(name: &str, content: &[u8]) -> Result<Vec<u8>, ClipFileError> {
    let name_bytes = name.as_bytes();
    if name_bytes.is_empty() {
        return Err(ClipFileError::EmptyName);
    }
    if name_bytes.len() > MAX_FILENAME_LEN {
        return Err(ClipFileError::NameTooLong);
    }
    let name_len = name_bytes.len() as u16;
    let mut out = Vec::with_capacity(2 + name_bytes.len() + content.len());
    out.extend_from_slice(&name_len.to_le_bytes());
    out.extend_from_slice(name_bytes);
    out.extend_from_slice(content);
    Ok(out)
}

/// Unpack first chunk back into `(name, content)`.
///
/// Note: `content` here is whatever bytes followed the name **in this
/// chunk** — the full file content arrives via subsequent
/// `ClipChunk` messages reassembled at the transport layer. The
/// returned `Vec<u8>` is the *prefix* of the file content carried
/// inline with the header.
///
/// # Errors
/// - `Truncated` if payload is too short for the header / declared name.
/// - `NameTooLong` if `name_len` exceeds [`MAX_FILENAME_LEN`].
/// - `BadUtf8` if the name bytes are not valid UTF-8.
pub fn unpack_first_chunk(payload: &[u8]) -> Result<(String, Vec<u8>), ClipFileError> {
    if payload.len() < 2 {
        return Err(ClipFileError::Truncated);
    }
    let name_len = u16::from_le_bytes([payload[0], payload[1]]) as usize;
    // Defense-in-depth: a peer (malicious or buggy) could send `name_len`
    // up to 65535. `pack_first_chunk` rejects > MAX_FILENAME_LEN on emit,
    // but the receiver must not trust the wire. Without this check, we'd
    // try to sanitize and `fs::write` a multi-kilobyte filename, getting
    // ENAMETOOLONG and log spam.
    if name_len > MAX_FILENAME_LEN {
        return Err(ClipFileError::NameTooLong);
    }
    let name_end = 2 + name_len;
    if payload.len() < name_end {
        return Err(ClipFileError::Truncated);
    }
    let name_bytes = &payload[2..name_end];
    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| ClipFileError::BadUtf8)?
        .to_owned();
    let content = payload[name_end..].to_vec();
    Ok((name, content))
}

/// NTFS reserved device names (case-insensitive). When the **stem**
/// (segment before the first `.`) matches one of these, the result is
/// prefixed with `_` to dodge legacy Win32 device redirection.
const NTFS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Sanitize a raw filename into a safe basename suitable for writing to
/// the local cache directory.
///
/// Strips:
/// - All path components (`/`, `\`) — only the final segment is kept.
/// - `..` segments anywhere in the input.
/// - Windows drive-letter prefixes (`C:`, `Z:foo`).
/// - Leading `:` or whitespace.
///
/// Replaces with `_`:
/// - Windows-reserved chars (`<>:"|?*`) — `fs::write` returns
///   `InvalidInput` on Win otherwise, silently dropping the file.
/// - NUL (`\0`) — illegal in filenames on every host OS.
///
/// NTFS reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`,
/// `LPT1..9`) — match against the stem, case-insensitive — are
/// prefixed with `_` (e.g. `CON.txt` → `_CON.txt`). Prefix-only
/// collisions like `console.txt` are left alone.
///
/// Empty results (e.g. `""`, `".."`, `"C:"`) fall back to
/// [`FALLBACK_BASENAME`].
pub fn sanitize_basename(raw: &str) -> String {
    // Strip Windows drive-letter prefix: "C:foo", "C:\foo" → "foo".
    let stripped_drive = strip_drive_letter(raw);

    // Split on both forward and backward slashes, drop ".." and empty
    // segments, take the *last* surviving segment.
    let segment = stripped_drive
        .split(['/', '\\'])
        .rfind(|s| !s.is_empty() && *s != "..")
        .unwrap_or("");

    // Strip leading colons or whitespace.
    let trimmed = segment.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
    let trimmed = trimmed.trim_end();

    if trimmed.is_empty() {
        return FALLBACK_BASENAME.to_owned();
    }

    // Replace Windows-reserved chars (`<>:"|?*`) and NUL with `_`. Files
    // like `foo|bar.txt` are valid on macOS, so the Mac sender accepts them
    // and packs them into the wire; on the Win receiver, `fs::write` would
    // fail with InvalidInput and the file would be silently lost. Substitute
    // here so the cross-platform paste-back path survives.
    let escaped: String = trimmed
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\0' => '_',
            _ => c,
        })
        .collect();

    // Reserved-name check: match against stem (before first '.').
    let stem = escaped.split('.').next().unwrap_or("");
    let stem_upper = stem.to_ascii_uppercase();
    if NTFS_RESERVED.iter().any(|r| *r == stem_upper) {
        return format!("_{escaped}");
    }

    escaped
}

/// Strip Windows drive-letter prefix like `C:`, `Z:foo`, `c:\path`.
/// Returns the remainder. Non-drive-letter inputs pass through.
fn strip_drive_letter(s: &str) -> &str {
    let mut chars = s.chars();
    let first = chars.next();
    let second = chars.next();
    if let (Some(c), Some(':')) = (first, second) {
        if c.is_ascii_alphabetic() {
            // SAFETY: we matched two ASCII chars, so byte offset = 2.
            return &s[2..];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pack / unpack roundtrip ----

    #[test]
    fn pack_unpack_roundtrip() {
        let name = "contract.pdf";
        let content = vec![0xAA_u8; 1024];
        let packed = pack_first_chunk(name, &content).unwrap();
        let (got_name, got_content) = unpack_first_chunk(&packed).unwrap();
        assert_eq!(got_name, name);
        assert_eq!(got_content, content);
    }

    #[test]
    fn pack_unpack_unicode() {
        let name = "привет 🎉.pdf";
        let content: Vec<u8> = (0..=255_u8).collect();
        let packed = pack_first_chunk(name, &content).unwrap();
        let (got_name, got_content) = unpack_first_chunk(&packed).unwrap();
        assert_eq!(got_name, name);
        assert_eq!(got_content, content);
    }

    #[test]
    fn pack_unpack_empty_content_ok() {
        let packed = pack_first_chunk("a.txt", &[]).unwrap();
        let (n, c) = unpack_first_chunk(&packed).unwrap();
        assert_eq!(n, "a.txt");
        assert!(c.is_empty());
    }

    #[test]
    fn pack_name_too_long() {
        let name = "a".repeat(MAX_FILENAME_LEN + 1);
        let err = pack_first_chunk(&name, &[]).unwrap_err();
        assert_eq!(err, ClipFileError::NameTooLong);
    }

    #[test]
    fn pack_empty_name_errors() {
        let err = pack_first_chunk("", &[1, 2, 3]).unwrap_err();
        assert_eq!(err, ClipFileError::EmptyName);
    }

    #[test]
    fn pack_name_at_max_ok() {
        let name = "a".repeat(MAX_FILENAME_LEN);
        let packed = pack_first_chunk(&name, b"x").unwrap();
        let (got_name, got_content) = unpack_first_chunk(&packed).unwrap();
        assert_eq!(got_name.len(), MAX_FILENAME_LEN);
        assert_eq!(got_content, b"x");
    }

    // ---- unpack error paths ----

    #[test]
    fn unpack_truncated_header() {
        assert_eq!(
            unpack_first_chunk(&[]).unwrap_err(),
            ClipFileError::Truncated
        );
        assert_eq!(
            unpack_first_chunk(&[0x05]).unwrap_err(),
            ClipFileError::Truncated
        );
    }

    #[test]
    fn unpack_truncated_name() {
        // name_len = 10, but only 3 name bytes available.
        let payload = vec![0x0A, 0x00, b'a', b'b', b'c'];
        assert_eq!(
            unpack_first_chunk(&payload).unwrap_err(),
            ClipFileError::Truncated
        );
    }

    #[test]
    fn unpack_invalid_utf8_name() {
        // name_len = 2, bytes 0xFF 0xFE — invalid UTF-8 start sequence.
        let payload = vec![0x02, 0x00, 0xFF, 0xFE, b'x'];
        assert_eq!(
            unpack_first_chunk(&payload).unwrap_err(),
            ClipFileError::BadUtf8
        );
    }

    #[test]
    fn unpack_name_too_long_rejected() {
        // A peer (malicious or buggy) could declare `name_len` up to 65535.
        // `pack_first_chunk` rejects > MAX_FILENAME_LEN on emit, but the
        // receiver must not trust the wire — otherwise we'd sanitize and
        // try to fs::write a multi-kilobyte filename, getting ENAMETOOLONG
        // and log spam.
        let oversize = MAX_FILENAME_LEN + 1;
        let mut payload = Vec::with_capacity(2 + oversize);
        payload.extend_from_slice(&(oversize as u16).to_le_bytes());
        payload.resize(payload.len() + oversize, b'A');
        assert_eq!(
            unpack_first_chunk(&payload).unwrap_err(),
            ClipFileError::NameTooLong,
            "name_len > MAX_FILENAME_LEN must be rejected before any allocation/sanitization"
        );
    }

    #[test]
    fn unpack_name_at_max_length_accepted() {
        // Boundary case — MAX_FILENAME_LEN itself is still valid.
        let mut payload = Vec::with_capacity(2 + MAX_FILENAME_LEN);
        payload.extend_from_slice(&(MAX_FILENAME_LEN as u16).to_le_bytes());
        payload.resize(payload.len() + MAX_FILENAME_LEN, b'A');
        let (name, content) = unpack_first_chunk(&payload).expect("MAX_FILENAME_LEN must be accepted");
        assert_eq!(name.len(), MAX_FILENAME_LEN);
        assert!(content.is_empty());
    }

    // ---- sanitize_basename: path strip ----

    #[test]
    fn sanitize_strips_path() {
        assert_eq!(sanitize_basename("../foo"), "foo");
        assert_eq!(sanitize_basename("..\\foo"), "foo");
        assert_eq!(sanitize_basename("/abs/path/foo"), "foo");
        assert_eq!(sanitize_basename("foo/../bar"), "bar");
        assert_eq!(sanitize_basename("/a/b/c/d.txt"), "d.txt");
        assert_eq!(sanitize_basename("a\\b\\c\\d.txt"), "d.txt");
    }

    // ---- sanitize_basename: Windows drive letter ----

    #[test]
    fn sanitize_strips_windows_drive() {
        assert_eq!(sanitize_basename("C:\\abs\\foo"), "foo");
        assert_eq!(sanitize_basename("C:foo"), "foo");
        assert_eq!(sanitize_basename("C:"), FALLBACK_BASENAME);
        assert_eq!(sanitize_basename("z:\\dir\\file.txt"), "file.txt");
    }

    // ---- sanitize_basename: NTFS reserved names ----

    #[test]
    fn sanitize_reserved_ntfs_names() {
        assert_eq!(sanitize_basename("CON"), "_CON");
        assert_eq!(sanitize_basename("con.txt"), "_con.txt");
        assert_eq!(sanitize_basename("PRN.dat"), "_PRN.dat");
        assert_eq!(sanitize_basename("COM1.log"), "_COM1.log");
        assert_eq!(sanitize_basename("LPT9"), "_LPT9");
        assert_eq!(sanitize_basename("aux"), "_aux");
        assert_eq!(sanitize_basename("nul.bin"), "_nul.bin");
    }

    #[test]
    fn sanitize_reserved_prefix_collision_left_alone() {
        // "console.txt" — stem is "console", not "CON" — must NOT be prefixed.
        assert_eq!(sanitize_basename("console.txt"), "console.txt");
        assert_eq!(sanitize_basename("comma.csv"), "comma.csv");
        assert_eq!(sanitize_basename("auxiliary.log"), "auxiliary.log");
    }

    // ---- sanitize_basename: Windows-reserved chars ----

    #[test]
    fn sanitize_replaces_windows_reserved_chars() {
        // Mac and Linux allow `<>:"|?*` in filenames; Windows fs::write
        // returns InvalidInput, silently losing the file. Substitute with
        // `_` so the cross-platform paste-back path survives.
        assert_eq!(sanitize_basename("foo|bar.txt"), "foo_bar.txt");
        assert_eq!(sanitize_basename("a<b>c?d*e\"f.dat"), "a_b_c_d_e_f.dat");
        // ':' inside a name (not as drive separator) is replaced too —
        // strip_drive_letter only consumes the leading `X:`.
        assert_eq!(sanitize_basename("foo:bar"), "foo_bar");
    }

    #[test]
    fn sanitize_replaces_nul_byte() {
        // Embedded NUL bytes are illegal in filenames on every host OS;
        // replace with `_`.
        assert_eq!(sanitize_basename("foo\0bar.txt"), "foo_bar.txt");
    }

    // ---- sanitize_basename: empty / fallback ----

    #[test]
    fn sanitize_empty_fallback() {
        assert_eq!(sanitize_basename(""), FALLBACK_BASENAME);
    }

    #[test]
    fn sanitize_dot_dot_only() {
        assert_eq!(sanitize_basename(".."), FALLBACK_BASENAME);
        assert_eq!(sanitize_basename("../.."), FALLBACK_BASENAME);
        assert_eq!(sanitize_basename("..\\..\\.."), FALLBACK_BASENAME);
    }

    #[test]
    fn sanitize_whitespace_only() {
        assert_eq!(sanitize_basename("   "), FALLBACK_BASENAME);
        assert_eq!(sanitize_basename("\t\t"), FALLBACK_BASENAME);
    }

    // ---- sanitize_basename: unicode ----

    #[test]
    fn sanitize_unicode_basename() {
        assert_eq!(sanitize_basename("папка/файл 🎉.txt"), "файл 🎉.txt");
        assert_eq!(sanitize_basename("dir\\привет.pdf"), "привет.pdf");
    }

    #[test]
    fn sanitize_keeps_spaces_in_middle() {
        assert_eq!(
            sanitize_basename("file with [spaces] (1).tar.gz"),
            "file with [spaces] (1).tar.gz"
        );
    }

    #[test]
    fn sanitize_strips_leading_colon_or_space() {
        // Leading colon after drive-letter strip should also be trimmed.
        assert_eq!(sanitize_basename(":foo.txt"), "foo.txt");
        assert_eq!(sanitize_basename("   spaced.txt"), "spaced.txt");
    }

    // ---- error display sanity ----

    #[test]
    fn error_display_includes_descriptor() {
        let e = ClipFileError::NameTooLong;
        let s = format!("{e}");
        assert!(s.contains("name"), "got: {s}");
    }

    // ---- constant sanity ----

    #[test]
    fn constants_are_reasonable() {
        assert_eq!(MAX_FILE_BYTES, 20 * 1024 * 1024);
        assert_eq!(MAX_FILENAME_LEN, 4096);
        assert!(!FALLBACK_BASENAME.is_empty());
    }

    #[test]
    fn max_file_payload_bytes_matches_components() {
        // The wire-payload cap must stay in sync with its three component
        // limits — if any of them shifts and this constant doesn't, the
        // size-gate at both sides' `on_offer` will diverge from
        // `pack_first_chunk`'s actual output and we'll either over-allocate
        // (gap > limit) or silently drop valid offers (gap < limit).
        assert_eq!(
            MAX_FILE_PAYLOAD_BYTES,
            MAX_FILE_BYTES + MAX_FILENAME_LEN + 2
        );
    }
}
