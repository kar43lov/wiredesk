//! Sentinel-driven runner for `wd --exec`-style execution. Drives an
//! `ExecTransport` to completion: sends the formatted command (with an
//! optional `ssh -tt` hop), reads `ShellOutput` events, walks lines
//! through a phase-tracker, and surfaces post-prefix output to the
//! caller via a streaming callback.
//!
//! Streaming model: chunks reach the caller as soon as they cross the
//! `Mute → Streaming` boundary. There is no "collect everything, then
//! slice" buffering — that gave the AC1 latency budget a hard 30 s
//! floor on long commands. Caller's callback gets each completed line
//! with its trailing `\n` already attached, so the caller can be a
//! dumb `write_all` pipe.

use std::time::{Duration, Instant};

use crate::helpers::{
    decode_compressed_stream, extract_compressed_rc, format_command, format_compressed_command,
    is_powershell_continuation, is_powershell_prompt, is_remote_prompt, parse_ready,
    parse_sentinel, strip_ansi,
};
use crate::transport::ExecTransport;
use crate::types::{ExecError, ExecEvent, OneShotState, ShellKind};
use crate::utf8_stream::Utf8Stream;

/// How long each `recv_event` call may park. Smaller = more frequent
/// timeout-budget re-checks, larger = fewer wakeups. 100 ms matches
/// the host's heartbeat tick and gives ~10 timeout-checks/sec —
/// plenty of resolution against a 90 s budget.
const RECV_TICK: Duration = Duration::from_millis(100);

/// Phase tracker for the line stream. Both muted phases end at the READY
/// marker; they differ in what they do with the lines before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Drop everything until READY. For `--ssh`, where the pre-command
    /// traffic is a login banner and the remote's echo of our own payload.
    Mute,
    /// Drop only what is recognisably pre-command noise - a shell prompt
    /// or a blank line - and pass the rest through. For pipe mode, where
    /// the command's own stderr can reach the queue ahead of the READY the
    /// wrapper wrote to stdout, because the host reads the two streams on
    /// separate threads.
    MuteNoise,
    /// Emit every completed non-echo line through the caller's callback.
    Streaming,
}

/// Drive a single sentinel-bracketed command to completion.
///
/// `on_chunk` is called once per emitted line in non-compress mode,
/// with the trailing `\n` already attached. Pre-sentinel output that
/// lacks a newline (the "unterminated output" case from
/// `parse_sentinel_after_unterminated_output`) is recovered as one
/// final chunk before the runner returns.
///
/// In `compress=true` mode the streaming property is intentionally
/// dropped: post-READY lines (and any pre-sentinel unterminated tail)
/// are accumulated into a single base64 buffer and decoded on
/// sentinel-detect. The caller's callback is then invoked **once**
/// with the decompressed bytes. Trade-off: latency vs throughput;
/// opt-in via the flag.
///
/// Returns `Ok(exit_code)` on success, `Err(ExecError::Timeout(buf))`
/// if the wall-clock budget elapses without the sentinel — `buf`
/// carries the raw wire log so the caller can pass it through
/// `format_timeout_diagnostic`. In compress mode a partial buffer
/// at timeout is **not** decoded (it would be a fragment, not data).
/// Other `ExecError` variants surface transport-layer failures
/// verbatim; `ExecError::CompressionFailed` covers decode errors
/// once the sentinel arrives.
/// How much of the wire log the timeout error carries.
///
/// `format_timeout_diagnostic` prints the last 256 bytes of it, so this is
/// already thirty times what anyone reads — and unlike an unbounded buffer it
/// cannot turn `wd --exec "docker logs"` into a copy of the whole output held
/// in memory beside the stream that is being written out anyway.
const TIMEOUT_LOG_TAIL: usize = 8 * 1024;

/// Append `text`, keeping at most `cap` bytes of the tail.
///
/// Cutting a `String` by byte offset panics unless the offset is a character
/// boundary, and this buffer is full of multi-byte output by definition — so
/// the cut walks forward to the next boundary instead of trusting the
/// arithmetic.
fn push_bounded_tail(buf: &mut String, text: &str, cap: usize) {
    buf.push_str(text);
    if buf.len() <= cap {
        return;
    }
    let want = buf.len() - cap;
    let cut = (want..=buf.len())
        .find(|i| buf.is_char_boundary(*i))
        .unwrap_or(buf.len());
    buf.drain(..cut);
}

pub fn run_oneshot<T, F>(
    transport: &mut T,
    cmd: &str,
    ssh: Option<&str>,
    timeout_secs: u64,
    compress: bool,
    mut on_chunk: F,
) -> Result<i32, ExecError>
where
    T: ExecTransport,
    F: FnMut(&[u8]),
{
    let uuid = uuid::Uuid::new_v4();
    let target_kind = if ssh.is_some() {
        ShellKind::Bash
    } else {
        ShellKind::PowerShell
    };
    let payload = if compress {
        format_compressed_command(&uuid, target_kind, cmd)
    } else {
        format_command(&uuid, target_kind, cmd)
    };
    log::debug!("[exec] uuid={uuid} kind={target_kind:?} compress={compress} payload={payload:?}");

    // SSH path: hop first, wait for *remote* prompt before sending payload.
    // PS path: pipe-mode reads stdin line-by-line, no need to sync.
    let mut state = if let Some(alias) = ssh {
        let ssh_cmd = format!("ssh -tt {alias}\n");
        log::debug!("[exec] ssh hop: {ssh_cmd:?}");
        transport.send_input(ssh_cmd.as_bytes())?;
        OneShotState::AwaitingRemotePrompt
    } else {
        log::debug!("[exec] sending payload");
        transport.send_input(payload.as_bytes())?;
        OneShotState::AwaitingSentinel
    };

    // Every wrapper `format_command` / `format_compressed_command` builds
    // now opens with the READY marker, on both shells and in both modes,
    // so the lower bound of real output is a marker rather than a guess.
    //
    // How strictly that is applied depends on what is upstream. Over
    // `--ssh` everything before READY is known noise - MOTD, the remote's
    // own echo of our payload - and dropping all of it is the point.
    //
    // In plain pipe mode it cannot be: the host reads the shell's stdout
    // and stderr on two separate threads into one queue, so a line the
    // *command* wrote to stderr can overtake the READY the wrapper wrote
    // to stdout a moment earlier. Dropping everything pre-READY would
    // silently eat it. Only the noise that path actually produces gets
    // dropped there - a prompt, a blank line - and anything else is
    // passed through.
    let mut phase = if ssh.is_some() {
        Phase::Mute
    } else {
        Phase::MuteNoise
    };

    let prefix = format!("__WD_DONE_{uuid}__");
    let done_echo = format!("__WD_DONE_{uuid}__$");
    let ready_echo = format!("__WD_READY_{uuid}__");
    // Stdin-echo filter: drop the literal echoes of our own payload that a
    // shell mirrors back at us - `ssh -tt` does it for the remote command,
    // and PowerShell does it for every line it reads from a redirected
    // stdin, prompt and all.
    //
    // Two signatures, and between them they cover every wrapper:
    //  - the unexpanded sentinel formatter (`__WD_DONE_<uuid>__$…`), which
    //    only ever appears in the source of the line, never in its output;
    //  - the READY marker *inside a longer line*. The expanded marker
    //    stands alone on its own line by construction, so a line that
    //    carries it together with anything else is the source being echoed
    //    back. This is what catches the first line of a PowerShell payload,
    //    which arrives glued to the prompt (`PS C:\…> $LASTEXITCODE=0; …`)
    //    and, before 2026-09-11, was only recognised when the user's own
    //    command happened to contain the word `echo`.
    //
    // The uuid is fresh per run, so neither signature can be produced by
    // the command's own output.
    let is_echo_line =
        |s: &str| s.contains(&done_echo) || (s.contains(&ready_echo) && s.trim() != ready_echo);

    /// In compress mode, the wire stream between READY and DONE must
    /// be pure base64 (with whitespace tolerated). Anything else is
    /// noise — stray PS error formatting, ssh-tt echo fragments,
    /// banners — that would corrupt the decode. This predicate is
    /// the second line of defence after `is_echo_line`.
    fn looks_like_base64(s: &str) -> bool {
        let t = s.trim();
        !t.is_empty()
            && t.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
    }

    let mut pending = String::new();
    // Bytes at the head of `pending` already known to hold no newline.
    let mut scanned = 0usize;
    let mut full_log = String::new();
    // The wire cuts the shell's output at packet boundaries, which land
    // wherever they land — decoding each chunk on its own would eat any
    // character sitting on the seam. See `utf8_stream`.
    let mut utf8 = Utf8Stream::new();
    // In compress mode, post-READY lines accumulate into a single base64
    // buffer that's decoded once the sentinel arrives. In non-compress
    // mode this stays empty and the streaming callback is used directly.
    let mut compress_buf = String::new();
    let started = Instant::now();
    let max_wait = Duration::from_secs(timeout_secs);

    while started.elapsed() < max_wait {
        match transport.recv_event(RECV_TICK)? {
            ExecEvent::ShellOutput(data) => {
                log::trace!("[exec] recv ShellOutput {} bytes", data.len());
                let text = utf8.push(&data);
                pending.push_str(&text);
                push_bounded_tail(&mut full_log, &text, TIMEOUT_LOG_TAIL);
            }
            ExecEvent::ShellClosed => {
                // An acknowledgement for the *previous* command's
                // `ShellClose`, arriving after that run's drain gave up.
                // Nothing to do with this one.
                log::debug!("[exec] ignoring a late ShellClose acknowledgement");
                continue;
            }
            ExecEvent::ShellExit(code) => {
                log::debug!("[exec] recv ShellExit code={code} — host shell died");
                return Ok(code);
            }
            ExecEvent::HostError(msg) => {
                log::debug!("[exec] recv host Error msg={msg:?}");
            }
            ExecEvent::Idle => {
                // No data this tick — re-check timeout via outer loop.
            }
        }

        // Walk completed lines out of `pending`. Each line is whatever
        // came before the next `\n`, with trailing `\r` stripped.
        //
        // The search starts where the last one gave up. Output that carries
        // no newline for a long stretch — a big base64 blob written with
        // `-NoNewline`, a binary dump — would otherwise be rescanned from the
        // front on every packet, which is quadratic in the size of the
        // output and turns into a hang rather than a slow command.
        // `scanned` is always `pending.len()` from a previous pass, and
        // `pending` only ever grows by whole characters (`Utf8Stream`), so
        // slicing at it cannot land inside one.
        while let Some(rel) = pending[scanned..].find('\n') {
            let nl_idx = scanned + rel;
            scanned = 0;
            let raw_line = pending[..nl_idx].to_string();
            let consume = nl_idx + 1;
            pending.drain(..consume);
            let line = raw_line.trim_end_matches('\r');
            log::trace!("[exec] line state={state:?} phase={phase:?}: {line:?}");

            match state {
                OneShotState::AwaitingRemotePrompt => {
                    // Strip ANSI before matching — Starship et al wrap
                    // prompts in color/cursor escapes plus a trailing
                    // `\x1b[K` that breaks naive ends_with checks.
                    let stripped = strip_ansi(line);
                    if is_remote_prompt(stripped.trim_end()) {
                        log::debug!("[exec] remote prompt matched (line), sending payload");
                        transport.send_input(payload.as_bytes())?;
                        state = OneShotState::AwaitingSentinel;
                    }
                }
                OneShotState::AwaitingSentinel => {
                    // Sentinel check FIRST — it might be glued onto an
                    // unterminated output line (the bash sandwich
                    // `cmd; echo "__WD_DONE_..."` does that whenever
                    // <cmd>'s last byte isn't a newline).
                    //
                    // BUT only once READY has been seen. Both markers come
                    // out of the same payload and READY is printed first,
                    // so a sentinel appearing ahead of it cannot be the
                    // real one — it is the shell echoing our own source
                    // back, where the sentinel sits as a literal. That
                    // happens in `--compress` (a hardcoded `__0` sentinel
                    // rather than an expanded variable) and over
                    // `ssh -tt`, and it used to be caught by looking for
                    // both markers on one line — which stops working the
                    // moment the payload spans several lines, because then
                    // each marker is echoed on a line of its own.
                    //
                    // `is_echo_line` still guards the rest: an echo can
                    // also arrive *after* READY on paths that mirror input.
                    if phase == Phase::Streaming && !is_echo_line(line) {
                        if let Some(code) = parse_sentinel(line, &uuid) {
                            if let Some(pos) = line.rfind(&prefix) {
                                if pos > 0 {
                                    let pre = line[..pos].trim_end_matches('\r');
                                    if !pre.is_empty() && !is_echo_line(pre) {
                                        if compress {
                                            if looks_like_base64(pre) {
                                                compress_buf.push_str(pre.trim());
                                                compress_buf.push('\n');
                                            }
                                        } else {
                                            let mut chunk = pre.to_string();
                                            chunk.push('\n');
                                            on_chunk(chunk.as_bytes());
                                        }
                                    }
                                }
                            }
                            if compress && !compress_buf.is_empty() {
                                let decoded = decode_compressed_stream(&compress_buf)?;
                                let (clean, in_band_rc) = extract_compressed_rc(decoded);
                                if !clean.is_empty() {
                                    on_chunk(&clean);
                                }
                                // In compress mode the sentinel rc is always 0
                                // (set by the wrapper); the real rc is the
                                // in-band marker we just extracted.
                                return Ok(in_band_rc);
                            }
                            return Ok(code);
                        }
                    } // close the !is_echo_line guard around parse_sentinel

                    // READY is the runner's own marker in every wrapper
                    // (PS and Bash, plain and `--compress`), so it is
                    // dropped unconditionally: on the PS path we are
                    // already Streaming and it would otherwise be printed
                    // as output or poison the base64 buffer; on the Bash
                    // path it is the Mute→Streaming trigger below.
                    if parse_ready(line, &uuid) {
                        phase = Phase::Streaming;
                        // Drop the READY line itself in every phase.
                    } else if phase == Phase::Mute {
                        // Still waiting for READY, and everything ahead of
                        // it here is known noise: MOTD, the remote's echo
                        // of our payload. Drop it.
                    } else if phase == Phase::MuteNoise
                        && (line.trim().is_empty()
                            || is_powershell_prompt(line)
                            || is_powershell_continuation(line)
                            || is_remote_prompt(line))
                    {
                        // Pre-READY, and it looks like the prompt a re-used
                        // warm shell left behind. That is the only noise
                        // this path produces; anything else falls through
                        // and is emitted, because it may be the command's
                        // own stderr arriving early.
                    } else if (is_powershell_prompt(line) || is_remote_prompt(line))
                        && !is_echo_line(line)
                    {
                        // Already Streaming (PS path) and we hit a
                        // stale prompt: swallow it. Matches pre-rewrite
                        // `clean_stdout` which used `rposition` on the
                        // last prompt to set the lower bound. Doesn't
                        // affect SSH path because there phase is Mute
                        // until READY; any prompt arriving in Streaming
                        // would be unusual but harmless to drop.
                    } else if !is_echo_line(line) {
                        if compress {
                            if looks_like_base64(line) {
                                compress_buf.push_str(line.trim());
                                compress_buf.push('\n');
                            }
                            // else: drop noise (stray PS error, banner, ...)
                        } else {
                            let mut chunk = String::with_capacity(line.len() + 1);
                            chunk.push_str(line);
                            chunk.push('\n');
                            on_chunk(chunk.as_bytes());
                        }
                    }
                }
            }
        }

        // Everything still in `pending` has been looked at and holds no
        // newline; the next pass starts after it.
        scanned = pending.len();

        // Remote prompts can arrive WITHOUT a trailing newline (bash/zsh
        // park the cursor right after `$ ` / `# ` / `➜ `). Peek the
        // partial leftover after stripping ANSI escapes.
        if state == OneShotState::AwaitingRemotePrompt {
            let stripped = strip_ansi(&pending);
            if is_remote_prompt(stripped.trim_end()) {
                log::debug!("[exec] remote prompt matched (partial), sending payload");
                transport.send_input(payload.as_bytes())?;
                state = OneShotState::AwaitingSentinel;
                pending.clear();
                scanned = 0;
            }
        }
    }

    // Out of time. Anything the decoder is still holding belongs in the log
    // the error carries — half a character is a better clue than silence.
    let tail = utf8.finish();
    push_bounded_tail(&mut full_log, &tail, TIMEOUT_LOG_TAIL);
    Err(ExecError::Timeout(full_log))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::MockExecTransport;

    /// A host that answers the payload instead of replaying a fixed script.
    ///
    /// The runner mints a fresh uuid per call, so a canned sentinel can never
    /// match; this reads the uuid back out of the wrapper the runner just
    /// sent and builds the reply from it. `chunks` decides how the reply is
    /// cut on the wire — which is the whole point for the UTF-8 test.
    /// uuid -> the wire chunks the host answers with.
    type ReplyFn = Box<dyn Fn(&str) -> Vec<Vec<u8>>>;

    struct ScriptedHost {
        queued: std::collections::VecDeque<ExecEvent>,
        reply: ReplyFn,
    }

    impl ScriptedHost {
        fn new(reply: impl Fn(&str) -> Vec<Vec<u8>> + 'static) -> Self {
            Self {
                queued: std::collections::VecDeque::new(),
                reply: Box::new(reply),
            }
        }
    }

    /// Pull the uuid out of `__WD_READY_<uuid>__` in the payload.
    fn uuid_of(payload: &str) -> String {
        let start =
            payload.find("__WD_READY_").expect("payload carries READY") + "__WD_READY_".len();
        let rest = &payload[start..];
        let end = rest.find("__").expect("READY marker is terminated");
        rest[..end].to_string()
    }

    impl ExecTransport for ScriptedHost {
        fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
            let payload = String::from_utf8_lossy(data);
            let uuid = uuid_of(&payload);
            for chunk in (self.reply)(&uuid) {
                self.queued.push_back(ExecEvent::ShellOutput(chunk));
            }
            Ok(())
        }

        fn recv_event(&mut self, _timeout: Duration) -> Result<ExecEvent, ExecError> {
            Ok(self.queued.pop_front().unwrap_or(ExecEvent::Idle))
        }
    }

    /// Output with no newline in it for a long stretch, with the sentinel
    /// finally glued to its end. Exercises the incremental newline search:
    /// the scan resumes where it stopped, and a bookkeeping slip there shows
    /// up as a lost or duplicated prefix rather than as a slow test.
    #[test]
    fn a_long_unterminated_run_before_the_sentinel_is_emitted_once() {
        let body = "x".repeat(50_000);
        let body_for_host = body.clone();

        let mut host = ScriptedHost::new(move |uuid| {
            let mut out = vec![format!("__WD_READY_{uuid}__\n").into_bytes()];
            for piece in body_for_host.as_bytes().chunks(1000) {
                out.push(piece.to_vec());
            }
            out.push(format!("__WD_DONE_{uuid}__0\n").into_bytes());
            out
        });

        let mut got = Vec::new();
        let code = run_oneshot(&mut host, "x", None, 5, false, |c| got.extend_from_slice(c))
            .expect("run_oneshot");
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(got).expect("utf-8"), format!("{body}\n"));
    }

    #[test]
    fn the_timeout_log_keeps_the_tail_and_stops_growing() {
        let mut buf = String::new();
        for i in 0..1000 {
            push_bounded_tail(&mut buf, &format!("line {i}\n"), 256);
            assert!(buf.len() <= 256 + 16, "buffer grew to {}", buf.len());
        }
        assert!(buf.ends_with("line 999\n"), "tail lost: {buf:?}");
        assert!(!buf.contains("line 0\n"), "head should have been dropped");
    }

    #[test]
    fn trimming_the_timeout_log_never_cuts_a_character_in_half() {
        // Every byte of this is part of a multi-byte character, so a cut
        // computed by arithmetic alone would land inside one and panic.
        let mut buf = String::new();
        for _ in 0..200 {
            push_bounded_tail(&mut buf, "ёжик", 37);
        }
        assert!(buf.len() <= 37 + 4);
        assert!(buf.ends_with("ёжик"));
    }

    /// The wire cuts output at packet boundaries that know nothing about
    /// character boundaries. Before `Utf8Stream` each chunk was decoded on
    /// its own, so a Cyrillic letter landing on the seam turned into two
    /// replacement characters — the long-standing "bytes go missing in long
    /// Cyrillic output" report, which `--compress` hid because base64 is
    /// ASCII.
    #[test]
    fn a_character_split_across_two_packets_arrives_whole() {
        let line = "Отчёт готов, ошибок нет";
        let cut = "Отчёт готов, о".len() + 1; // one byte into "ш"
        let expected = format!("{line}\n");

        let mut host = ScriptedHost::new(move |uuid| {
            let head = format!("__WD_READY_{uuid}__\n");
            let tail = format!("\n__WD_DONE_{uuid}__0\n");
            let body = line.as_bytes();
            vec![
                head.into_bytes(),
                body[..cut].to_vec(),
                body[cut..].to_vec(),
                tail.into_bytes(),
            ]
        });

        let mut got = Vec::new();
        let code = run_oneshot(&mut host, "x", None, 5, false, |c| got.extend_from_slice(c))
            .expect("run_oneshot");
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(got).expect("valid utf-8"), expected);
    }

    /// Same seam, but one byte at a time — the pathological case for a
    /// decoder that keeps state.
    #[test]
    fn output_delivered_byte_by_byte_still_arrives_whole() {
        let line = "щётка ёж 漢字 🙂";
        let expected = format!("{line}\n");

        let mut host = ScriptedHost::new(move |uuid| {
            let mut out = vec![format!("__WD_READY_{uuid}__\n").into_bytes()];
            out.extend(line.as_bytes().iter().map(|b| vec![*b]));
            out.push(format!("\n__WD_DONE_{uuid}__0\n").into_bytes());
            out
        });

        let mut got = Vec::new();
        let code = run_oneshot(&mut host, "x", None, 5, false, |c| got.extend_from_slice(c))
            .expect("run_oneshot");
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(got).expect("valid utf-8"), expected);
    }

    /// Helper: build an `ExecEvent::ShellOutput` from a `&str` slice.
    fn out(s: &str) -> ExecEvent {
        ExecEvent::ShellOutput(s.as_bytes().to_vec())
    }

    /// Build a fixed test UUID so we can craft sentinel lines with
    /// matching markers. The runner generates a fresh UUID each call,
    /// so we can't pin it — instead we build host-side responses that
    /// are sentinel-shape regardless of UUID, which `parse_sentinel`
    /// will accept once we extract the UUID from the payload.
    ///
    /// Trick: the runner sends `format_command(uuid, ...)` immediately
    /// (PS path) or after the SSH prompt (Bash path). The mock
    /// transport records this in its outbox; the test fixture builder
    /// reads it, extracts the UUID, then crafts a matching sentinel
    /// response and pushes it onto the event queue mid-test.
    ///
    /// For unit tests we side-step that complexity by pre-loading the
    /// transport with sentinels keyed to a known UUID and then asserting
    /// that the runner's response matches up. But the runner generates
    /// UUIDs internally, so we instead test via the *outbox*: assert
    /// that the runner sent the correct sentinel-format payload.
    ///
    /// Real integration testing of the sentinel-match path lives in
    /// `wiredesk-term::tests` via the split-pair fixture (which can
    /// extract the UUID from a real `Packet`).
    fn expected_payload_uuid(outbox: &[Vec<u8>]) -> uuid::Uuid {
        let payload = std::str::from_utf8(&outbox[0]).expect("utf8 payload");
        extract_uuid_from(payload)
    }

    /// Extract the UUID from a `format_command` / `format_compressed_command`
    /// payload by locating the `__WD_DONE_<uuid>__` marker within it.
    fn extract_uuid_from(payload: &str) -> uuid::Uuid {
        let marker = "__WD_DONE_";
        let start = payload.find(marker).expect("uuid marker") + marker.len();
        let after = &payload[start..];
        let end = after.find("__").expect("uuid end");
        uuid::Uuid::parse_str(&after[..end]).expect("parse uuid")
    }

    /// Hand-crafted scenario:
    ///   1. Runner sends payload (PS path, no SSH)
    ///   2. Test extracts UUID from the recorded outbox
    ///   3. Test builds a matching expanded-sentinel response and
    ///      injects it as if the host had emitted it, then runs the
    ///      transport once more (re-entry into recv_event)
    ///
    /// That's two-pass: not the cleanest API. The next test below uses
    /// a simpler shape — just shove the response into the queue *before*
    /// run_oneshot starts and trust the runner's UUID happens to match.
    /// That doesn't work. So we instead pre-load with multiple UUIDs
    /// and the runner will see "wrong UUID" sentinels and ignore them
    /// — those are tested separately by `parse_sentinel_rejects_other_uuid`.
    ///
    /// For runner-level tests we rely on a stub-event-builder pattern:
    /// the mock can replay events lazily via a closure. But MockExec-
    /// Transport isn't that flexible yet — it's a static queue. So we
    /// keep these tests focused on phase/streaming/echo behavior with
    /// pre-baked sentinel UUIDs that we *assume* match (and the test
    /// asserts it via the outbox check).
    ///
    /// Cleaner: use a closure-based mock. But that's overkill — the
    /// 6 split-pair tests in wiredesk-term cover the UUID-roundtrip
    /// path through `Transport::send`. Here we exercise the runner's
    /// pure-callback semantics with a hand-rolled scenario.
    #[test]
    fn happy_path_ps_streams_post_prompt_lines() {
        // Pre-load with: noise + prompt + actual lines + sentinel.
        // We can't know the runner's UUID up front, so we use the
        // nil UUID and ASSUME parse_sentinel will see it. This test
        // is a *negative* assertion: nothing matches the runner's
        // generated UUID, so the runner times out — and we instead
        // assert (via outbox) that it sent the right payload-shape
        // and (via callback) that nothing was emitted (still Mute).
        //
        // ↑ scratched. Instead: write the response with the sentinel
        // line as raw template `__WD_DONE_$UUID__0` and post-process
        // the queue *after* the runner publishes its UUID — but mock
        // doesn't support that.
        //
        // Pragmatic: bypass the UUID generation by giving the runner
        // a transport that emits the sentinel for *whatever* UUID
        // appears in the outbox. Achieved by a custom impl below.

        struct UuidEchoTransport {
            outbox: Vec<Vec<u8>>,
            queued_after_payload: Vec<ExecEvent>,
            payload_seen: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEchoTransport {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                if !self.payload_seen
                    && std::str::from_utf8(data)
                        .map(|s| s.contains("__WD_DONE_"))
                        .unwrap_or(false)
                {
                    self.payload_seen = true;
                    let uuid = expected_payload_uuid(&self.outbox);
                    // Stage scripted host output now that we know the UUID.
                    let scripted = vec![
                        out("Some pre-prompt noise\n"),
                        out("PS C:\\Users\\User>\n"),
                        out(&format!("__WD_READY_{uuid}__\n")),
                        out("actual line 1\n"),
                        out("actual line 2\n"),
                        out(&format!("__WD_DONE_{uuid}__0\n")),
                    ];
                    self.queue.extend(scripted);
                    self.queue.extend(self.queued_after_payload.drain(..));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEchoTransport {
            outbox: Vec::new(),
            queued_after_payload: Vec::new(),
            payload_seen: false,
            queue: std::collections::VecDeque::new(),
        };

        let mut emitted: Vec<u8> = Vec::new();
        let code = run_oneshot(&mut t, "echo hi", None, 5, false, |chunk| {
            emitted.extend_from_slice(chunk);
        })
        .expect("run_oneshot ok");

        assert_eq!(code, 0);
        let s = String::from_utf8(emitted).unwrap();
        // Pipe mode drops the prompt ahead of READY and keeps the rest:
        // a line that is not recognisable noise may be the command's own
        // stderr, which the host can deliver ahead of stdout because it
        // reads the two streams on separate threads.
        assert_eq!(
            s, "Some pre-prompt noise\nactual line 1\nactual line 2\n",
            "the prompt goes, anything else survives"
        );
    }

    #[test]
    fn happy_path_ssh_strips_motd_and_echo_streams_post_ready() {
        struct UuidEchoSsh {
            outbox: Vec<Vec<u8>>,
            sent_ssh_hop: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEchoSsh {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh_hop && s.starts_with("ssh -tt ") {
                    self.sent_ssh_hop = true;
                    // Emit a remote prompt so runner advances state.
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = {
                        let payload = std::str::from_utf8(self.outbox.last().unwrap()).unwrap();
                        let marker = "__WD_DONE_";
                        let start = payload.find(marker).unwrap() + marker.len();
                        let after = &payload[start..];
                        let end = after.find("__").unwrap();
                        uuid::Uuid::parse_str(&after[..end]).unwrap()
                    };
                    let scripted = vec![
                        out("Welcome to Ubuntu\n"),
                        out("MOTD line 1\n"),
                        out(&format!(
                            "echo __WD_READY_{uuid}__; docker ps; echo \"__WD_DONE_{uuid}__$?\"\n"
                        )),
                        out(&format!("__WD_READY_{uuid}__\n")),
                        out("row1\n"),
                        out("row2\n"),
                        out(&format!("__WD_DONE_{uuid}__0\n")),
                    ];
                    self.queue.extend(scripted);
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEchoSsh {
            outbox: Vec::new(),
            sent_ssh_hop: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };

        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "docker ps", Some("prod"), 5, false, |chunk| {
            emitted.extend_from_slice(chunk);
        })
        .expect("run_oneshot ok");

        assert_eq!(code, 0);
        let s = String::from_utf8(emitted).unwrap();
        assert_eq!(
            s, "row1\nrow2\n",
            "MOTD and echo line dropped, post-READY streamed"
        );
    }

    #[test]
    fn timeout_returns_err_with_full_log_buffer() {
        // No sentinel ever arrives — runner should hit the wall-clock
        // budget and return Err(Timeout(buf)) carrying everything we
        // sent. Caller (term) will run format_timeout_diagnostic on it.
        let mut t = MockExecTransport::new([
            out("partial output but no sentinel\n"),
            ExecEvent::Idle,
            ExecEvent::Idle,
        ]);
        // Loop seeds idles after queue drains, which keeps the runner
        // ticking until budget elapses.

        let result = run_oneshot(&mut t, "stuck", None, 1, false, |_| {});

        match result {
            Err(ExecError::Timeout(buf)) => {
                assert!(
                    buf.contains("partial output but no sentinel"),
                    "Timeout buf must include wire log: {buf:?}"
                );
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn a_late_close_acknowledgement_does_not_become_this_commands_exit_code() {
        // The host answers every `ShellClose` with `ShellClosed`. One that
        // arrives after the previous run's drain gave up lands in this
        // run's stream, and it must not be read as a result - it carries
        // no status precisely so that it cannot be.
        struct LateAck {
            outbox: Vec<Vec<u8>>,
            staged: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for LateAck {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                if !self.staged {
                    self.staged = true;
                    let uuid = expected_payload_uuid(&self.outbox);
                    self.queue.push_back(ExecEvent::ShellClosed);
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out("real output\n"));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = LateAck {
            outbox: Vec::new(),
            staged: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "echo hi", None, 5, false, |c| {
            emitted.extend_from_slice(c);
        })
        .unwrap();
        assert_eq!(code, 0, "the run must finish on its own sentinel");
        assert_eq!(String::from_utf8(emitted).unwrap(), "real output\n");
    }

    #[test]
    fn a_sentinel_echoed_before_ready_is_not_the_real_one() {
        // A multi-line `--compress` payload is echoed line by line, and its
        // last source line carries the hardcoded `__WD_DONE_<uuid>__0`
        // while READY sits on the first. Matching that echo ends the run
        // before the command has produced anything: rc 0, no output.
        struct SplitEcho {
            outbox: Vec<Vec<u8>>,
            staged: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for SplitEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                if !self.staged {
                    self.staged = true;
                    let uuid = expected_payload_uuid(&self.outbox);
                    // Echo of the tail of our own source: sentinel literal,
                    // no READY anywhere on the line.
                    self.queue
                        .push_back(out(&format!(">> Write-Output \"__WD_DONE_{uuid}__0\"\n")));
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out("real output\n"));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }
        let mut t = SplitEcho {
            outbox: Vec::new(),
            staged: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "whatever", None, 5, false, |c| {
            emitted.extend_from_slice(c);
        })
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(emitted).unwrap(),
            "real output\n",
            "the run must end on the real sentinel, after the real output"
        );
    }

    #[test]
    fn the_echo_of_our_own_first_line_never_reaches_the_caller() {
        // PowerShell mirrors every line it reads from a redirected stdin,
        // and the first one comes glued to its prompt. It carries the READY
        // marker inside a longer line, which is what identifies it - the
        // expanded marker stands alone. Until 2026-09-11 the filter looked
        // for the word `echo` instead, so this leaked for any command that
        // did not happen to contain it.
        struct EchoesInput {
            outbox: Vec<Vec<u8>>,
            staged: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for EchoesInput {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                if !self.staged {
                    self.staged = true;
                    let uuid = expected_payload_uuid(&self.outbox);
                    let payload = String::from_utf8_lossy(&self.outbox[0]).to_string();
                    let first = payload.lines().next().unwrap_or_default().to_string();
                    // Prompt + the payload's own first line, as PowerShell
                    // prints it.
                    self.queue
                        .push_back(out(&format!("PS C:\\Users\\User> {first}\n")));
                    self.queue.push_back(out(">> \n"));
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out("in\n"));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }
        let mut t = EchoesInput {
            outbox: Vec::new(),
            staged: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        // A command with no `echo` in it anywhere.
        let code = run_oneshot(&mut t, "if ($true) {\n  \"in\"\n}", None, 5, false, |c| {
            emitted.extend_from_slice(c);
        })
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(emitted).unwrap(), "in\n");
    }

    #[test]
    fn stderr_arriving_before_ready_is_not_swallowed() {
        // The host reads the shell's stdout and stderr on two threads into
        // one queue, so a line the command wrote to stderr can overtake the
        // READY the wrapper wrote to stdout. Dropping everything pre-READY
        // would eat it; only recognisable noise may be dropped here.
        struct EarlyStderr {
            outbox: Vec<Vec<u8>>,
            staged: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for EarlyStderr {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                if !self.staged {
                    self.staged = true;
                    let uuid = expected_payload_uuid(&self.outbox);
                    self.queue.push_back(out("PS C:\\Users\\User>\n"));
                    self.queue.push_back(out("\n"));
                    self.queue.push_back(out("warning: something went wrong\n"));
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out("result\n"));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }
        let mut t = EarlyStderr {
            outbox: Vec::new(),
            staged: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "something", None, 5, false, |c| {
            emitted.extend_from_slice(c);
        })
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(emitted).unwrap(),
            "warning: something went wrong\nresult\n",
            "the prompt and the blank line go, the command's own line stays"
        );
    }

    #[test]
    fn a_shell_that_really_dies_still_ends_the_run() {
        // The guard above must not swallow a genuine shell death - including
        // `exit -1`, which used to collide with the acknowledgement when it
        // was spelled as an exit code.
        struct Dies {
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for Dies {
            fn send_input(&mut self, _data: &[u8]) -> Result<(), ExecError> {
                self.queue.push_back(ExecEvent::ShellExit(-1));
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }
        let mut t = Dies {
            queue: std::collections::VecDeque::new(),
        };
        let code = run_oneshot(&mut t, "boom", None, 5, false, |_| {}).unwrap();
        assert_eq!(
            code, -1,
            "`exit -1` is a real status, not an acknowledgement"
        );
    }

    #[test]
    fn nonzero_exit_propagates_through_callback() {
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            staged: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                if !self.staged {
                    self.staged = true;
                    let uuid = expected_payload_uuid(&self.outbox);
                    // READY first — flips the runner from Mute to
                    // Streaming so the next line reaches the callback.
                    self.queue.push_back(out("PS C:\\>\n"));
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out("err: nope\n"));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__7\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEcho {
            outbox: Vec::new(),
            staged: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "false_cmd", None, 5, false, |c| {
            emitted.extend_from_slice(c);
        })
        .unwrap();
        assert_eq!(code, 7);
        assert_eq!(String::from_utf8(emitted).unwrap(), "err: nope\n");
    }

    #[test]
    fn unterminated_output_glued_to_sentinel_recovers_prefix() {
        // Regression mirror of parse_sentinel_after_unterminated_output:
        // command emits stdout WITHOUT trailing newline (`head -c 800`),
        // bash sandwich glues the expanded sentinel directly onto it.
        // The runner must (a) detect the sentinel, (b) emit the
        // pre-sentinel prefix as one final chunk, (c) return the exit code.
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = {
                        let p = std::str::from_utf8(self.outbox.last().unwrap()).unwrap();
                        let marker = "__WD_DONE_";
                        let start = p.find(marker).unwrap() + marker.len();
                        let after = &p[start..];
                        let end = after.find("__").unwrap();
                        uuid::Uuid::parse_str(&after[..end]).unwrap()
                    };
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out(&format!(
                        "{{\"hits\":{{\"total\":42}}}}__WD_DONE_{uuid}__0\n"
                    )));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "head -c 800 …", Some("prod"), 5, false, |c| {
            emitted.extend_from_slice(c);
        })
        .unwrap();
        assert_eq!(code, 0);
        let s = String::from_utf8(emitted).unwrap();
        assert_eq!(
            s, "{\"hits\":{\"total\":42}}\n",
            "unterminated prefix recovered, sentinel stripped"
        );
    }

    /// Build a base64-of-gzip fixture with a trailing `__WD_RC__<rc>__`
    /// marker — mimicking what the new compress wrapper actually emits
    /// (rc is in-band, sentinel rc is hardcoded 0).
    fn make_compressed_b64_with_rc(payload: &[u8], rc: i32) -> String {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut full = Vec::from(payload);
        full.extend_from_slice(format!("__WD_RC__{rc}__\n").as_bytes());
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&full).unwrap();
        let gzipped = encoder.finish().unwrap();
        let raw = STANDARD.encode(&gzipped);
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
    fn runner_compress_happy_path_ssh_decodes_buffer_once() {
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = extract_uuid_from(s);
                    // Wrapper emits cmd output + __WD_RC__<rc>__ marker
                    // before sentinel; happy path uses rc=0.
                    let b64 =
                        make_compressed_b64_with_rc(b"the quick brown fox\nover the lazy dog\n", 0);
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out(&format!("{b64}\n")));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let mut callback_calls = 0;
        let code = run_oneshot(&mut t, "head /var/log", Some("prod"), 5, true, |c| {
            callback_calls += 1;
            emitted.extend_from_slice(c);
        })
        .expect("ok");
        assert_eq!(code, 0);
        assert_eq!(callback_calls, 1, "compress mode emits exactly one chunk");
        // Cmd output's trailing \n is preserved byte-for-byte
        // (AC2 byte-identical with non-compress baseline).
        assert_eq!(emitted, b"the quick brown fox\nover the lazy dog\n");
    }

    #[test]
    fn runner_compress_skips_sentinel_match_on_echo_line() {
        // Regression for live-test 2026-05-05: ssh -tt PTY echoes our
        // wrapper input back as-is, and the new compress wrapper has
        // a literal `__WD_DONE_<uuid>__0` in its source (sentinel rc
        // is hardcoded 0). Without the is_echo_line guard around
        // parse_sentinel, the runner sees the echoed line, matches
        // the sentinel pattern, returns Ok(0) before the cmd runs —
        // empty buffer, 0 bytes output.
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = extract_uuid_from(s);
                    let echoed_cmd = s.trim_end_matches('\n');
                    let b64 = make_compressed_b64_with_rc(b"real output\n", 0);
                    // 1) PTY echoes our compress cmd back literally
                    //    (contains "__WD_DONE_<uuid>__0" in the source!)
                    self.queue.push_back(out(&format!("{echoed_cmd}\r\n")));
                    // 2) READY from `echo __WD_READY_...`
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    // 3) Real base64 payload
                    self.queue.push_back(out(&format!("{b64}\n")));
                    // 4) Real sentinel
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }
        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "ls -la", Some("prod"), 5, true, |c| {
            emitted.extend_from_slice(c);
        })
        .expect("ok");
        assert_eq!(code, 0);
        // Without the guard, this assertion would fail with empty
        // emitted (runner returned on the echo'd line's literal
        // `__WD_DONE_<uuid>__0` BEFORE the real cmd ran). Trailing
        // \n preserved byte-for-byte.
        assert_eq!(emitted, b"real output\n");
    }

    #[test]
    fn runner_compress_in_band_rc_propagates_over_sentinel_zero() {
        // The new wrapper hardcodes sentinel rc=0; the real exit code
        // is in the in-band __WD_RC__ marker. Verify the runner picks
        // up the in-band rc, not the sentinel one.
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = extract_uuid_from(s);
                    let b64 = make_compressed_b64_with_rc(b"err: nope\n", 42);
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out(&format!("{b64}\n")));
                    // Sentinel rc is 0 — runner must use in-band 42 instead.
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }
        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "false", Some("prod"), 5, true, |c| {
            emitted.extend_from_slice(c);
        })
        .expect("ok");
        assert_eq!(code, 42, "in-band rc must override sentinel rc=0");
        assert_eq!(emitted, b"err: nope\n");
    }

    #[test]
    fn runner_compress_non_base64_noise_is_dropped_silently() {
        // Noise lines (PS error formatting with quotes, ssh banners,
        // anything that isn't pure base64) get filtered out by the
        // looks_like_base64 predicate. Buffer ends up empty → runner
        // returns Ok(0) without invoking the callback. This is the
        // robust-to-host-noise behaviour: better to silently produce
        // no output than to hard-fail with CompressionFailed on
        // legitimate stray host text.
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = extract_uuid_from(s);
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    // Three noise variants the filter must drop.
                    self.queue.push_back(out("!!!not base64!!!\n"));
                    self.queue.push_back(out(
                        "Get-Item : Cannot find path \"C:\\nope\" because it does not exist.\n",
                    ));
                    self.queue
                        .push_back(out("    + CategoryInfo : ObjectNotFound\n"));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "noisy", Some("prod"), 5, true, |c| {
            emitted.extend_from_slice(c);
        })
        .expect("ok — noise dropped, sentinel rc=0");
        assert_eq!(code, 0);
        assert!(emitted.is_empty(), "no callback invoked for empty buffer");
    }

    #[test]
    fn runner_compress_valid_b64_invalid_gzip_returns_compression_failed() {
        // Filter passes (chars are base64-shaped) but the decoded
        // bytes aren't a valid gzip stream. Distinct from the noise
        // case above — here we made it past base64 decode and
        // failed at gunzip.
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = extract_uuid_from(s);
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    // Valid base64 of "hello" — but "hello" isn't gzip.
                    self.queue.push_back(out("aGVsbG8=\n"));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let result = run_oneshot(&mut t, "x", Some("prod"), 5, true, |_| {});
        assert!(
            matches!(result, Err(ExecError::CompressionFailed(_))),
            "valid-b64-invalid-gzip must surface as CompressionFailed: {result:?}"
        );
    }

    #[test]
    fn runner_compress_timeout_returns_timeout_not_compression_failed() {
        // Host streams READY + partial base64 then goes idle. Runner
        // must hit wall-clock timeout and return Err(Timeout(_)) — NOT
        // attempt to decode the partial buffer (which would yield a
        // misleading CompressionFailed).
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let _uuid = extract_uuid_from(s);
                    self.queue
                        .push_back(out(&format!("__WD_READY_{_uuid}__\n")));
                    self.queue.push_back(out("H4sIAAAAAAAAAytJLS4BAAhJ\n"));
                    // No DONE sentinel — runner times out.
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let result = run_oneshot(&mut t, "stuck", Some("prod"), 1, true, |_| {});
        assert!(
            matches!(result, Err(ExecError::Timeout(_))),
            "expected Timeout (not CompressionFailed) on partial buffer + budget exhaust: {result:?}"
        );
    }

    #[test]
    fn runner_compress_pre_prefix_unterminated_recovery_buffered() {
        // If the host glues sentinel directly onto the last base64 line
        // (no trailing newline before the marker), the runner's pre-
        // prefix recovery path kicks in. In compress mode that prefix
        // must go into the base64 buffer, not the callback — otherwise
        // the buffer is missing its tail and decode fails.
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = extract_uuid_from(s);
                    let b64 = make_compressed_b64_with_rc(b"hello compressed world", 0);
                    let single = b64.replace('\n', "");
                    // Glue: last base64 line with sentinel directly
                    // appended (no \n between them).
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue
                        .push_back(out(&format!("{single}__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "x", Some("prod"), 5, true, |c| {
            emitted.extend_from_slice(c);
        })
        .expect("ok");
        assert_eq!(code, 0);
        assert_eq!(emitted, b"hello compressed world");
    }

    #[test]
    fn pre_ready_chunks_are_muted_not_emitted() {
        // Phase-tracker correctness: anything that arrives BEFORE the
        // READY marker (or PS prompt) must NOT reach the callback,
        // even if it looks like normal output.
        struct UuidEcho {
            outbox: Vec<Vec<u8>>,
            sent_ssh: bool,
            sent_payload: bool,
            queue: std::collections::VecDeque<ExecEvent>,
        }
        impl ExecTransport for UuidEcho {
            fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError> {
                self.outbox.push(data.to_vec());
                let s = std::str::from_utf8(data).unwrap_or("");
                if !self.sent_ssh && s.starts_with("ssh -tt ") {
                    self.sent_ssh = true;
                    self.queue.push_back(out("user@host:~$ "));
                } else if !self.sent_payload && s.contains("__WD_DONE_") {
                    self.sent_payload = true;
                    let uuid = {
                        let p = std::str::from_utf8(self.outbox.last().unwrap()).unwrap();
                        let marker = "__WD_DONE_";
                        let start = p.find(marker).unwrap() + marker.len();
                        let after = &p[start..];
                        let end = after.find("__").unwrap();
                        uuid::Uuid::parse_str(&after[..end]).unwrap()
                    };
                    self.queue.push_back(out("MOTD-ish line\n"));
                    self.queue.push_back(out("PRE-READY junk\n"));
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out("real-output\n"));
                    self.queue.push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
                }
                Ok(())
            }
            fn recv_event(&mut self, _t: Duration) -> Result<ExecEvent, ExecError> {
                Ok(self.queue.pop_front().unwrap_or(ExecEvent::Idle))
            }
        }

        let mut t = UuidEcho {
            outbox: Vec::new(),
            sent_ssh: false,
            sent_payload: false,
            queue: std::collections::VecDeque::new(),
        };
        let mut emitted = Vec::new();
        let code = run_oneshot(&mut t, "x", Some("prod"), 5, false, |c| {
            emitted.extend_from_slice(c);
        })
        .unwrap();
        assert_eq!(code, 0);
        let s = String::from_utf8(emitted).unwrap();
        assert!(
            !s.contains("MOTD-ish"),
            "Mute phase must drop pre-READY noise: {s:?}"
        );
        assert!(
            !s.contains("PRE-READY"),
            "Mute phase must drop pre-READY noise: {s:?}"
        );
        assert_eq!(s, "real-output\n");
    }
}
