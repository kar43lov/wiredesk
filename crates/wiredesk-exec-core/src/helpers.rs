//! Pure helpers shared by `wd --exec` standalone path and the GUI's
//! IPC handler — sentinel formatting, line classification, ANSI
//! stripping, output slicing.

use std::io::Read;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use flate2::read::GzDecoder;

use crate::types::{ExecError, ShellKind};

/// Build the `<command>; <emit-sentinel>` payload for the runner.
///
/// PowerShell variant:
///   - `$LASTEXITCODE = 0` — pre-init the variable. Cmdlets (like
///     `echo`/`Write-Output`) do NOT set `$LASTEXITCODE`, only
///     external commands do. Without pre-init `$LASTEXITCODE` may be
///     `$null` and the interpolated sentinel becomes
///     `__WD_DONE_<uuid>__` (no integer tail), which `parse_sentinel`
///     correctly rejects → runner hangs to `--timeout`. This was the
///     root cause of the very first sentinel-never-arrives bug.
///   - `try { <cmd> } catch { $LASTEXITCODE = 1 }` — catches *terminating*
///     errors (`Get-Item /nonexistent`, mistyped cmdlet) so the
///     sentinel still emits. Without try/catch, a terminating error
///     skips the trailing statement.
///   - The trailing string is just emitted to the success stream;
///     PS prints it on its own line via implicit Write-Output.
///
/// Bash variant uses `$?` — bash always sets it after every command,
/// terminating or not. Bash also continues past a non-zero exit in a
/// `;`-list, so a plain `cmd; echo "<sentinel>"` is enough.
///
/// Line terminator is bare `\n` — PowerShell stdin in pipe mode does
/// NOT treat a lone `\r` as end-of-line and parks the line in its
/// read buffer waiting for `\n`.
pub fn format_command(uuid: &uuid::Uuid, kind: ShellKind, cmd: &str) -> String {
    match kind {
        // `$ErrorActionPreference='Stop'` flips PS *non-terminating*
        // errors into terminating ones for the duration of this line.
        // Without it, `Get-Item /nonexistent` writes to the error
        // stream, returns control, and the catch block never fires —
        // `$LASTEXITCODE` stays 0 → `--exec` returns 0 for an
        // obviously-failed command (the original AC2a regression).
        ShellKind::PowerShell => format!(
            "$LASTEXITCODE=0; $ErrorActionPreference='Stop'; try {{ {cmd} }} catch {{ $LASTEXITCODE=1 }}; \"__WD_DONE_{uuid}__$LASTEXITCODE\"\n"
        ),
        // Bash sandwich: READY marker BEFORE the command and DONE
        // sentinel AFTER. READY is the lower-bound that lets
        // clean_stdout slice off MOTD / SSH banner / prompt fragments.
        ShellKind::Bash => format!(
            "echo __WD_READY_{uuid}__; {cmd}; echo \"__WD_DONE_{uuid}__$?\"\n"
        ),
    }
}

/// Strip ANSI/VT100 escape sequences from a string so prompt-detection
/// can match a real Starship/oh-my-zsh prompt that arrives wrapped in
/// color and terminal-mode escapes. Real-world `ssh -tt` Starship
/// trace ends a prompt line with `➜ \x1b[K\x1b[?1h\x1b=\x1b[?2004h` —
/// `is_remote_prompt` against the raw string fails (last char is `h`,
/// not `➜`/`$`/`#`).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some(&'[') => {
                chars.next();
                for nc in chars.by_ref() {
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(&']') => {
                chars.next();
                while let Some(nc) = chars.next() {
                    if nc == '\x07' {
                        break;
                    }
                    if nc == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {
                chars.next();
            }
        }
    }
    out
}

/// Test whether a line is the expanded READY marker emitted by the
/// Bash sandwich. We trim before comparison because remote `ssh -tt`
/// echoes stdin, so the *unexpanded* literal `echo __WD_READY_<uuid>__`
/// also surfaces — `parse_ready` only matches the *expanded* form.
pub fn parse_ready(line: &str, uuid: &uuid::Uuid) -> bool {
    line.trim() == format!("__WD_READY_{uuid}__")
}

/// Format a diagnostic message for `--exec` timeout. Includes the
/// last 256 bytes of the wire log so the user can see where things
/// stalled (mid-MOTD vs after READY-marker vs mid-command output).
pub fn format_timeout_diagnostic(buf: &str, timeout_secs: u64) -> String {
    let bytes = buf.as_bytes();
    let start = bytes.len().saturating_sub(256);
    let tail = String::from_utf8_lossy(&bytes[start..]);
    format!(
        "wiredesk-term: --exec timeout after {timeout_secs}s (no sentinel from host)\nlast bytes received: {tail:?}"
    )
}

/// Slice the accumulated output buffer down to *just* what `<cmd>`
/// produced. The wire-stream of one runner execution roughly looks
/// like:
///
/// ```text
/// [host MOTD / SSH banner / pre-prompt noise]
/// __WD_READY_<uuid>__              <- only in --ssh (Bash) path
/// [echoed command with sentinel format string]   <- only in --ssh path
/// [actual stdout of <cmd>]
/// __WD_DONE_<uuid>__<exit_code>    <- expanded sentinel
/// ```
///
/// Lower bound: prefer the READY marker (Bash path); fall back to the
/// last prompt line (PS-only path).
///
/// Upper bound: the sentinel line. Sentinel itself is dropped.
pub fn clean_stdout(buf: &str, uuid: &uuid::Uuid) -> String {
    let lines: Vec<&str> = buf.split('\n').collect();
    let prefix = format!("__WD_DONE_{uuid}__");

    let sentinel_idx = lines
        .iter()
        .position(|l| parse_sentinel(l, uuid).is_some());
    let upper = sentinel_idx.unwrap_or(lines.len());

    let ready_idx = lines[..upper]
        .iter()
        .position(|l| parse_ready(l, uuid));
    let lower = if let Some(idx) = ready_idx {
        idx + 1
    } else {
        let prompt_idx = lines[..upper]
            .iter()
            .rposition(|l| is_powershell_prompt(l) || is_remote_prompt(l));
        prompt_idx.map(|i| i + 1).unwrap_or(0)
    };

    let done_echo = format!("__WD_DONE_{uuid}__$");
    let ready_echo = format!("__WD_READY_{uuid}__");
    let echo_check = |s: &str| {
        !(s.contains(&done_echo) || s.contains("echo ") && s.contains(&ready_echo))
    };

    let mut kept: Vec<String> = lines[lower..upper]
        .iter()
        .copied()
        .filter(|l| echo_check(l))
        .map(|l| l.to_string())
        .collect();

    // Sentinel-line may carry pre-prefix output when the command's
    // stdout had no trailing newline. The Bash sandwich glues the
    // sentinel onto it. Recover that prefix portion as the last line.
    if let Some(idx) = sentinel_idx {
        let line = lines[idx];
        if let Some(pos) = line.rfind(&prefix) {
            if pos > 0 {
                let pre = line[..pos].trim_end_matches('\r');
                if !pre.is_empty() && echo_check(pre) {
                    kept.push(pre.to_string());
                }
            }
        }
    }

    let mut out = kept.join("\n");
    while out.ends_with('\n') || out.ends_with('\r') {
        out.pop();
    }
    out
}

/// Parse a line for our sentinel marker. Returns `Some(exit_code)` when
/// `__WD_DONE_<our-uuid>__<digits>` appears anywhere in the line.
///
/// We anchor with `rfind` (not `strip_prefix`) because Bash sandwich
/// `<cmd>; echo "__WD_DONE_<uuid>__$?"` glues the sentinel directly
/// onto unterminated `<cmd>` output (e.g. `head -c 800` on a JSON
/// payload without trailing newline).
pub fn parse_sentinel(line: &str, uuid: &uuid::Uuid) -> Option<i32> {
    let prefix = format!("__WD_DONE_{uuid}__");
    let trimmed = line.trim();
    let pos = trimmed.rfind(&prefix)?;
    let rest = &trimmed[pos + prefix.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse::<i32>().ok()
}

/// `true` when `line` looks like a Windows PowerShell prompt:
/// `PS X:\…> ` or `PS C:\Users\User\path>`.
pub fn is_powershell_prompt(line: &str) -> bool {
    let s = line.trim_end();
    if !s.starts_with("PS ") {
        return false;
    }
    let bytes = s.as_bytes();
    if bytes.len() < 6 {
        return false;
    }
    if !bytes[3].is_ascii_uppercase() || bytes[4] != b':' {
        return false;
    }
    s.ends_with('>')
}

/// `true` when `line` looks like a remote shell prompt (the kind that
/// follows a successful `ssh -tt` hop). Recognises the common endings:
/// `$ ` (plain bash), `# ` (root bash), and Starship's `➜` glyph.
pub fn is_remote_prompt(line: &str) -> bool {
    let s = line.trim_end();
    if s.is_empty() {
        return false;
    }
    s.ends_with('$') || s.ends_with('#') || s.ends_with('➜')
}

/// Build the `--compress` variant of the sentinel-bearing payload.
///
/// Both shell variants emit the user command's exit code as an
/// **in-band marker** `__WD_RC__<rc>__` at the very end of the
/// compressed stream, NOT in the post-pipe sentinel. Reasoning:
/// `${PIPESTATUS[0]}` is bash-only (breaks on `sh`/`dash` remote
/// login shells), and `{ cmd; rc=$?; } | gzip` loses rc to the
/// pipe-subshell anyway. Putting rc inside the gzipped payload
/// makes the wrapper POSIX-portable on the bash side and uniform
/// across both paths. The `__WD_DONE_<uuid>__0` sentinel becomes
/// a fixed marker — runner extracts the real rc from the in-band
/// marker after decode (see `extract_compressed_rc`).
///
/// Bash variant pipes stdout (with stderr merged via `2>&1`) plus
/// the rc-marker through `gzip -c | base64`. Trailing `echo` before
/// the sentinel guarantees `\n` boundary so the last base64 line
/// can't glue onto `__WD_DONE_`.
///
/// PowerShell variant appends the rc-marker to `$out` before
/// `[Text.Encoding]::UTF8.GetBytes`, then gzips and emits as one
/// base64 string. `[Console]::OutputEncoding = UTF8` is harmless
/// here (sentinel is ASCII), kept for symmetry. Non-ASCII source
/// literals (e.g. Cyrillic in script text) parse via PS console
/// input encoding before this line runs and arrive as mojibake —
/// known limitation, not fixable in pipe-mode without multi-line
/// codepage handshake.
pub fn format_compressed_command(uuid: &uuid::Uuid, kind: ShellKind, cmd: &str) -> String {
    match kind {
        ShellKind::Bash => format!(
            "echo __WD_READY_{uuid}__; {{ {cmd} 2>&1; printf \"__WD_RC__%s__\\n\" \"$?\"; }} | gzip -c | base64; echo; echo \"__WD_DONE_{uuid}__0\"\n"
        ),
        ShellKind::PowerShell => format!(
            "[Console]::OutputEncoding = [Text.Encoding]::UTF8; \
             Write-Output \"__WD_READY_{uuid}__\"; \
             $LASTEXITCODE=0; $ErrorActionPreference='Stop'; \
             try {{ $out = & {{ {cmd} }} 2>&1 | Out-String }} catch {{ $out = $_.ToString(); $LASTEXITCODE=1 }}; \
             $rc = $LASTEXITCODE; \
             $ms = New-Object System.IO.MemoryStream; \
             $gz = New-Object System.IO.Compression.GZipStream($ms, [System.IO.Compression.CompressionMode]::Compress); \
             $bytes = [Text.Encoding]::UTF8.GetBytes($out + \"__WD_RC__\" + $rc + \"__\"); \
             $gz.Write($bytes, 0, $bytes.Length); $gz.Close(); \
             Write-Output ([Convert]::ToBase64String($ms.ToArray())); \
             Write-Output \"__WD_DONE_{uuid}__0\"\n"
        ),
    }
}

/// Strip the trailing `__WD_RC__<rc>__` marker from decoded compress
/// output and return `(payload_without_marker, rc)`. If no valid
/// marker is found, returns `(bytes, 0)` — runner treats it as
/// success but leaves payload untouched.
///
/// Byte-oriented (not str-oriented) so binary cmd output (e.g.
/// `cat /usr/bin/ls`) doesn't corrupt the search. We only look at
/// the trailing 64 bytes since the marker is ~16 bytes and always
/// at end. If user's binary somehow contains the marker pattern in
/// the middle, `rposition` finds the LAST occurrence — which is
/// our marker, near end.
///
/// We do NOT strip the byte before the marker — both wrappers
/// append the marker DIRECTLY to cmd output (bash: `{ cmd; printf
/// "__WD_RC__%s__\n" "$?"; } | gzip`; PS: `$out + "__WD_RC__" +
/// $rc + "__"`). Any trailing `\n` (or `\r\n`) seen there is part
/// of the cmd's own output (`ls` always emits `\n` after each
/// entry) and should be preserved byte-for-byte to match the
/// non-compress baseline (AC2: stdout byte-identical).
pub fn extract_compressed_rc(bytes: Vec<u8>) -> (Vec<u8>, i32) {
    let needle = b"__WD_RC__";
    let len = bytes.len();
    let search_start = len.saturating_sub(64);
    let tail = &bytes[search_start..];

    let pos_in_tail = match tail.windows(needle.len()).rposition(|w| w == needle) {
        Some(p) => p,
        None => return (bytes, 0),
    };
    let abs_pos = search_start + pos_in_tail;
    let after = &bytes[abs_pos + needle.len()..];

    let end_pos = match after.windows(2).position(|w| w == b"__") {
        Some(p) => p,
        None => return (bytes, 0),
    };

    let rc_str = match std::str::from_utf8(&after[..end_pos]) {
        Ok(s) => s,
        Err(_) => return (bytes, 0),
    };
    let rc = match rc_str.parse::<i32>() {
        Ok(r) => r,
        Err(_) => return (bytes, 0),
    };

    (bytes[..abs_pos].to_vec(), rc)
}

/// Decode a base64-of-gzip payload back into raw bytes.
///
/// The host wrapper for `--compress` mode emits stdout as
/// `gzip -c | base64` (76-char wrapped) for bash, or
/// `[Convert]::ToBase64String([System.IO.Compression.GZipStream]...)` for
/// PowerShell. Both can include `\r\n` between base64 lines, so we strip
/// all whitespace before decoding.
///
/// Errors are wrapped as `ExecError::CompressionFailed` regardless of
/// stage (bad base64, truncated gzip header, malformed deflate stream)
/// so the runner can surface a single error type.
pub fn decode_compressed_stream(input: &str) -> Result<Vec<u8>, ExecError> {
    let stripped: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    if stripped.is_empty() {
        return Err(ExecError::CompressionFailed("empty input".into()));
    }
    let raw = STANDARD
        .decode(stripped.as_bytes())
        .map_err(|e| ExecError::CompressionFailed(format!("base64: {e}")))?;
    let mut decoder = GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    decoder
        .read_to_end(&mut out)
        .map_err(|e| ExecError::CompressionFailed(format!("gzip: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ShellKind;

    // --- is_powershell_prompt ---

    #[test]
    fn is_powershell_prompt_classic() {
        assert!(is_powershell_prompt("PS C:\\>"));
        assert!(is_powershell_prompt("PS C:\\Users\\User>"));
        assert!(is_powershell_prompt("PS C:\\Users\\User> "));
    }

    #[test]
    fn is_powershell_prompt_other_drives() {
        assert!(is_powershell_prompt("PS D:\\Projects\\foo>"));
        assert!(is_powershell_prompt("PS Z:\\>"));
    }

    #[test]
    fn is_powershell_prompt_rejects_non_prompt() {
        assert!(!is_powershell_prompt(""));
        assert!(!is_powershell_prompt("PS"));
        assert!(!is_powershell_prompt("PS >"));
        assert!(!is_powershell_prompt("bash$"));
        assert!(!is_powershell_prompt("> ls"));
        assert!(!is_powershell_prompt("PS c:\\>")); // lowercase drive — reject
    }

    // --- is_remote_prompt ---

    #[test]
    fn is_remote_prompt_bash_user() {
        assert!(is_remote_prompt("user@host:~$"));
        assert!(is_remote_prompt("user@host:~$ "));
    }

    #[test]
    fn is_remote_prompt_bash_root() {
        assert!(is_remote_prompt("root@host:/#"));
        assert!(is_remote_prompt("root@host:/# "));
    }

    #[test]
    fn is_remote_prompt_starship() {
        // Starship renders cwd on a separate info-line; the prompt
        // cursor line is just `➜ `.
        assert!(is_remote_prompt("➜"));
        assert!(is_remote_prompt("➜ "));
        assert!(is_remote_prompt("karlovpg in 🌐 knd02 in ~ ➜ "));
    }

    #[test]
    fn is_remote_prompt_rejects_non_prompt() {
        assert!(!is_remote_prompt(""));
        assert!(!is_remote_prompt("Welcome to Ubuntu 20.04.6 LTS"));
        assert!(!is_remote_prompt("karlovpg in 🌐 knd02 in ~"));
        assert!(!is_remote_prompt("PS C:\\>"));
    }

    // --- format_command ---

    #[test]
    fn format_command_powershell_wraps_in_try_catch() {
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::PowerShell, "Get-ChildItem");
        assert!(
            s.starts_with("$LASTEXITCODE=0;"),
            "PS payload must pre-init $LASTEXITCODE so cmdlet success → 0: {s}"
        );
        assert!(
            s.contains("try { Get-ChildItem }"),
            "PS payload must wrap cmd in try/catch: {s}"
        );
        assert!(
            s.contains("catch { $LASTEXITCODE=1 }"),
            "PS payload must set $LASTEXITCODE on terminating error: {s}"
        );
        assert!(
            s.contains("$LASTEXITCODE"),
            "PS sentinel must use $LASTEXITCODE: {s}"
        );
        assert!(s.ends_with('\n'), "payload must end with LF for host stdin: {s}");
    }

    #[test]
    fn format_command_powershell_cmdlet_yields_zero_exit() {
        // Regression: pre-init `$LASTEXITCODE=0` is what makes
        // sentinel parsing work for cmdlets — without it, PS would
        // interpolate `$null` and the wire line becomes
        // `__WD_DONE_<uuid>__` (no integer tail).
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::PowerShell, "echo hello");
        let simulated_wire_line = format!("__WD_DONE_{uuid}__0");
        assert_eq!(parse_sentinel(&simulated_wire_line, &uuid), Some(0));
        assert!(s.contains("__WD_DONE_") && s.contains("$LASTEXITCODE"));
    }

    #[test]
    fn format_command_bash_appends_sentinel() {
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::Bash, "docker ps");
        assert!(
            s.starts_with("echo __WD_READY_"),
            "bash payload must start with READY emitter: {s}"
        );
        assert!(s.contains("docker ps;"), "bash payload must contain cmd: {s}");
        assert!(s.contains("$?"), "bash sentinel must reference $?: {s}");
        assert!(
            !s.contains("$LASTEXITCODE"),
            "bash payload must NOT use $LASTEXITCODE: {s}"
        );
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn format_command_bash_includes_ready_marker() {
        // Regression: READY marker before the cmd is what makes
        // clean_stdout slice MOTD / `ssh -tt` PTY warning / banner
        // off the output.
        let uuid = uuid::Uuid::nil();
        let s = format_command(&uuid, ShellKind::Bash, "ls");
        let ready_marker = format!("__WD_READY_{uuid}__");
        assert!(s.contains(&ready_marker), "missing READY marker: {s}");
        let ready_pos = s.find(&ready_marker).unwrap();
        let cmd_pos = s.find("ls;").unwrap();
        assert!(ready_pos < cmd_pos, "READY must come before cmd: {s}");
    }

    #[test]
    fn format_command_uuid_in_payload() {
        let uuid_a = uuid::Uuid::nil();
        let uuid_b = uuid::Uuid::from_u128(0x1234_5678_90ab_cdef_1234_5678_90ab_cdef);
        let a1 = format_command(&uuid_a, ShellKind::Bash, "ls");
        let a2 = format_command(&uuid_a, ShellKind::Bash, "ls");
        let b = format_command(&uuid_b, ShellKind::Bash, "ls");
        assert_eq!(a1, a2, "same UUID + same args should be deterministic");
        assert_ne!(a1, b, "different UUID → different payload");
        assert!(a1.contains(&uuid_a.to_string()));
        assert!(b.contains(&uuid_b.to_string()));
    }

    // --- format_timeout_diagnostic ---

    #[test]
    fn format_timeout_diagnostic_truncates_and_handles_utf8() {
        let long = "X".repeat(1024);
        let out = format_timeout_diagnostic(&long, 30);
        assert!(out.contains("--exec timeout after 30s"));
        let x_count = out.matches('X').count();
        assert_eq!(x_count, 256, "expected last 256 X's, got {x_count}");

        let out = format_timeout_diagnostic("", 5);
        assert!(out.contains("--exec timeout after 5s"));
        assert!(out.contains("last bytes received: \"\""));

        // Buffer ending mid-cyrillic multi-byte char → no panic.
        let mut buf = String::from("a");
        for _ in 0..128 {
            buf.push('к'); // 256 bytes of cyrillic
        }
        let out = format_timeout_diagnostic(&buf, 1);
        assert!(out.contains("--exec timeout after 1s"));
        assert!(out.is_ascii() || out.chars().all(|c| !c.is_control() || c == '\n'));
    }

    // --- strip_ansi ---

    #[test]
    fn strip_ansi_csi_color_codes() {
        assert_eq!(
            strip_ansi("\x1b[1;33muser\x1b[0m in \x1b[1;36m~\x1b[0m"),
            "user in ~"
        );
    }

    #[test]
    fn strip_ansi_keeps_unicode_arrow() {
        assert_eq!(strip_ansi("➜ \x1b[K\x1b[?1h\x1b=\x1b[?2004h"), "➜ ");
    }

    #[test]
    fn strip_ansi_leaves_plain_text_unchanged() {
        assert_eq!(strip_ansi("just text"), "just text");
        assert_eq!(strip_ansi(""), "");
        assert_eq!(strip_ansi("PS C:\\>"), "PS C:\\>");
    }

    #[test]
    fn strip_ansi_starship_full_prompt_line_matches_remote_prompt() {
        let raw = "\r\u{1b}[0m\u{1b}[27m\u{1b}[24m\u{1b}[J\u{1b}[1;33muser\u{1b}[0m in \u{1b}[1;2;32m🌐 cgu-knd-firecards-1\u{1b}[0m in \u{1b}[1;36m~\u{1b}[0m \r\n➜ \u{1b}[K\u{1b}[?1h\u{1b}=\u{1b}[?2004h";
        let stripped = strip_ansi(raw);
        assert!(
            is_remote_prompt(stripped.trim_end()),
            "stripped Starship prompt should match is_remote_prompt: {stripped:?}"
        );
    }

    // --- parse_ready ---

    #[test]
    fn parse_ready_matches_expanded_only() {
        let uuid = uuid::Uuid::nil();
        assert!(parse_ready(&format!("__WD_READY_{uuid}__"), &uuid));
        assert!(parse_ready(&format!("  __WD_READY_{uuid}__  "), &uuid));
        // Stdin echo from `ssh -tt` (literal `echo …`) — must NOT match.
        assert!(!parse_ready(&format!("echo __WD_READY_{uuid}__"), &uuid));
        let other = uuid::Uuid::from_u128(1);
        assert!(!parse_ready(&format!("__WD_READY_{other}__"), &uuid));
        assert!(!parse_ready("", &uuid));
        assert!(!parse_ready("hello", &uuid));
    }

    // --- clean_stdout ---

    #[test]
    fn clean_stdout_ps_only_mode() {
        let uuid = uuid::Uuid::nil();
        let buf = format!(
            "Some pre-prompt noise\nPS C:\\Users\\User>\nactual line 1\nactual line 2\n__WD_DONE_{uuid}__0\n"
        );
        assert_eq!(clean_stdout(&buf, &uuid), "actual line 1\nactual line 2");
    }

    #[test]
    fn clean_stdout_ssh_mode_strips_motd_and_echo() {
        let uuid = uuid::Uuid::nil();
        let buf = format!(
            "Welcome to Ubuntu\nMOTD line 1\nMOTD line 2\n\
             echo __WD_READY_{uuid}__; docker ps; echo \"__WD_DONE_{uuid}__$?\"\n\
             __WD_READY_{uuid}__\n\
             row1\nrow2\n\
             __WD_DONE_{uuid}__0\n"
        );
        let out = clean_stdout(&buf, &uuid);
        assert!(!out.contains("Welcome"), "MOTD must be stripped: {out:?}");
        assert!(!out.contains("__WD_READY"), "READY echo must be stripped: {out:?}");
        assert!(!out.contains("__WD_DONE"), "echoed/expanded sentinel must be stripped: {out:?}");
        assert!(!out.contains("docker ps;"), "echoed cmd line should be gone: {out:?}");
        assert_eq!(out, "row1\nrow2");
    }

    #[test]
    fn clean_stdout_no_prompt_returns_pre_sentinel() {
        let uuid = uuid::Uuid::nil();
        let buf = format!("output line\n__WD_DONE_{uuid}__0\n");
        assert_eq!(clean_stdout(&buf, &uuid), "output line");
    }

    #[test]
    fn clean_stdout_uuid_disambiguates() {
        let ours = uuid::Uuid::nil();
        let theirs = uuid::Uuid::from_u128(1);
        let buf = format!(
            "PS C:\\>\nleftover from earlier\n__WD_DONE_{theirs}__0\nour output\n__WD_DONE_{ours}__0\n"
        );
        let out = clean_stdout(&buf, &ours);
        assert!(out.contains("our output"));
        assert!(out.contains(&theirs.to_string()));
    }

    #[test]
    fn clean_stdout_no_sentinel_returns_post_prompt() {
        let uuid = uuid::Uuid::nil();
        let buf = "PS C:\\>\nstuff\n";
        assert_eq!(clean_stdout(buf, &uuid), "stuff");
    }

    #[test]
    fn clean_stdout_recovers_prefix_from_mixed_sentinel_line() {
        // Regression: command output without trailing newline glues the
        // sentinel onto its last line. clean_stdout must extract the
        // prefix (the actual stdout) and drop the sentinel.
        let uuid = uuid::Uuid::nil();
        let buf = format!(
            "__WD_READY_{uuid}__\n\
             {{\"hits\":{{\"total\":42}}}}__WD_DONE_{uuid}__0\n"
        );
        let out = clean_stdout(&buf, &uuid);
        assert!(
            out.contains("{\"hits\":{\"total\":42}}"),
            "expected JSON output preserved: {out:?}"
        );
        assert!(
            !out.contains("__WD_DONE_"),
            "sentinel must not leak into stdout: {out:?}"
        );
    }

    // --- parse_sentinel ---

    #[test]
    fn parse_sentinel_matches_zero() {
        let uuid = uuid::Uuid::nil();
        let s = format!("__WD_DONE_{uuid}__0");
        assert_eq!(parse_sentinel(&s, &uuid), Some(0));
    }

    #[test]
    fn parse_sentinel_matches_nonzero() {
        let uuid = uuid::Uuid::nil();
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__7"), &uuid), Some(7));
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__124"), &uuid), Some(124));
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__9\r"), &uuid), Some(9));
    }

    #[test]
    fn parse_sentinel_rejects_stdin_echo() {
        // Host PS echoing the format-string back: literal $LASTEXITCODE / $?.
        let uuid = uuid::Uuid::nil();
        assert_eq!(
            parse_sentinel(&format!("__WD_DONE_{uuid}__$LASTEXITCODE"), &uuid),
            None
        );
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__$?"), &uuid), None);
    }

    #[test]
    fn parse_sentinel_rejects_other_uuid() {
        let ours = uuid::Uuid::nil();
        let theirs = uuid::Uuid::from_u128(1);
        let line = format!("__WD_DONE_{theirs}__0");
        assert_eq!(parse_sentinel(&line, &ours), None);
    }

    #[test]
    fn parse_sentinel_rejects_garbage() {
        let uuid = uuid::Uuid::nil();
        assert_eq!(parse_sentinel("", &uuid), None);
        assert_eq!(parse_sentinel("hello world", &uuid), None);
        assert_eq!(parse_sentinel("__WD_DONE__0", &uuid), None);
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__"), &uuid), None);
        assert_eq!(parse_sentinel(&format!("__WD_DONE_{uuid}__abc"), &uuid), None);
    }

    #[test]
    fn parse_sentinel_after_unterminated_output() {
        // Regression: command emits stdout without trailing newline,
        // bash sandwich glues sentinel directly onto it. rfind must
        // still locate the marker mid-string.
        let uuid = uuid::Uuid::nil();
        let glued = format!("<long unterminated json>__WD_DONE_{uuid}__0");
        assert_eq!(parse_sentinel(&glued, &uuid), Some(0));

        let glued_nonzero = format!("xxxxx__WD_DONE_{uuid}__7");
        assert_eq!(parse_sentinel(&glued_nonzero, &uuid), Some(7));
    }

    #[test]
    fn parse_sentinel_with_trailing_garbage() {
        let uuid = uuid::Uuid::nil();
        let with_ansi = format!("__WD_DONE_{uuid}__42\x1b[K\x1b[?2004h");
        assert_eq!(parse_sentinel(&with_ansi, &uuid), Some(42));
    }

    #[test]
    fn parse_sentinel_prefers_expanded_over_echo_in_same_line() {
        let uuid = uuid::Uuid::nil();
        let mixed = format!(
            "echo \"__WD_DONE_{uuid}__$?\" some-output __WD_DONE_{uuid}__7"
        );
        assert_eq!(parse_sentinel(&mixed, &uuid), Some(7));
    }

    // --- format_compressed_command ---

    #[test]
    fn format_compressed_bash_shape() {
        let uuid = uuid::Uuid::nil();
        let out = format_compressed_command(&uuid, ShellKind::Bash, "ls -la");
        assert!(out.contains(&format!("__WD_READY_{uuid}__")));
        assert!(out.contains("{ ls -la 2>&1; printf \"__WD_RC__%s__\\n\" \"$?\"; } | gzip -c | base64"));
        // sentinel rc is hardcoded 0 — real rc is in-band via __WD_RC__
        assert!(out.contains(&format!("__WD_DONE_{uuid}__0")));
        // explicit echo before the DONE sentinel so a trailing
        // base64 line cannot glue onto the marker.
        assert!(out.contains("| base64; echo; echo"));
        // No bashism left over.
        assert!(!out.contains("PIPESTATUS"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn format_compressed_powershell_shape() {
        let uuid = uuid::Uuid::nil();
        let out = format_compressed_command(&uuid, ShellKind::PowerShell, "Get-ChildItem");
        assert!(out.contains("[Console]::OutputEncoding = [Text.Encoding]::UTF8"));
        assert!(out.contains(&format!("__WD_READY_{uuid}__")));
        assert!(out.contains("$LASTEXITCODE=0"));
        assert!(out.contains("$ErrorActionPreference='Stop'"));
        assert!(out.contains("try { $out = & { Get-ChildItem } 2>&1 | Out-String }"));
        assert!(out.contains("catch { $out = $_.ToString(); $LASTEXITCODE=1 }"));
        assert!(out.contains("System.IO.Compression.GZipStream"));
        assert!(out.contains("[Convert]::ToBase64String"));
        // PS wrapper appends __WD_RC__$rc__ to $out before encoding.
        assert!(out.contains("$out + \"__WD_RC__\" + $rc + \"__\""));
        assert!(out.contains(&format!("__WD_DONE_{uuid}__0")));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn format_compressed_bash_preserves_quotes() {
        let uuid = uuid::Uuid::nil();
        let out = format_compressed_command(
            &uuid,
            ShellKind::Bash,
            "echo 'single' && echo \"double\"",
        );
        assert!(out.contains("{ echo 'single' && echo \"double\" 2>&1;"));
    }

    #[test]
    fn format_compressed_powershell_preserves_cmd_verbatim() {
        let uuid = uuid::Uuid::nil();
        let out = format_compressed_command(
            &uuid,
            ShellKind::PowerShell,
            "Get-Content C:\\test.txt",
        );
        assert!(out.contains("& { Get-Content C:\\test.txt }"));
    }

    #[test]
    fn format_compressed_uuid_consistent_in_both_markers() {
        let uuid = uuid::Uuid::new_v4();
        for kind in [ShellKind::Bash, ShellKind::PowerShell] {
            let out = format_compressed_command(&uuid, kind, "echo hi");
            // Both markers must use the same uuid.
            assert!(out.matches(&uuid.to_string()).count() >= 2, "kind={kind:?}");
        }
    }

    // --- extract_compressed_rc ---

    #[test]
    fn extract_rc_preserves_cmd_trailing_newline() {
        // cmd's trailing \n is part of its output (e.g. `ls` always
        // emits \n after each entry) — must NOT be stripped, else
        // AC2 byte-identical fails for typical CLI tools.
        let bytes = b"hello world\n__WD_RC__0__\n".to_vec();
        let (clean, rc) = extract_compressed_rc(bytes);
        assert_eq!(clean, b"hello world\n");
        assert_eq!(rc, 0);
    }

    #[test]
    fn extract_rc_propagates_nonzero() {
        let bytes = b"some output\n__WD_RC__42__\n".to_vec();
        let (clean, rc) = extract_compressed_rc(bytes);
        assert_eq!(clean, b"some output\n");
        assert_eq!(rc, 42);
    }

    #[test]
    fn extract_rc_handles_glued_marker_no_leading_newline() {
        // PS wrapper appends marker directly to $out (no separator).
        let bytes = b"some output__WD_RC__1__".to_vec();
        let (clean, rc) = extract_compressed_rc(bytes);
        assert_eq!(clean, b"some output");
        assert_eq!(rc, 1);
    }

    #[test]
    fn extract_rc_no_marker_returns_zero_and_keeps_bytes() {
        let bytes = b"plain output, no marker".to_vec();
        let (clean, rc) = extract_compressed_rc(bytes.clone());
        assert_eq!(clean, bytes);
        assert_eq!(rc, 0);
    }

    #[test]
    fn extract_rc_byte_safe_for_binary_payload() {
        // Binary cmd output (non-UTF-8 bytes) followed by marker.
        // The intermediate byte (0x42 here) is preserved as part of
        // the original payload — wrapper appends marker directly.
        let mut bytes = vec![0xFFu8, 0xFE, 0x00, 0x80, 0x42];
        bytes.extend_from_slice(b"__WD_RC__7__\n");
        let (clean, rc) = extract_compressed_rc(bytes);
        assert_eq!(clean, &[0xFFu8, 0xFE, 0x00, 0x80, 0x42]);
        assert_eq!(rc, 7);
    }

    #[test]
    fn extract_rc_preserves_binary_with_internal_newlines() {
        // Binary payload that ends with \n (e.g. `ls` output) — \n
        // before marker is part of payload, must be preserved.
        let bytes = b"line1\nline2\n__WD_RC__0__\n".to_vec();
        let (clean, _rc) = extract_compressed_rc(bytes);
        assert_eq!(clean, b"line1\nline2\n");
    }

    #[test]
    fn extract_rc_finds_last_marker_if_multiple() {
        // If user output happens to contain "__WD_RC__N__", we still
        // pick the LAST one (our wrapper marker is always last).
        let bytes = b"fake __WD_RC__99__ in middle\nreal output\n__WD_RC__3__\n".to_vec();
        let (_clean, rc) = extract_compressed_rc(bytes);
        assert_eq!(rc, 3);
    }

    // --- decode_compressed_stream ---

    /// Generate a base64-of-gzipped fixture on the fly so test data
    /// stays consistent with the real wire format. 76-char wrapping
    /// matches what `base64` (no `-w0`) emits on the host side.
    fn make_compressed_b64(payload: &[u8]) -> String {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        let gzipped = encoder.finish().unwrap();
        let raw = STANDARD.encode(&gzipped);
        // Wrap at 76 chars to mimic `base64` default output.
        let mut out = String::with_capacity(raw.len() + raw.len() / 76);
        for (i, ch) in raw.chars().enumerate() {
            if i > 0 && i % 76 == 0 {
                out.push('\n');
            }
            out.push(ch);
        }
        out
    }

    #[test]
    fn decode_valid_singleline() {
        let b64 = make_compressed_b64(b"hello world");
        let single = b64.replace('\n', "");
        assert_eq!(decode_compressed_stream(&single).unwrap(), b"hello world");
    }

    #[test]
    fn decode_valid_multiline_crlf() {
        let payload = b"the quick brown fox jumps over the lazy dog. ".repeat(10);
        let b64 = make_compressed_b64(&payload);
        let crlf = b64.replace('\n', "\r\n");
        assert_eq!(decode_compressed_stream(&crlf).unwrap(), payload);
    }

    #[test]
    fn decode_invalid_base64() {
        let result = decode_compressed_stream("!!!not base64!!!");
        assert!(matches!(result, Err(ExecError::CompressionFailed(_))));
    }

    #[test]
    fn decode_valid_b64_invalid_gzip() {
        // Valid base64 of plain text "hello" — not gzip framed.
        let b64 = STANDARD.encode(b"hello");
        let result = decode_compressed_stream(&b64);
        assert!(matches!(result, Err(ExecError::CompressionFailed(msg)) if msg.contains("gzip")));
    }

    #[test]
    fn decode_empty_string() {
        let result = decode_compressed_stream("");
        assert!(matches!(result, Err(ExecError::CompressionFailed(msg)) if msg.contains("empty")));

        let whitespace_only = decode_compressed_stream("\r\n  \t\n");
        assert!(matches!(whitespace_only, Err(ExecError::CompressionFailed(_))));
    }

    #[test]
    fn decode_cyrillic_payload() {
        let original = "Привет мир\nТестовый файл\n".as_bytes();
        let b64 = make_compressed_b64(original);
        let decoded = decode_compressed_stream(&b64).unwrap();
        assert_eq!(decoded, original);
        assert_eq!(
            std::str::from_utf8(&decoded).unwrap(),
            "Привет мир\nТестовый файл\n"
        );
    }
}
