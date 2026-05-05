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
    is_powershell_prompt, is_remote_prompt, parse_ready, parse_sentinel, strip_ansi,
};
use crate::transport::ExecTransport;
use crate::types::{ExecError, ExecEvent, OneShotState, ShellKind};

/// How long each `recv_event` call may park. Smaller = more frequent
/// timeout-budget re-checks, larger = fewer wakeups. 100 ms matches
/// the host's heartbeat tick and gives ~10 timeout-checks/sec —
/// plenty of resolution against a 90 s budget.
const RECV_TICK: Duration = Duration::from_millis(100);

/// Phase tracker for the line stream. `Mute` skips noise that
/// precedes the user command's actual output (host MOTD, SSH banner,
/// `ssh -tt` warning, host prompt line). `Streaming` emits each
/// completed non-echo line through the caller's callback. The
/// transition trigger is the READY marker (Bash/--ssh path) or the
/// host shell prompt (PowerShell pipe-mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Mute,
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

    // Phase initial: SSH path keeps Mute until READY (we know there
    // will be MOTD + ssh-tt echoes to drop). PS pipe-mode goes
    // straight into Streaming because PS doesn't echo stdin and
    // the only pre-cmd noise *might* be a stray prompt line — which
    // we then opportunistically swallow once we see one (matches
    // pre-rewrite `clean_stdout` `unwrap_or(0)` fallback).
    let mut phase = if ssh.is_some() {
        Phase::Mute
    } else {
        Phase::Streaming
    };

    let prefix = format!("__WD_DONE_{uuid}__");
    let done_echo = format!("__WD_DONE_{uuid}__$");
    let done_zero_echo = format!("__WD_DONE_{uuid}__0");
    let ready_echo = format!("__WD_READY_{uuid}__");
    // Stdin-echo filter: drop the literal echoes that the remote shell
    // emits in `ssh -tt` mode (echoing our READY emitter and DONE
    // formatter back at us). Compress wrappers use a hardcoded `__0`
    // sentinel rather than `$rc`/`$LASTEXITCODE`, so we look for the
    // unique compress-only fragments (`gzip -c | base64` for bash,
    // `[Console]::OutputEncoding` for PS) anchored by the READY uuid
    // marker — the b64 payload itself can never plausibly match those.
    let is_echo_line = |s: &str| {
        s.contains(&done_echo)
            || (s.contains("echo ") && s.contains(&ready_echo))
            || (s.contains(&ready_echo) && s.contains(&done_zero_echo))
            || (s.contains(&ready_echo) && s.contains("gzip -c | base64"))
            || (s.contains(&ready_echo) && s.contains("[Console]::OutputEncoding"))
    };

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
    let mut full_log = String::new();
    // In compress mode, post-READY lines accumulate into a single base64
    // buffer that's decoded once the sentinel arrives. In non-compress
    // mode this stays empty and the streaming callback is used directly.
    let mut compress_buf = String::new();
    let started = Instant::now();
    let max_wait = Duration::from_secs(timeout_secs);

    while started.elapsed() < max_wait {
        match transport.recv_event(RECV_TICK)? {
            ExecEvent::ShellOutput(data) => {
                let text = String::from_utf8_lossy(&data);
                log::trace!("[exec] recv ShellOutput {} bytes", data.len());
                pending.push_str(&text);
                full_log.push_str(&text);
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
        while let Some(nl_idx) = pending.find('\n') {
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
                    // BUT: skip parse_sentinel on the echo'd cmd line
                    // itself. In compress mode the wrapper carries a
                    // hardcoded `__WD_DONE_<uuid>__0` literal in its
                    // source — when ssh -tt echoes our input back,
                    // parse_sentinel would match the literal and we'd
                    // return Ok(0) before the real cmd has even run.
                    // is_echo_line catches both bash and PS compress
                    // cmd echoes via anchor-pair signatures.
                    if !is_echo_line(line) {
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

                    // Compress mode: PS path emits the READY line as
                    // regular output (we go straight to Streaming on
                    // PS, no Mute phase). Drop it explicitly so it
                    // doesn't poison the base64 buffer. Bash path
                    // already drops READY via the Mute→Streaming
                    // transition below — no double-handling.
                    if compress && parse_ready(line, &uuid) {
                        if phase == Phase::Mute {
                            phase = Phase::Streaming;
                        }
                        // Drop the READY line itself in either phase.
                    } else if phase == Phase::Mute {
                        // Mute → Streaming on READY (Bash/--ssh path).
                        // We don't trigger on prompt here because the
                        // SSH path's READY is the canonical lower bound;
                        // a stale prompt earlier would belong in the
                        // pre-READY noise we want to drop.
                        if parse_ready(line, &uuid) {
                            phase = Phase::Streaming;
                            // Drop the READY line itself.
                        }
                        // else: still Mute, drop the line.
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
            }
        }
    }

    Err(ExecError::Timeout(full_log))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::mock::MockExecTransport;

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
        // PS path streams from start; the prompt line is swallowed
        // mid-stream (matches pre-rewrite clean_stdout `rposition` on
        // the last prompt). Pre-prompt noise comes through — same as
        // the old `unwrap_or(0)` lower-bound when no prompt was found
        // *before* it. Slight semantic shift vs the old clean_stdout
        // behaviour that took `rposition` (last prompt) and dropped
        // everything before — accepted because live PS pipe-mode
        // doesn't actually emit pre-prompt noise; the test is purely
        // synthetic.
        assert_eq!(
            s,
            "Some pre-prompt noise\nactual line 1\nactual line 2\n",
            "PS path streams from start; only the prompt line is swallowed"
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
        assert_eq!(s, "row1\nrow2\n", "MOTD and echo line dropped, post-READY streamed");
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
                    // Prompt line first — flips runner from Mute to
                    // Streaming so the next line reaches the callback.
                    self.queue.push_back(out("PS C:\\>\n"));
                    self.queue.push_back(out("err: nope\n"));
                    self.queue
                        .push_back(out(&format!("__WD_DONE_{uuid}__7\n")));
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
            s,
            "{\"hits\":{\"total\":42}}\n",
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
                    let b64 = make_compressed_b64_with_rc(
                        b"the quick brown fox\nover the lazy dog\n",
                        0,
                    );
                    self.queue.push_back(out(&format!("__WD_READY_{uuid}__\n")));
                    self.queue.push_back(out(&format!("{b64}\n")));
                    self.queue
                        .push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
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
                    self.queue
                        .push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
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
                    self.queue
                        .push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
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
                    self.queue.push_back(out("    + CategoryInfo : ObjectNotFound\n"));
                    self.queue
                        .push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
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
                    self.queue
                        .push_back(out(&format!("__WD_DONE_{uuid}__0\n")));
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
                    self.queue.push_back(out(&format!("__WD_READY_{_uuid}__\n")));
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
                    let b64 =
                        make_compressed_b64_with_rc(b"hello compressed world", 0);
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
        assert!(!s.contains("MOTD-ish"), "Mute phase must drop pre-READY noise: {s:?}");
        assert!(!s.contains("PRE-READY"), "Mute phase must drop pre-READY noise: {s:?}");
        assert_eq!(s, "real-output\n");
    }
}
