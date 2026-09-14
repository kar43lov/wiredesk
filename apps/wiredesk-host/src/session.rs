use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wiredesk_core::error::{Result, WireDeskError};
use wiredesk_core::storm::{StormCounter, DEFAULT_STORM_THRESHOLD};
use wiredesk_protocol::message::{
    Message, ERR_PTY_BUSY, ERR_PTY_SPAWN, ERR_SHELL_BUSY, ERR_SHELL_SPAWN, HOST_PROTO_VERSION,
    VERSION,
};
use wiredesk_protocol::packet::{Packet, MAX_PAYLOAD};
use wiredesk_transport::transport::Transport;

use crate::clipboard::{ClipboardSync, ProgressCounters};
use crate::injector::InputInjector;
use crate::shell::{shell_argv, ShellEvent, ShellProcess};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
/// Heartbeat timeout while the link is idle. 3 missed heartbeats — fast
/// enough for the user to notice an unplugged cable but loose enough to
/// tolerate a single dropped CRC.
const HEARTBEAT_TIMEOUT_IDLE: Duration = Duration::from_secs(6);
/// Heartbeat timeout while a clipboard transfer is in flight (incoming
/// reassembly armed or outgoing chunks queued). At 11 KB/s an 80–500 KB
/// image takes 7–45 s on the wire, during which the strict 6 s timeout
/// would falsely fire — the peer is busy receiving chunks and its
/// heartbeats can be queued behind ours. ×5 the idle timeout: enough
/// slack for ~3 MB of in-flight payload before we treat silence as a
/// real disconnect.
const HEARTBEAT_TIMEOUT_BUSY: Duration = Duration::from_secs(30);

/// Pure helper — pick busy or idle heartbeat budget. Extracted so the
/// branching can be unit-tested without spawning a real `ShellProcess`
/// (which forks PowerShell on Windows and isn't reachable from CI).
fn heartbeat_timeout_for(clipboard_busy: bool, shell_open: bool) -> Duration {
    if clipboard_busy || shell_open {
        HEARTBEAT_TIMEOUT_BUSY
    } else {
        HEARTBEAT_TIMEOUT_IDLE
    }
}

/// How many bytes of output one shell slot may put on the wire per tick.
///
/// `transport.send` blocks and `tick` does not call `recv` while it is
/// sending, so a full budget is dead air for everything else on the link:
/// 64 KB is ~218 ms on the 3 Mbaud serial link, ~533 ms on RFCOMM, seconds
/// on BLE.
///
/// Counted in bytes, not in reads. It used to be 16 *reads*, and a shell
/// printing line by line hands over one ~50-byte line per read — so a tick
/// shipped under a kilobyte and the rate was set by how often `tick` runs
/// (~32/s behind the 10 ms recv timeout), not by the wire. Measured live
/// 2026-09-14: 200 KB took 31.0 s as 4000 lines and 2.5 s as one string.
const PUMP_BUDGET: usize = 16 * MAX_PAYLOAD;

/// The exec slot's budget while an interactive PTY is in use.
///
/// A `wd --exec` dumping hundreds of KB (a live case: a 407 KB Elasticsearch
/// `_search`) would otherwise hold the wire for its whole burst and freeze the
/// owner's console. A quarter budget caps one blocking stretch at ~55 ms
/// (serial) / ~136 ms (RFCOMM) and still leaves exec far more throughput than
/// anything but a bulk dump needs.
const PUMP_BUDGET_EXEC_SHARED: usize = 4 * MAX_PAYLOAD;

/// How long a PTY has to stay silent before exec gets its full budget back.
///
/// The quarter budget used to apply for as long as a console was merely
/// *open*, and a console is open-and-idle most of its life: measured live
/// 2026-09-12, a 407 KB dump took 61 s beside an untouched console against
/// 15 s alone. Now the throttle holds only while the console is in use —
/// keystrokes, resizes or output within this window.
///
/// The price is the first keystroke after a pause. The host reads one packet
/// per tick, after pumping, so that keystroke waits out one full-budget
/// stretch (~218 ms serial, ~533 ms RFCOMM) — and one more for each packet
/// queued ahead of it. The client's heartbeat, every 2 s, lands there about
/// one time in ten. From its echo on the throttle is back. None of it applies
/// unless exec has a bulk dump pending; two seconds spans the gaps inside
/// ordinary typing.
const PTY_QUIET_BEFORE_FULL_EXEC: Duration = Duration::from_secs(2);

/// Pure helper — the exec slot's per-tick byte budget. Extracted so the
/// yield-to-the-console rule can be unit-tested without spawning shells or
/// reading the clock.
///
/// `pty_quiet_for` is how long the PTY has been silent, `None` when no PTY is
/// open. With none open, or one silent long enough, this is the pre-two-slot
/// number, so a lone `wd --exec` streams at exactly the rate it always did.
fn exec_pump_budget(pty_quiet_for: Option<Duration>) -> usize {
    match pty_quiet_for {
        Some(quiet) if quiet < PTY_QUIET_BEFORE_FULL_EXEC => PUMP_BUDGET_EXEC_SHARED,
        _ => PUMP_BUDGET,
    }
}

/// Which of the host's two shell slots something belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellSlot {
    /// `wd --exec` — pipe-mode, one command at a time, fed by the warm shell.
    Exec,
    /// Interactive `wd` — PTY-mode, lives for minutes with a human watching.
    Pty,
}

impl ShellSlot {
    fn name(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Pty => "pty",
        }
    }
}

/// The order `pump_shell_events` visits the slots in.
///
/// PTY first, deliberately: it carries a human's keystroke echo, where a few
/// hundred ms of added delay is the difference between a usable console and an
/// unusable one, and it is never the slot producing hundreds of KB.
fn pump_order() -> [ShellSlot; 2] {
    [ShellSlot::Pty, ShellSlot::Exec]
}

/// The host's PTY slot plus how the client opened it.
struct PtySlot {
    proc: ShellProcess,
    /// `true` when opened with the pre-2026-09-12 `ShellOpenPty` opcode.
    ///
    /// A legacy PTY speaks the old `ShellInput`/`ShellOutput`/`ShellExit`/
    /// `ShellClose` opcodes and takes the whole shell side exclusively,
    /// because the client that opened it has no idea a second slot exists —
    /// it would mis-route anything the exec slot sent back. A PTY opened with
    /// `PtyOpen` speaks the `Pty*` opcodes and coexists with `wd --exec`.
    legacy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SessionState {
    WaitingForHello,
    Connected,
    Disconnected,
}

pub struct Session<T: Transport, I: InputInjector> {
    transport: T,
    injector: I,
    state: SessionState,
    seq: u16,
    last_heartbeat_sent: Instant,
    last_heartbeat_recv: Instant,
    host_name: String,
    screen_w: u16,
    screen_h: u16,
    /// Pipe-mode shell driving `wd --exec`. Fed by [`Self::warm`].
    exec: Option<ShellProcess>,
    /// PTY-mode shell driving interactive `wd`. Independent of [`Self::exec`]
    /// unless it was opened the legacy way — see [`PtySlot::legacy`].
    pty: Option<PtySlot>,
    /// Last time the PTY slot saw input, a resize or output, or was opened.
    /// Meaningless while [`Self::pty`] is `None`. Drives the exec budget —
    /// see [`PTY_QUIET_BEFORE_FULL_EXEC`].
    pty_last_activity: Instant,
    /// A shell started ahead of time, waiting to be handed to the next
    /// `ShellOpen`, together with the argv it was started with.
    ///
    /// PowerShell needs ~210 ms from `CreateProcess` to answering its first
    /// line of stdin (measured on the Win11 host, 2026-09-11), and a
    /// `wd --exec` is one open, one line and one close - so that warm-up
    /// was the single largest item in a command's latency, larger than the
    /// wire and the command itself put together. Starting the next shell
    /// the moment the previous one is handed over moves the warm-up into
    /// the gap between commands, where nobody is waiting for it.
    warm: Option<(Vec<String>, ShellProcess)>,
    /// Whether to pre-warm at all. Off in the unit-test fixtures: they
    /// handshake dozens of times and every one of those would otherwise
    /// leave an interactive `/bin/bash` behind on the dev machine. The
    /// dedicated warm-shell test turns it back on.
    warm_enabled: bool,
    clipboard: ClipboardSync,
    /// Latest client display name reported via Hello (None until handshake).
    client_name: Option<String>,
    /// Frame-error storm detector. Incremented on each `Protocol` recv error
    /// (via `note_protocol_error`), reset on each successfully decoded
    /// packet (in `tick`). When it fires, `session_thread` reopens the port.
    storm: StormCounter,
}

/// Split a tick's worth of a shell's output into packets.
///
/// The protocol takes a payload of up to 4096 bytes, so every packet but the
/// last is full. It used to
/// be cut at 480 — the limit back when `MAX_PAYLOAD` was 512 — and that
/// literal stayed behind when the limit was raised to 4096, turning every
/// full read into nine packets instead of one. Nothing was lost by it, but
/// the busiest path in the project (the output of `wd --exec`) paid nine
/// transport writes for one read, which is what bulk output over Bluetooth
/// was actually spending its time on.
fn split_shell_output(chunk: &[u8]) -> std::slice::Chunks<'_, u8> {
    chunk.chunks(MAX_PAYLOAD)
}

impl<T: Transport, I: InputInjector> Session<T, I> {
    /// Convenience ctor with default (zero-init) progress counters and a
    /// default-on `receive_files` toggle. Used by the `#[cfg(test)]` fixtures;
    /// production wiring goes through `with_counters_and_toggles` directly so
    /// the overlay sees the same atomics.
    #[cfg(test)]
    pub fn new(transport: T, injector: I, host_name: String, screen_w: u16, screen_h: u16) -> Self {
        let mut s = Self::with_counters_and_toggles(
            transport,
            injector,
            host_name,
            screen_w,
            screen_h,
            ProgressCounters::default(),
            Arc::new(AtomicBool::new(true)),
        );
        s.warm_enabled = false;
        s
    }

    /// Opt back into pre-warming for the one test that exercises it.
    #[cfg(test)]
    pub fn enable_warm_shell(&mut self) {
        self.warm_enabled = true;
    }

    #[cfg(test)]
    pub fn has_warm_shell(&self) -> bool {
        self.warm.is_some()
    }

    /// Full ctor: progress counters plus a `receive_files` runtime toggle
    /// threaded through to `ClipboardSync::with_counters_and_toggles`. Production
    /// session-thread spawn wires the toggle from `HostConfig.receive_files`
    /// (a `false` in TOML disables incoming `FORMAT_FILE` offers at boot). The
    /// Arc is owned by `main` and shared with the Settings UI, which `store`s
    /// into it on Save — so the flag IS live-mutable from outside the session
    /// loop: the toggle applies without a process restart (matches the Mac
    /// side's live `send_images`/`receive_images` toggles).
    pub fn with_counters_and_toggles(
        transport: T,
        injector: I,
        host_name: String,
        screen_w: u16,
        screen_h: u16,
        counters: ProgressCounters,
        receive_files: Arc<AtomicBool>,
    ) -> Self {
        let now = Instant::now();
        Self {
            transport,
            injector,
            state: SessionState::WaitingForHello,
            seq: 0,
            last_heartbeat_sent: now,
            last_heartbeat_recv: now,
            host_name,
            screen_w,
            screen_h,
            exec: None,
            pty: None,
            pty_last_activity: now,
            warm: None,
            warm_enabled: true,
            // Unit tests get a clipboard with no OS backend: see
            // `ClipboardSync::new_for_test_with`.
            #[cfg(not(test))]
            clipboard: ClipboardSync::with_counters_and_toggles(counters, receive_files),
            #[cfg(test)]
            clipboard: ClipboardSync::new_for_test_with(counters, receive_files),
            client_name: None,
            storm: StormCounter::new(DEFAULT_STORM_THRESHOLD),
        }
    }

    pub fn current_state(&self) -> SessionState {
        self.state
    }

    pub fn client_name(&self) -> Option<&str> {
        self.client_name.as_deref()
    }

    #[cfg(test)]
    pub fn state(&self) -> SessionState {
        self.state
    }

    #[cfg(test)]
    pub fn clipboard_state(&self) -> &ClipboardSync {
        &self.clipboard
    }

    /// Drain any transient warning the clipboard layer queued up since the
    /// last call (e.g., "image too large"). The session thread forwards
    /// this to the tray UI as a balloon notification.
    pub fn take_clipboard_warning(&mut self) -> Option<String> {
        self.clipboard.take_warning()
    }

    #[cfg(test)]
    #[allow(dead_code)] // consumed by tests that are themselves platform-gated
    pub fn has_exec_shell(&self) -> bool {
        self.exec.is_some()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn has_pty_shell(&self) -> bool {
        self.pty.is_some()
    }

    /// Test-only: park a pipe-mode process in the PTY slot.
    ///
    /// A real PTY needs ConPTY, so `ShellProcess::spawn(.., Some(..))` only
    /// works on Windows and the Mac test runs can't open one. Routing,
    /// opcode selection and teardown are slot-shaped, not backend-shaped, so
    /// a pipe child stands in for the PTY and lets all of that be covered
    /// where the tests actually run.
    #[cfg(test)]
    pub fn inject_pty_for_test(&mut self, legacy: bool, shell: &str) {
        let proc = ShellProcess::spawn(shell, None).expect("test shell");
        self.pty = Some(PtySlot { proc, legacy });
        self.note_pty_activity();
    }

    /// Test-only: open the exec slot with its output fed by the test instead
    /// of by the child, so the exact shape of the reads is under control.
    #[cfg(test)]
    pub fn exec_events_for_test(&mut self) -> std::sync::mpsc::Sender<ShellEvent> {
        let mut proc = ShellProcess::spawn("", None).expect("test shell");
        let (tx, rx) = std::sync::mpsc::channel();
        proc.events_rx = rx;
        self.exec = Some(proc);
        tx
    }

    /// Test-only: make the PTY look silent for longer than
    /// [`PTY_QUIET_BEFORE_FULL_EXEC`], without sleeping.
    #[cfg(test)]
    pub fn silence_pty_for_test(&mut self) {
        self.pty_last_activity =
            Instant::now() - PTY_QUIET_BEFORE_FULL_EXEC - Duration::from_secs(1);
    }

    fn note_pty_activity(&mut self) {
        self.pty_last_activity = Instant::now();
    }

    /// The exec slot's chunk budget as of `now` — see [`exec_pump_budget`].
    fn exec_budget_at(&self, now: Instant) -> usize {
        exec_pump_budget(
            self.pty
                .as_ref()
                .map(|_| now.saturating_duration_since(self.pty_last_activity)),
        )
    }

    /// Test-only: rewind `last_heartbeat_recv` so the next tick() sees the
    /// heartbeat-timeout branch and drives the disconnect cleanup path.
    #[cfg(test)]
    pub fn force_heartbeat_timeout(&mut self) {
        // Use the busy timeout so the rewind triggers regardless of which
        // branch the runtime check picks.
        self.last_heartbeat_recv = Instant::now() - HEARTBEAT_TIMEOUT_BUSY - Duration::from_secs(1);
    }

    /// Effective heartbeat timeout — extended while wire is saturated, so
    /// false-positive disconnects don't kill the channel mid-transfer.
    /// Two saturation sources today:
    ///
    /// 1. **Clipboard** — chunked image / large text transfer in flight.
    /// 2. **Shell** — `wd --exec` (especially `--ssh ALIAS curl ...`)
    ///    streaming command output back. With CH340 @ 115200 baud the wire
    ///    runs at ~11 KB/s; a 24 KB ES `_search` response monopolises it
    ///    for ~2 seconds plus any server-side delay, and the client's
    ///    writer thread (which also services heartbeats) can fall further
    ///    behind if it's writing back too. With the prior 6s idle timeout
    ///    the channel reliably tore down on every non-trivial ES read.
    ///    Live-test 2026-05-06: 43s between `opening shell` and
    ///    `heartbeat timeout — disconnecting` for an ES `_search?size=1`
    ///    query (~24 KB JSON response). With the busy budget (30s) plus
    ///    the natural prefix of MOTD / ssh hop the channel survives.
    ///
    /// "Either slot is open" is the simplest signal — true between an open
    /// and its close. Worst case if the shell is genuinely idle (e.g., user
    /// opened a console and walked away), we wait 30s instead of 6s before
    /// tearing down. Acceptable — and an interactive `wd` is precisely the
    /// case where a long silence is normal.
    fn heartbeat_timeout(&self) -> Duration {
        heartbeat_timeout_for(
            self.clipboard.transfer_in_flight(),
            self.exec.is_some() || self.pty.is_some(),
        )
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    fn send(&mut self, msg: Message) -> Result<()> {
        let seq = self.next_seq();
        let packet = Packet::new(msg, seq);
        self.transport.send(&packet)
    }

    /// Process one incoming packet. Returns Ok(true) if packet was processed,
    /// Ok(false) if no packet available (timeout), Err on fatal error.
    pub fn tick(&mut self) -> Result<bool> {
        // Send heartbeat if needed
        if self.state == SessionState::Connected
            && self.last_heartbeat_sent.elapsed() >= HEARTBEAT_INTERVAL
        {
            self.send(Message::Heartbeat)?;
            self.last_heartbeat_sent = Instant::now();
        }

        // Check heartbeat timeout
        if self.state == SessionState::Connected
            && self.last_heartbeat_recv.elapsed() >= self.heartbeat_timeout()
        {
            log::warn!("heartbeat timeout — disconnecting");
            self.injector.release_all()?;
            self.kill_all_shells();
            self.warm_kill();
            self.clipboard.reset();
            self.state = SessionState::WaitingForHello;
            self.client_name = None;
            return Ok(false);
        }

        // Drain pending shell output and exit events without blocking
        self.pump_shell_events()?;

        // Push local clipboard changes (poll-rate-limited internally).
        if self.state == SessionState::Connected {
            for msg in self.clipboard.poll() {
                self.send(msg)?;
            }
        }

        // Try to receive a packet
        let packet = match self.transport.recv() {
            Ok(p) => p,
            Err(WireDeskError::Transport(ref msg)) if msg.contains("timeout") => {
                return Ok(false);
            }
            Err(e) => return Err(e),
        };

        // A real packet decoded → the channel is alive; clear the storm run
        // BEFORE handling (Codex iter3 P3): a handler error (e.g. injector
        // failure on a key event) returns early via `?`, and a decoded frame
        // must still break the protocol-error streak — the wire is fine, the
        // failure is local. This is the SINGLE reset site: the other Ok-paths
        // of tick() (heartbeat-timeout, recv-timeout) return without a decoded
        // packet, and resetting there would break "timeouts don't participate".
        self.storm.on_valid_packet();
        self.handle_packet(packet)?;
        Ok(true)
    }

    /// Record one protocol (decode) error from the recv path. Returns `true`
    /// once the consecutive-error run reaches the storm threshold, signalling
    /// `session_thread` to reopen the transport. Delegates to the internal
    /// [`StormCounter`]; `tick` resets the run on every decoded packet.
    pub fn note_protocol_error(&mut self) -> bool {
        self.storm.on_protocol_error()
    }

    /// Current consecutive protocol-error count (test/diagnostic hook).
    #[cfg(test)]
    pub fn storm_count(&self) -> u32 {
        self.storm.count()
    }

    /// Decompose the session, returning the injector and dropping the
    /// transport (which releases the underlying COM-port handle). Used by
    /// the reopen loop: the injector is built once via a `FnOnce` and must
    /// survive across transport reopens, so the old session is dismantled
    /// and the injector migrates into the freshly-opened one.
    ///
    /// Releases all held input first (Codex iter5 P2): if the link died
    /// mid-keypress / mid-drag, the heartbeat-timeout path would normally
    /// `release_all` — but the reopen paths consume the session directly,
    /// and without this Windows would keep the key/button stuck down across
    /// the reconnect.
    pub fn into_injector(mut self) -> I {
        if let Err(e) = self.injector.release_all() {
            log::warn!("release_all during session teardown failed: {e}");
        }
        self.injector
    }

    /// Drain stdout/stderr from both shell slots into outbound packets, and
    /// notify the client when either shell exits. See [`pump_order`] for why
    /// the console goes first and [`exec_pump_budget`] for what `wd --exec`
    /// gives up while it is in use.
    fn pump_shell_events(&mut self) -> Result<()> {
        for slot in pump_order() {
            self.pump_slot(slot)?;
        }
        Ok(())
    }

    /// One slot's share of a tick: drain up to its budget of bytes, ship them
    /// on the opcodes that slot speaks, and report an exit.
    fn pump_slot(&mut self, slot: ShellSlot) -> Result<()> {
        let budget = match slot {
            ShellSlot::Pty => PUMP_BUDGET,
            ShellSlot::Exec => self.exec_budget_at(Instant::now()),
        };

        // Everything the shell has handed over, glued into one buffer: a shell
        // printing line by line produces reads of a few dozen bytes, and one
        // packet per read wastes both the budget and a transport write each.
        let mut output: Vec<u8> = Vec::new();
        let mut exit_code: Option<i32> = None;

        {
            let proc = match slot {
                ShellSlot::Exec => self.exec.as_ref(),
                ShellSlot::Pty => self.pty.as_ref().map(|p| &p.proc),
            };
            let Some(sh) = proc else {
                return Ok(());
            };
            // Whole reads only, so the last one may overshoot by under a read.
            while output.len() < budget {
                match sh.events_rx.try_recv() {
                    Ok(ShellEvent::Output(data)) => output.extend_from_slice(&data),
                    Ok(ShellEvent::Exit(code)) => {
                        exit_code = Some(code);
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
            }
        }

        // Which opcodes this slot answers on. The exec slot always uses the
        // originals; a PTY uses them only when it was opened the legacy way,
        // because that client knows no others. Decided before the slot can be
        // cleared below.
        let legacy_opcodes = match slot {
            ShellSlot::Exec => true,
            ShellSlot::Pty => self.pty.as_ref().is_some_and(|p| p.legacy),
        };

        for piece in split_shell_output(&output) {
            let data = piece.to_vec();
            self.send(if legacy_opcodes {
                Message::ShellOutput { data }
            } else {
                Message::PtyOutput { data }
            })?;
        }

        // Output counts as use: a console running `ping` or a build keeps the
        // exec throttle on just like typing does. Marked after the sends, not
        // before: on a slow link they can outlast the whole quiet window, and
        // exec — pumped next — would find the mark already stale.
        if slot == ShellSlot::Pty && !output.is_empty() {
            self.note_pty_activity();
        }

        // Detect process exit even if we didn't get an Exit event
        if exit_code.is_none() {
            exit_code = match slot {
                ShellSlot::Exec => self.exec.as_mut().and_then(|sh| sh.try_exit_code()),
                ShellSlot::Pty => self.pty.as_mut().and_then(|p| p.proc.try_exit_code()),
            };
        }

        if let Some(code) = exit_code {
            log::info!("{} shell exited with code {code}", slot.name());
            match slot {
                ShellSlot::Exec => self.exec = None,
                ShellSlot::Pty => self.pty = None,
            }
            self.send(if legacy_opcodes {
                Message::ShellExit { code }
            } else {
                Message::PtyExit { code }
            })?;
        }

        Ok(())
    }

    fn exec_kill(&mut self) {
        if let Some(mut sh) = self.exec.take() {
            sh.kill();
        }
    }

    fn pty_kill(&mut self) {
        if let Some(mut slot) = self.pty.take() {
            slot.proc.kill();
        }
    }

    /// Kill whatever runs in either slot. Every teardown path — disconnect,
    /// re-handshake, heartbeat timeout — goes through here: the client that
    /// owned these shells is gone, and neither slot may outlive it.
    fn kill_all_shells(&mut self) {
        self.exec_kill();
        self.pty_kill();
    }

    /// Hand over the pre-warmed shell when it is the one being asked for,
    /// otherwise start a fresh one. A warm shell that doesn't match is
    /// kept, not discarded: the mismatch is a one-off `ShellOpen` for
    /// `cmd`, and the next `wd --exec` will want PowerShell again.
    ///
    /// Anything the warm shell buffered while it sat idle is dropped on
    /// the way out. PowerShell in pipe mode prints nothing before its
    /// first input, but a shell that has been waiting has had time to
    /// print something unexpected, and that would otherwise arrive in
    /// front of the command's own output.
    fn take_warm_or_spawn(&mut self, requested: &str) -> Result<ShellProcess> {
        let want = shell_argv(requested);
        if self.warm.as_ref().is_some_and(|(argv, _)| *argv == want) {
            let (_, proc) = self.warm.take().expect("checked just above");
            let dropped = proc.drain_pending();
            log::info!("opening shell '{requested}' (pre-warmed, {dropped} stale chunks dropped)");
            return Ok(proc);
        }
        log::info!("opening shell '{requested}'");
        ShellProcess::spawn(requested, None)
    }

    /// Start the shell the next `ShellOpen` will most likely ask for. Only
    /// ever the default one - a request for anything else is rare enough
    /// that guessing would just leave a stray process around.
    fn warm_up(&mut self) {
        if !self.warm_enabled || self.warm.is_some() {
            return;
        }
        match ShellProcess::spawn("", None) {
            Ok(proc) => self.warm = Some((shell_argv(""), proc)),
            // Not fatal: the next `ShellOpen` simply spawns its own.
            Err(e) => log::warn!("could not pre-warm a shell: {e}"),
        }
    }

    /// Kill the pre-warmed shell. Called when the client goes away - an
    /// idle PowerShell should not outlive the link that would have used it.
    fn warm_kill(&mut self) {
        if let Some((_, mut proc)) = self.warm.take() {
            proc.kill();
        }
    }

    fn handle_packet(&mut self, packet: Packet) -> Result<()> {
        match (&self.state, &packet.message) {
            (
                SessionState::WaitingForHello,
                Message::Hello {
                    version,
                    client_name,
                },
            ) => {
                if *version != VERSION {
                    log::warn!("version mismatch: client v{version}, host v{VERSION}");
                    self.send(Message::Error {
                        code: 1,
                        msg: format!("unsupported version {version}, expected {VERSION}"),
                    })?;
                    return Ok(());
                }
                log::info!("HELLO from '{client_name}' v{version}");
                self.client_name = Some(client_name.clone());
                // Announce the generation, not the `Hello` version: this is
                // how the client learns whether it may use the pty slot.
                self.send(Message::HelloAck {
                    version: HOST_PROTO_VERSION,
                    host_name: self.host_name.clone(),
                    screen_w: self.screen_w,
                    screen_h: self.screen_h,
                })?;
                self.state = SessionState::Connected;
                self.last_heartbeat_recv = Instant::now();
                log::info!("connected (screen: {}x{})", self.screen_w, self.screen_h);
                // Have a shell ready before the first `wd --exec` asks for
                // one; without this the very first command still pays the
                // full PowerShell warm-up.
                self.warm_up();
            }

            (SessionState::Connected, Message::Heartbeat) => {
                self.last_heartbeat_recv = Instant::now();
            }

            (SessionState::Connected, Message::MouseMove { x, y }) => {
                self.injector.mouse_move_absolute(*x, *y)?;
            }

            (SessionState::Connected, Message::MouseButton { button, pressed }) => {
                self.injector.mouse_button(*button, *pressed)?;
            }

            (SessionState::Connected, Message::MouseScroll { delta_x, delta_y }) => {
                self.injector.mouse_scroll(*delta_x, *delta_y)?;
            }

            (
                SessionState::Connected,
                Message::KeyDown {
                    scancode,
                    modifiers,
                },
            ) => {
                self.injector.key_down(*scancode, *modifiers)?;
            }

            (
                SessionState::Connected,
                Message::KeyUp {
                    scancode,
                    modifiers,
                },
            ) => {
                self.injector.key_up(*scancode, *modifiers)?;
            }

            (SessionState::Connected, Message::ClipOffer { format, total_len }) => {
                if let Some(decline) = self.clipboard.on_offer(*format, *total_len) {
                    // Forward the policy-decline back to the peer so it
                    // drops its outbox and stops streaming chunks we're
                    // going to discard — without this the link's RX
                    // direction stays full of data we ignore, starving
                    // TX (mouse, heartbeats, the decline itself).
                    self.send(decline)?;
                }
            }

            (SessionState::Connected, Message::ClipChunk { index, data }) => {
                self.clipboard.on_chunk(*index, data.clone());
            }

            (SessionState::Connected, Message::ClipDecline { format }) => {
                // Peer doesn't want this transfer (its receive_* toggle is
                // off). Drop the pending outbox so we stop saturating the
                // wire with chunks the peer is going to discard. Without
                // this the link's RX direction stays full of data the peer
                // ignores, starving its TX (mouse, heartbeats, decline
                // ack itself) and triggering a heartbeat timeout.
                let dropped = self.clipboard.cancel_outgoing();
                if dropped > 0 {
                    log::info!(
                        "clipboard: peer declined offer (format={format}); dropped {dropped} queued packets"
                    );
                }
                // Task 7d: surface a tray-balloon for FORMAT_FILE so the
                // user has parity with the Mac toast "Peer declined file".
                // Other formats already had no UI feedback historically —
                // keeping that behaviour to avoid noise.
                if *format == wiredesk_protocol::message::FORMAT_FILE {
                    self.clipboard
                        .push_warning("Peer declined file (Receive files off)".into());
                }
            }

            (SessionState::Connected, Message::ShellOpen { shell }) => {
                // A legacy PTY holds the whole shell side: its client predates
                // the second slot and would mis-read anything exec sent back.
                if self.exec.is_some() || self.pty.as_ref().is_some_and(|p| p.legacy) {
                    log::warn!("ShellOpen received but the exec slot is already taken");
                    self.send(Message::Error {
                        code: ERR_SHELL_BUSY,
                        msg: "shell already open".into(),
                    })?;
                } else {
                    match self.take_warm_or_spawn(shell) {
                        Ok(proc) => {
                            self.exec = Some(proc);
                            // Start the next one now, so its warm-up runs
                            // while this command is still being typed, sent
                            // and executed.
                            self.warm_up();
                        }
                        Err(e) => {
                            log::error!("failed to spawn shell: {e}");
                            self.send(Message::Error {
                                code: ERR_SHELL_SPAWN,
                                msg: format!("shell spawn: {e}"),
                            })?;
                        }
                    }
                }
            }

            (SessionState::Connected, Message::ShellOpenPty { shell, cols, rows }) => {
                // The *legacy* open: this client speaks the pre-two-slot
                // protocol, so it gets the pre-two-slot behaviour — one shell
                // for the whole host, original opcodes in both directions.
                if self.pty.is_some() || self.exec.is_some() {
                    log::warn!("ShellOpenPty received but a shell is already running");
                    self.send(Message::Error {
                        code: ERR_SHELL_BUSY,
                        msg: "shell already open".into(),
                    })?;
                } else {
                    log::info!("opening pty shell '{shell}' ({cols}x{rows}, legacy=true)");
                    match ShellProcess::spawn(shell, Some((*cols, *rows))) {
                        Ok(proc) => self.pty = Some(PtySlot { proc, legacy: true }),
                        Err(e) => {
                            log::error!("failed to spawn pty shell: {e}");
                            self.send(Message::Error {
                                code: ERR_SHELL_SPAWN,
                                msg: format!("pty shell spawn: {e}"),
                            })?;
                        }
                    }
                }
            }

            (SessionState::Connected, Message::PtyOpen { shell, cols, rows }) => {
                // The dedicated slot: an open exec shell is none of its
                // business, which is the whole point of the second slot.
                if self.pty.is_some() {
                    log::warn!("PtyOpen received but the pty slot is already taken");
                    self.send(Message::Error {
                        code: ERR_PTY_BUSY,
                        msg: "pty shell already open".into(),
                    })?;
                } else {
                    log::info!("opening pty shell '{shell}' ({cols}x{rows}, legacy=false)");
                    match ShellProcess::spawn(shell, Some((*cols, *rows))) {
                        Ok(proc) => {
                            self.pty = Some(PtySlot {
                                proc,
                                legacy: false,
                            });
                            // The prompt is about to be drawn.
                            self.note_pty_activity();
                        }
                        Err(e) => {
                            log::error!("failed to spawn pty shell: {e}");
                            self.send(Message::Error {
                                code: ERR_PTY_SPAWN,
                                msg: format!("pty shell spawn: {e}"),
                            })?;
                        }
                    }
                }
            }

            (SessionState::Connected, Message::PtyResize { cols, rows }) => {
                // Both kinds of PTY resize the same way — the opcode predates
                // the split and stayed shared.
                if let Some(slot) = self.pty.as_ref() {
                    slot.proc.resize(*cols, *rows);
                    // A resize makes the console redraw.
                    self.note_pty_activity();
                }
                // No pty open → silently ignore. Pre-spawn resize is
                // a benign race when client computes initial size in
                // parallel with the open.
            }

            (SessionState::Connected, Message::ShellInput { data }) => {
                // Addresses the exec slot. With no exec shell it falls through
                // to a *legacy* PTY — the only shell such a client can have
                // open. It must never fall through to a `PtyOpen` PTY: that
                // client has `PtyInput` for it, so a `ShellInput` arriving
                // here is a stray from an older stream, and feeding it to a
                // live console would type someone else's keystrokes into it.
                let target = if self.exec.is_some() {
                    self.exec.as_ref()
                } else {
                    self.pty.as_ref().filter(|p| p.legacy).map(|p| &p.proc)
                };
                match target {
                    Some(sh) => {
                        if !sh.write(data.clone()) {
                            log::warn!("shell stdin writer is gone");
                        }
                    }
                    None => log::warn!("ShellInput with no exec shell to take it — dropped"),
                }
            }

            (SessionState::Connected, Message::PtyInput { data }) => {
                match self.pty.as_ref().filter(|p| !p.legacy) {
                    Some(slot) => {
                        if !slot.proc.write(data.clone()) {
                            log::warn!("pty stdin writer is gone");
                        }
                        // Throttle exec before the echo exists, not after.
                        self.note_pty_activity();
                    }
                    // Either nothing is open, or the open PTY is a legacy one
                    // whose client speaks `ShellInput`. Both mean this frame
                    // has no owner.
                    None => {
                        log::warn!("PtyInput with no dedicated pty shell to take it — dropped")
                    }
                }
            }

            (SessionState::Connected, Message::ShellClose) => {
                // Close stdin first so the shell sees EOF — bash/zsh
                // (and most well-behaved CLIs) exit cleanly. PowerShell
                // launched with -NoExit ignores stdin EOF and keeps
                // running, leaving `self.shell` stuck at `Some(...)`
                // which then makes the *next* ShellOpen fail with
                // "shell already open". Force-kill after the close so
                // the slot is always free when the client re-opens.
                //
                // Routing mirrors `ShellInput`: the exec slot first, a legacy
                // PTY second, never a `PtyOpen` one.
                let had_shell = if self.exec.is_some() {
                    if let Some(sh) = self.exec.as_ref() {
                        sh.close();
                    }
                    self.exec_kill();
                    true
                } else if self.pty.as_ref().is_some_and(|p| p.legacy) {
                    if let Some(slot) = self.pty.as_ref() {
                        slot.proc.close();
                    }
                    self.pty_kill();
                    true
                } else {
                    false
                };
                // Answer the close. The client holds its exec slot until the
                // wire goes quiet, and with nothing coming back that meant
                // waiting out a fixed idle window on every single command -
                // 150 ms of pure latency between back-to-back `wd --exec`
                // runs. One packet turns that into one round trip.
                //
                // Only when a shell was actually here: if it had already
                // exited on its own, `tick` has sent a `ShellExit` with the
                // real status, and an acknowledgement on top of it would be
                // one more packet nobody is waiting for.
                if had_shell {
                    self.send(Message::ShellClosed)?;
                }
            }

            (SessionState::Connected, Message::PtyClose) => {
                // No acknowledgement, unlike `ShellClose`. `PtyClose` only
                // comes from the interactive relay's teardown, which does not
                // wait for one (it discards `ShellClosed` too); answering
                // would just drop a stray packet into whatever the exec slot
                // is streaming at that moment.
                if self.pty.as_ref().is_some_and(|p| !p.legacy) {
                    if let Some(slot) = self.pty.as_ref() {
                        slot.proc.close();
                    }
                    self.pty_kill();
                } else {
                    log::warn!("PtyClose with no dedicated pty shell open — ignored");
                }
            }

            (SessionState::Connected, Message::Disconnect) => {
                log::info!("client disconnected");
                self.injector.release_all()?;
                self.kill_all_shells();
                self.warm_kill();
                self.clipboard.reset();
                self.state = SessionState::WaitingForHello;
                self.client_name = None;
            }

            (_, Message::Hello { .. }) => {
                // Re-handshake from any state — drop in-flight clipboard
                // reassembly so a half-finished transfer doesn't leak
                // across sessions, AND kill any leftover shell so the
                // new client's ShellOpen doesn't bounce off "shell
                // already open" (typical when the previous wiredesk-term
                // exited too fast for heartbeat-timeout to fire).
                self.injector.release_all().ok();
                self.clipboard.reset();
                self.kill_all_shells();
                self.state = SessionState::WaitingForHello;
                self.client_name = None;
                self.handle_packet(packet)?;
            }

            (state, msg) => {
                log::debug!("ignored {msg:?} in state {state:?}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::injector::MockInjector;
    use wiredesk_transport::mock::MockTransport;

    fn setup() -> (Session<MockTransport, MockInjector>, MockTransport) {
        let (host_transport, client_transport) = MockTransport::pair();
        let injector = MockInjector::default();
        let session = Session::new(host_transport, injector, "test-host".into(), 1920, 1080);
        (session, client_transport)
    }

    /// Hello → HelloAck, leaving the session `Connected` so a test can reach
    /// the shell arms. Drops the ack; tests that care read it themselves.
    fn connect(session: &mut Session<MockTransport, MockInjector>, client: &mut MockTransport) {
        client
            .send(&Packet::new(
                Message::Hello {
                    version: VERSION,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();
    }

    /// Drive the session until a packet the predicate accepts comes back, or
    /// give up and return `None` — which is how a test asserts that something
    /// must *not* arrive without hanging the runner.
    ///
    /// Every pump feeds the session a heartbeat first: `MockTransport::recv`
    /// blocks and `tick` always reaches it, so a tick with nothing to consume
    /// would never return. A heartbeat is the cheapest packet that changes
    /// nothing else. The loop also gives a real child process time to echo —
    /// output arrives on a reader thread, not synchronously.
    fn pump_until(
        session: &mut Session<MockTransport, MockInjector>,
        client: &mut MockTransport,
        mut want: impl FnMut(&Message) -> bool,
    ) -> Option<Message> {
        for i in 0..25u16 {
            client
                .send(&Packet::new(Message::Heartbeat, 1000 + i))
                .unwrap();
            session.tick().unwrap();
            while let Some(p) = client.recv_timeout(Duration::from_millis(20)) {
                if want(&p.message) {
                    return Some(p.message);
                }
            }
        }
        None
    }

    /// Only the echo tests below look at output opcodes, and those are
    /// Unix-only — see `ECHO_SHELL`.
    #[cfg(not(target_os = "windows"))]
    fn is_output(m: &Message) -> bool {
        matches!(m, Message::ShellOutput { .. } | Message::PtyOutput { .. })
    }

    #[test]
    fn a_warm_shell_is_ready_before_the_first_command_and_refilled_after_it() {
        // PowerShell needs ~210 ms from spawn to reading its first line of
        // stdin, and a `wd --exec` is open-write-close - so the shell has
        // to exist before the command arrives, or that warm-up lands in
        // the user's latency.
        let (mut session, mut client) = setup();
        session.enable_warm_shell();
        assert!(!session.has_warm_shell(), "nothing spawned before a client");

        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();
        assert!(
            session.has_warm_shell(),
            "the handshake must leave a shell warming up"
        );

        client
            .send(&Packet::new(
                Message::ShellOpen {
                    shell: String::new(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(session.has_exec_shell(), "the warm shell was handed over");
        assert!(
            session.has_warm_shell(),
            "and the next one started right away"
        );

        // The client going away must not leave an idle shell behind.
        client.send(&Packet::new(Message::Disconnect, 2)).unwrap();
        session.tick().unwrap();
        assert!(!session.has_exec_shell());
        assert!(!session.has_warm_shell(), "no shell outlives the link");
    }

    #[test]
    fn handshake() {
        let (mut session, mut client) = setup();
        assert_eq!(session.state(), SessionState::WaitingForHello);

        // Client sends HELLO
        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();

        session.tick().unwrap();
        assert_eq!(session.state(), SessionState::Connected);

        // Host should have sent HELLO_ACK
        let ack = client.recv().unwrap();
        match ack.message {
            Message::HelloAck {
                screen_w, screen_h, ..
            } => {
                assert_eq!(screen_w, 1920);
                assert_eq!(screen_h, 1080);
            }
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    #[test]
    fn input_forwarding() {
        let (mut session, mut client) = setup();

        // Handshake first
        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        // Send mouse move
        client
            .send(&Packet::new(Message::MouseMove { x: 100, y: 200 }, 1))
            .unwrap();
        session.tick().unwrap();

        // Send key
        client
            .send(&Packet::new(
                Message::KeyDown {
                    scancode: 0x1E,
                    modifiers: 0x01,
                },
                2,
            ))
            .unwrap();
        session.tick().unwrap();

        // Verify injector received events
        // Note: we need to access injector through session, but it's moved in.
        // For now, just verify no errors occurred.
    }

    #[test]
    fn disconnect_releases_keys() {
        let (mut session, mut client) = setup();

        // Handshake
        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        // Disconnect
        client.send(&Packet::new(Message::Disconnect, 1)).unwrap();
        session.tick().unwrap();

        assert_eq!(session.state(), SessionState::WaitingForHello);
    }

    #[test]
    fn rehandshake() {
        let (mut session, mut client) = setup();

        // First handshake
        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "first".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();
        assert_eq!(session.state(), SessionState::Connected);

        // Second HELLO (reconnect)
        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "second".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        assert_eq!(session.state(), SessionState::Connected);

        // Should get a new HELLO_ACK
        let ack = client.recv().unwrap();
        assert!(matches!(ack.message, Message::HelloAck { .. }));
    }

    /// Bring `session` to Connected and seed an in-flight reassembly
    /// (one ClipOffer + one ClipChunk). Returns the live client transport
    /// so the caller can keep driving messages.
    fn setup_with_partial_reassembly() -> (Session<MockTransport, MockInjector>, MockTransport) {
        let (mut session, mut client) = setup();
        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        // Push a partial reassembly: 1024-byte text offer + one 256-byte chunk.
        client
            .send(&Packet::new(
                Message::ClipOffer {
                    format: 0,
                    total_len: 1024,
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        client
            .send(&Packet::new(
                Message::ClipChunk {
                    index: 0,
                    data: vec![b'a'; 256],
                },
                2,
            ))
            .unwrap();
        session.tick().unwrap();

        assert_eq!(
            session.clipboard_state().expected_len(),
            1024,
            "precondition: in-flight reassembly must be active"
        );
        (session, client)
    }

    #[test]
    fn one_read_of_shell_output_is_one_packet() {
        // Regression for a literal that outlived its constant: the cut was
        // 480 bytes, from the days when `MAX_PAYLOAD` was 512, while the
        // shell reader hands over 4096 at a time. Every full read went out
        // as nine packets.
        assert_eq!(split_shell_output(&[]).count(), 0, "nothing to send");
        assert_eq!(split_shell_output(&[b'x'; 1]).count(), 1);
        assert_eq!(
            split_shell_output(&vec![b'x'; MAX_PAYLOAD]).count(),
            1,
            "a full read must fit in one packet"
        );
        assert_eq!(
            split_shell_output(&vec![b'x'; MAX_PAYLOAD + 1]).count(),
            2,
            "one byte over must split, not be refused by Packet::to_bytes"
        );
        // Every piece has to be acceptable to the protocol.
        for piece in split_shell_output(&vec![b'x'; MAX_PAYLOAD * 3 + 7]) {
            assert!(piece.len() <= MAX_PAYLOAD, "piece of {} bytes", piece.len());
            assert!(
                Packet::new(
                    Message::ShellOutput {
                        data: piece.to_vec()
                    },
                    0
                )
                .to_bytes()
                .is_ok(),
                "protocol refused a piece this function produced"
            );
        }
    }

    #[test]
    fn heartbeat_timeout_for_pure_logic() {
        // Idle: no clipboard, no shell — strict 6s.
        assert_eq!(
            heartbeat_timeout_for(false, false),
            HEARTBEAT_TIMEOUT_IDLE,
            "with both flags false the budget must be IDLE so unplugged cables fire fast"
        );

        // Clipboard transfer alive — busy budget. (Pre-existing behaviour.)
        assert_eq!(
            heartbeat_timeout_for(true, false),
            HEARTBEAT_TIMEOUT_BUSY,
            "clipboard transfer must extend the budget"
        );

        // Shell open (no clipboard) — busy budget. (Regression for the
        // 2026-05-06 ES `_search` channel-tear-down: 24 KB JSON response
        // monopolised the wire long enough to miss the IDLE deadline.)
        assert_eq!(
            heartbeat_timeout_for(false, true),
            HEARTBEAT_TIMEOUT_BUSY,
            "open shell must extend the budget — wire saturation kills the IDLE deadline"
        );

        // Both — still busy.
        assert_eq!(heartbeat_timeout_for(true, true), HEARTBEAT_TIMEOUT_BUSY,);
    }

    #[test]
    fn heartbeat_timeout_resets_clipboard() {
        // Heartbeat-timeout branch must drop in-flight reassembly so a
        // half-finished transfer doesn't leak into the next session.
        let (mut session, _client) = setup_with_partial_reassembly();

        session.force_heartbeat_timeout();
        let _ = session.tick(); // returns Ok(false) once the timeout fires

        assert_eq!(
            session.clipboard_state().expected_len(),
            0,
            "heartbeat timeout must reset clipboard reassembly"
        );
    }

    #[test]
    fn disconnect_resets_clipboard() {
        // Message::Disconnect must drop in-flight reassembly.
        let (mut session, mut client) = setup_with_partial_reassembly();

        client.send(&Packet::new(Message::Disconnect, 3)).unwrap();
        session.tick().unwrap();

        assert_eq!(
            session.clipboard_state().expected_len(),
            0,
            "Disconnect must reset clipboard reassembly"
        );
    }

    #[test]
    fn rehandshake_resets_clipboard() {
        // A fresh Hello during an active session must drop in-flight
        // reassembly so the new session starts clean.
        let (mut session, mut client) = setup_with_partial_reassembly();

        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "second".into(),
                },
                3,
            ))
            .unwrap();
        session.tick().unwrap();

        assert_eq!(
            session.clipboard_state().expected_len(),
            0,
            "re-handshake must reset clipboard reassembly"
        );
    }

    #[test]
    fn pty_resize_without_shell_is_silent_noop() {
        // Pre-spawn PtyResize is benign (client computes size in parallel
        // with ShellOpenPty). Session must accept it, ignore it, and
        // not respond with Error.
        let (mut session, mut client) = setup();
        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();

        client
            .send(&Packet::new(Message::PtyResize { cols: 80, rows: 24 }, 1))
            .unwrap();
        session.tick().unwrap();

        // Sending another packet should still work (no protocol breakage).
        client.send(&Packet::new(Message::Heartbeat, 2)).unwrap();
        session.tick().unwrap();
        assert_eq!(session.state(), SessionState::Connected);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn pty_spawn_failure_reports_the_code_that_matches_the_opcode() {
        // PTY-backed shell is Windows-only. A Mac/Linux host must surface the
        // spawn error back to the client through Message::Error — a silent
        // fallback to pipe-mode would mask a misconfigured deployment.
        //
        // The code differs by opcode, and that is the point: a legacy client
        // only knows 2/3, so `ShellOpenPty` keeps answering 3, while the
        // dedicated `PtyOpen` answers 5 so the new client can route the error
        // to its pty consumer instead of the exec one.
        for (open, want_code) in [
            (
                Message::ShellOpenPty {
                    shell: "/bin/sh".into(),
                    cols: 80,
                    rows: 24,
                },
                ERR_SHELL_SPAWN,
            ),
            (
                Message::PtyOpen {
                    shell: "/bin/sh".into(),
                    cols: 80,
                    rows: 24,
                },
                ERR_PTY_SPAWN,
            ),
        ] {
            let (mut session, mut client) = setup();
            connect(&mut session, &mut client);
            assert!(!session.has_pty_shell());

            client.send(&Packet::new(open.clone(), 1)).unwrap();
            session.tick().unwrap();

            assert!(
                !session.has_pty_shell(),
                "non-Windows host must refuse PTY shell ({open:?})"
            );

            match pump_until(&mut session, &mut client, |m| {
                matches!(m, Message::Error { .. })
            }) {
                Some(Message::Error { code, msg }) => {
                    assert_eq!(code, want_code, "wrong code for {open:?}: {msg}");
                }
                other => panic!("expected Message::Error for {open:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn hello_ack_announces_the_protocol_generation() {
        // This field is how the client learns the pty slot exists. Answering
        // with the `Hello` version instead would pin every client to the
        // legacy single-slot path forever.
        let (mut session, mut client) = setup();
        client
            .send(&Packet::new(
                Message::Hello {
                    version: VERSION,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        match client.recv().unwrap().message {
            Message::HelloAck { version, .. } => assert_eq!(version, HOST_PROTO_VERSION),
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }

    #[test]
    fn exec_and_dedicated_pty_coexist_and_close_independently() {
        // The whole point of the split: `wd --exec` opens while an
        // interactive console is live, and neither close touches the other.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);

        session.inject_pty_for_test(false, "");
        assert!(session.has_pty_shell());

        client
            .send(&Packet::new(
                Message::ShellOpen {
                    shell: String::new(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(
            session.has_exec_shell(),
            "a dedicated pty must not block ShellOpen"
        );
        assert!(session.has_pty_shell(), "and must survive it");

        // Closing exec leaves the console alone.
        client.send(&Packet::new(Message::ShellClose, 2)).unwrap();
        session.tick().unwrap();
        assert!(!session.has_exec_shell());
        assert!(session.has_pty_shell(), "ShellClose must not kill the pty");
        assert!(
            pump_until(&mut session, &mut client, |m| matches!(
                m,
                Message::ShellClosed
            ))
            .is_some(),
            "exec close still gets its acknowledgement"
        );

        // And PtyClose takes down only the console.
        client.send(&Packet::new(Message::PtyClose, 3)).unwrap();
        session.tick().unwrap();
        assert!(!session.has_pty_shell());
    }

    #[test]
    fn legacy_pty_still_takes_the_whole_shell_side() {
        // A client that opened with `ShellOpenPty` has no second consumer, so
        // it must keep seeing the old exclusive behaviour in both directions.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        session.inject_pty_for_test(true, "");

        client
            .send(&Packet::new(
                Message::ShellOpen {
                    shell: String::new(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(!session.has_exec_shell(), "legacy pty must refuse exec");
        match pump_until(&mut session, &mut client, |m| {
            matches!(m, Message::Error { .. })
        }) {
            Some(Message::Error { code, .. }) => assert_eq!(code, ERR_SHELL_BUSY),
            other => panic!("expected busy Error, got {other:?}"),
        }

        // The reverse: an open exec slot refuses a legacy pty open.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        client
            .send(&Packet::new(
                Message::ShellOpen {
                    shell: String::new(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(session.has_exec_shell());
        client
            .send(&Packet::new(
                Message::ShellOpenPty {
                    shell: String::new(),
                    cols: 80,
                    rows: 24,
                },
                2,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(!session.has_pty_shell());
        match pump_until(&mut session, &mut client, |m| {
            matches!(m, Message::Error { .. })
        }) {
            Some(Message::Error { code, .. }) => assert_eq!(code, ERR_SHELL_BUSY),
            other => panic!("expected busy Error, got {other:?}"),
        }
    }

    #[test]
    fn second_pty_open_is_refused_with_its_own_code() {
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        session.inject_pty_for_test(false, "");

        client
            .send(&Packet::new(
                Message::PtyOpen {
                    shell: String::new(),
                    cols: 80,
                    rows: 24,
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();

        match pump_until(&mut session, &mut client, |m| {
            matches!(m, Message::Error { .. })
        }) {
            // A distinct code, not ERR_SHELL_BUSY: the client routes this to
            // its interactive consumer, and 2 belongs to the exec one.
            Some(Message::Error { code, .. }) => assert_eq!(code, ERR_PTY_BUSY),
            other => panic!("expected pty-busy Error, got {other:?}"),
        }
    }

    #[test]
    fn shell_close_closes_a_legacy_pty_but_never_a_dedicated_one() {
        // `ShellClose` is the exec slot's opcode. A legacy pty answers it
        // because its client has nothing else; a dedicated one must not, or
        // a finishing `wd --exec` would hang up the owner's console.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        session.inject_pty_for_test(true, "");
        client.send(&Packet::new(Message::ShellClose, 1)).unwrap();
        session.tick().unwrap();
        assert!(!session.has_pty_shell(), "legacy pty closes on ShellClose");
        assert!(
            pump_until(&mut session, &mut client, |m| matches!(
                m,
                Message::ShellClosed
            ))
            .is_some(),
            "and is acknowledged"
        );

        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        session.inject_pty_for_test(false, "");
        client.send(&Packet::new(Message::ShellClose, 1)).unwrap();
        session.tick().unwrap();
        assert!(
            session.has_pty_shell(),
            "a dedicated pty must ignore ShellClose"
        );
        assert!(
            pump_until(&mut session, &mut client, |m| matches!(
                m,
                Message::ShellClosed
            ))
            .is_none(),
            "and nothing is acknowledged — there was no exec shell to close"
        );
    }

    #[test]
    fn pty_close_never_touches_the_exec_slot() {
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        client
            .send(&Packet::new(
                Message::ShellOpen {
                    shell: String::new(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(session.has_exec_shell());

        client.send(&Packet::new(Message::PtyClose, 2)).unwrap();
        session.tick().unwrap();
        assert!(
            session.has_exec_shell(),
            "PtyClose with no pty open must leave exec alone"
        );
    }

    #[test]
    fn pump_visits_the_console_first_and_throttles_exec_beside_it() {
        // Order and budget are the whole latency story: `transport.send`
        // blocks and `tick` doesn't call `recv` while it runs, so whoever
        // goes first — and how much it may ship — is what the person typing
        // in the console actually feels.
        assert_eq!(pump_order(), [ShellSlot::Pty, ShellSlot::Exec]);
        // Alone, exec streams at exactly the pre-split rate.
        assert_eq!(exec_pump_budget(None), PUMP_BUDGET);
        // Beside a console in use it yields.
        const { assert!(PUMP_BUDGET_EXEC_SHARED < PUMP_BUDGET) };
        assert_eq!(
            exec_pump_budget(Some(Duration::ZERO)),
            PUMP_BUDGET_EXEC_SHARED
        );
        let almost = PTY_QUIET_BEFORE_FULL_EXEC - Duration::from_millis(1);
        assert_eq!(exec_pump_budget(Some(almost)), PUMP_BUDGET_EXEC_SHARED);
        // Beside a console left alone it gets everything back — an open but
        // untouched console cost a 407 KB dump 61 s instead of 15 s.
        assert_eq!(
            exec_pump_budget(Some(PTY_QUIET_BEFORE_FULL_EXEC)),
            PUMP_BUDGET
        );
    }

    #[test]
    fn line_by_line_output_is_glued_into_full_packets_up_to_the_byte_budget() {
        // A shell printing lines hands over one line per read. Budgeted per
        // read, a tick shipped 16 lines — under a kilobyte — and 200 KB took
        // 31 s live against 2.5 s for the same bytes as one string.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        let events = session.exec_events_for_test();
        let mut line = vec![b'x'; 51];
        line.push(b'\n');
        for _ in 0..4000 {
            events.send(ShellEvent::Output(line.clone())).unwrap();
        }

        session.pump_shell_events().unwrap();

        let (mut packets, mut bytes) = (0, 0);
        while let Some(p) = client.recv_timeout(Duration::from_millis(20)) {
            if let Message::ShellOutput { data } = p.message {
                assert!(data.len() <= MAX_PAYLOAD, "packet of {} bytes", data.len());
                packets += 1;
                bytes += data.len();
            }
        }
        assert!(
            (PUMP_BUDGET..PUMP_BUDGET + line.len()).contains(&bytes),
            "one tick ships the byte budget in whole reads, got {bytes}"
        );
        assert_eq!(packets, bytes.div_ceil(MAX_PAYLOAD), "full packets only");
    }

    #[test]
    fn console_use_rearms_the_exec_throttle() {
        // Every kind of use has to restart the quiet window, or exec would
        // take the full budget in the middle of someone's typing. Compared
        // against a timestamp rather than a sleep, so a slow runner can't
        // make it flaky.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        assert_eq!(session.exec_budget_at(Instant::now()), PUMP_BUDGET);

        session.inject_pty_for_test(false, "");
        session.silence_pty_for_test();
        assert_eq!(
            session.exec_budget_at(Instant::now()),
            PUMP_BUDGET,
            "an idle console must not throttle exec"
        );

        let before = Instant::now();
        client
            .send(&Packet::new(
                Message::PtyInput {
                    data: b"x".to_vec(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(session.pty_last_activity >= before, "a keystroke is use");
        assert_eq!(session.exec_budget_at(before), PUMP_BUDGET_EXEC_SHARED);

        session.silence_pty_for_test();
        let before = Instant::now();
        client
            .send(&Packet::new(
                Message::PtyResize {
                    cols: 100,
                    rows: 30,
                },
                2,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(session.pty_last_activity >= before, "a resize is use");
    }

    #[test]
    fn either_open_slot_buys_the_busy_heartbeat_budget() {
        // An interactive `wd` is exactly the case where minutes of silence are
        // normal; the strict idle budget would tear the link down under it.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        assert_eq!(session.heartbeat_timeout(), HEARTBEAT_TIMEOUT_IDLE);

        session.inject_pty_for_test(false, "");
        assert_eq!(session.heartbeat_timeout(), HEARTBEAT_TIMEOUT_BUSY);

        session.pty_kill();
        assert_eq!(session.heartbeat_timeout(), HEARTBEAT_TIMEOUT_IDLE);

        client
            .send(&Packet::new(
                Message::ShellOpen {
                    shell: String::new(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert_eq!(session.heartbeat_timeout(), HEARTBEAT_TIMEOUT_BUSY);
    }

    #[test]
    fn every_teardown_path_clears_both_slots() {
        // Disconnect, re-handshake and heartbeat timeout all mean "the client
        // that owned these shells is gone". Leaving either behind would bounce
        // the next client's open off a slot nobody can reach.
        for teardown in ["disconnect", "re-hello", "heartbeat"] {
            let (mut session, mut client) = setup();
            connect(&mut session, &mut client);
            client
                .send(&Packet::new(
                    Message::ShellOpen {
                        shell: String::new(),
                    },
                    1,
                ))
                .unwrap();
            session.tick().unwrap();
            session.inject_pty_for_test(false, "");
            assert!(session.has_exec_shell() && session.has_pty_shell());

            match teardown {
                "disconnect" => {
                    client.send(&Packet::new(Message::Disconnect, 2)).unwrap();
                    session.tick().unwrap();
                }
                "re-hello" => {
                    client
                        .send(&Packet::new(
                            Message::Hello {
                                version: VERSION,
                                client_name: "test2".into(),
                            },
                            2,
                        ))
                        .unwrap();
                    session.tick().unwrap();
                }
                _ => {
                    session.force_heartbeat_timeout();
                    client.send(&Packet::new(Message::Heartbeat, 2)).unwrap();
                    session.tick().unwrap();
                }
            }

            assert!(!session.has_exec_shell(), "{teardown} left an exec shell");
            assert!(!session.has_pty_shell(), "{teardown} left a pty shell");
        }
    }

    /// `/bin/cat` echoes stdin to stdout and prints nothing of its own, so a
    /// test can assert on exactly the bytes it wrote — and it is unbuffered on
    /// macOS, so the echo comes back inside a tick rather than at exit.
    /// Windows has no equivalent one-word command, and a real PTY can't be
    /// opened on a Mac anyway (see `Backend::Pty`), so these live here.
    #[cfg(not(target_os = "windows"))]
    const ECHO_SHELL: &str = "/bin/cat";

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn the_two_slots_never_borrow_each_other_opcodes() {
        // The heart of the split: an exec command and an interactive console
        // running at the same time, each answering on its own opcodes. Mixing
        // them up would deliver `wd --exec` output into the owner's terminal
        // (or the owner's keystrokes into the agent's stdout).
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);

        session.inject_pty_for_test(false, ECHO_SHELL);
        client
            .send(&Packet::new(
                Message::ShellOpen {
                    shell: ECHO_SHELL.into(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(session.has_exec_shell() && session.has_pty_shell());

        client
            .send(&Packet::new(
                Message::ShellInput {
                    data: b"from-exec\n".to_vec(),
                },
                2,
            ))
            .unwrap();
        session.tick().unwrap();
        match pump_until(&mut session, &mut client, is_output) {
            Some(Message::ShellOutput { data }) => {
                assert_eq!(data, b"from-exec\n", "exec output on exec opcodes");
            }
            other => panic!("expected ShellOutput from the exec slot, got {other:?}"),
        }

        client
            .send(&Packet::new(
                Message::PtyInput {
                    data: b"from-pty\n".to_vec(),
                },
                3,
            ))
            .unwrap();
        session.tick().unwrap();
        // `tick` pumps before it reads, so the echo can't have been pumped
        // yet: whatever marks activity from here on is the output itself.
        session.silence_pty_for_test();
        let before = Instant::now();
        match pump_until(&mut session, &mut client, is_output) {
            Some(Message::PtyOutput { data }) => {
                assert_eq!(data, b"from-pty\n", "console output on pty opcodes");
            }
            other => panic!("expected PtyOutput from the pty slot, got {other:?}"),
        }
        assert!(
            session.pty_last_activity >= before,
            "console output keeps the exec throttle on"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_legacy_pty_answers_on_the_original_opcodes() {
        // The compatibility half: a client that opened with `ShellOpenPty`
        // knows nothing about `PtyInput`/`PtyOutput`, so its console has to
        // keep speaking `ShellInput`/`ShellOutput` in both directions.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        session.inject_pty_for_test(true, ECHO_SHELL);

        client
            .send(&Packet::new(
                Message::ShellInput {
                    data: b"legacy\n".to_vec(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        match pump_until(&mut session, &mut client, is_output) {
            Some(Message::ShellOutput { data }) => assert_eq!(data, b"legacy\n"),
            other => panic!("expected ShellOutput from a legacy pty, got {other:?}"),
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn input_meant_for_the_other_slot_is_dropped_not_delivered() {
        // Both directions of the mismatch. The dangerous one is `ShellInput`
        // reaching a dedicated pty: a stray frame from an older stream would
        // be typed straight into a live console. The other way round is
        // harmless but equally wrong, and both must end as a dropped frame
        // rather than a delivery.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        session.inject_pty_for_test(false, ECHO_SHELL);
        client
            .send(&Packet::new(
                Message::ShellInput {
                    data: b"stray\n".to_vec(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(
            pump_until(&mut session, &mut client, is_output).is_none(),
            "ShellInput must never reach a dedicated pty"
        );

        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        session.inject_pty_for_test(true, ECHO_SHELL);
        client
            .send(&Packet::new(
                Message::PtyInput {
                    data: b"stray\n".to_vec(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        assert!(
            pump_until(&mut session, &mut client, is_output).is_none(),
            "PtyInput must never reach a legacy pty"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn shell_input_still_reaches_a_legacy_pty_when_exec_is_empty() {
        // The fallback that keeps an old client working: with no exec shell
        // open, its `ShellInput` is meant for the console it opened with
        // `ShellOpenPty`.
        let (mut session, mut client) = setup();
        connect(&mut session, &mut client);
        session.inject_pty_for_test(true, ECHO_SHELL);
        assert!(!session.has_exec_shell());

        client
            .send(&Packet::new(
                Message::ShellInput {
                    data: b"typed\n".to_vec(),
                },
                1,
            ))
            .unwrap();
        session.tick().unwrap();
        match pump_until(&mut session, &mut client, is_output) {
            Some(Message::ShellOutput { data }) => assert_eq!(data, b"typed\n"),
            other => panic!("expected the legacy console to get it, got {other:?}"),
        }
    }

    #[test]
    fn storm_fires_after_threshold_consecutive_errors() {
        let (mut session, _client) = setup();
        // threshold-1 reports → no storm yet
        for _ in 0..DEFAULT_STORM_THRESHOLD - 1 {
            assert!(!session.note_protocol_error());
        }
        assert_eq!(session.storm_count(), DEFAULT_STORM_THRESHOLD - 1);
        // threshold-th report → storm
        assert!(session.note_protocol_error());
        assert_eq!(session.storm_count(), DEFAULT_STORM_THRESHOLD);
    }

    #[test]
    fn storm_resets_on_valid_packet_via_tick() {
        let (mut session, mut client) = setup();
        // Handshake so subsequent packets are processed in Connected state.
        client
            .send(&Packet::new(
                Message::Hello {
                    version: 1,
                    client_name: "test".into(),
                },
                0,
            ))
            .unwrap();
        session.tick().unwrap();
        let _ack = client.recv().unwrap();
        assert_eq!(session.storm_count(), 0, "handshake decode already reset");

        // Accumulate a partial storm run.
        for _ in 0..5 {
            assert!(!session.note_protocol_error());
        }
        assert_eq!(session.storm_count(), 5);

        // A real decoded packet (heartbeat) through tick() must reset the run.
        client.send(&Packet::new(Message::Heartbeat, 1)).unwrap();
        session.tick().unwrap();
        assert_eq!(
            session.storm_count(),
            0,
            "decoded packet must reset storm run"
        );
    }

    #[test]
    fn storm_count_persists_without_valid_packet() {
        // The storm run is reset ONLY by a decoded packet (via tick's
        // on_valid_packet call). Nothing else — including a heartbeat-timeout
        // or recv-timeout, which both return Ok(false) without decoding —
        // touches it. MockTransport::recv() blocks instead of timing out, so
        // we can't drive an empty tick here without hanging; the invariant
        // we assert is that the counter only moves via note_protocol_error /
        // on_valid_packet and never self-resets between error reports.
        let (mut session, _client) = setup();
        for _ in 0..3 {
            session.note_protocol_error();
        }
        assert_eq!(session.storm_count(), 3);
        // More errors with no interleaved valid packet keep climbing.
        for _ in 0..2 {
            session.note_protocol_error();
        }
        assert_eq!(
            session.storm_count(),
            5,
            "run must persist without a decoded packet"
        );
    }
}
