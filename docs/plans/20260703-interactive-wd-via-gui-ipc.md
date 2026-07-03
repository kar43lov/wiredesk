# Interactive `wd` through GUI IPC — PTY stream parallel with open GUI

## Overview

Make interactive `wd` (PowerShell PTY in Ghostty/iTerm) work while `WireDesk.app` is
running — **including during active mouse/keyboard capture** — without quitting the GUI
and without contending for the serial port.

Today the interactive `wd` path is the last one that directly, exclusively opens the
serial port (`SerialTransport::open` in `apps/wiredesk-term/src/main.rs:192`), so it is
mutually exclusive with a running GUI. `wd --exec` already solved this via the embedded
Unix-socket IPC (`wd-exec.sock`): GUI holds the port, `wd --exec` connects to the socket,
falls back to direct serial when the socket is absent. This plan extends that same IPC
bridge to a **bidirectional PTY stream** so interactive `wd` routes through the GUI too.

**Benefits:** removes the last "quit the GUI to use `wd`" friction; the serial-terminal
`single-port ownership` limitation stops applying to WireDesk entirely.

**Key enabling fact (from research 2026-07-03):** on the Windows host, input injection and
shell are *orthogonal arms of the same tick-loop* (`session.rs` `handle_packet` match:
`Mouse*`/`Key*` touch only `injector`, `Shell*` touch only `self.shell`). A PTY shell and
active capture already coexist by construction. **The host is NOT modified.** All work is on
the Mac side (term + GUI client).

## Context (from discovery)

- **Files/components involved:**
  - `apps/wiredesk-term/src/main.rs` — `run()` (branch at `:230`), `bridge_loop` (`:611`,
    already transport-agnostic over `Arc<Mutex<Box<dyn Transport>>>` + `Box<dyn Transport>`),
    `try_socket_first` (`:317`, the fallback pattern to mirror), shared serial open (`:192`).
  - `crates/wiredesk-exec-core/src/ipc.rs` — length-prefixed bincode framing
    (`write_frame`/`read_frame`), `IpcRequest`/`IpcResponse`, `default_socket_path()`.
  - `apps/wiredesk-client/src/ipc.rs` — existing exec acceptor + per-connection handler,
    `single_inflight`, `link_up` gating.
  - `apps/wiredesk-client/src/exec_bridge.rs` — `ExecEventSlot`/`ExecSlotGuard` (single-consumer
    shell-event slot), `broadcast_exec_event`.
  - `apps/wiredesk-client/src/link.rs` — reader-thread shell fan-out (`events_tx` + `exec_slot`),
    `outgoing_tx` survival across reconnect, `link_up: Arc<AtomicBool>` (true only on HelloAck).
  - `apps/wiredesk-client/src/app.rs` — dead GUI shell-panel to remove (`:98-101` state,
    `:1805-1875` UI, `shell_*` handlers, `TransportEvent::Shell*` consumption `:1505-1517`).
  - `crates/wiredesk-transport/src/transport.rs` — `Transport` trait (5 methods).
- **Related patterns found:**
  - `try_socket_first` → `Ok(None)` fall-through on `ENOENT`/`ECONNREFUSED`/first-frame-timeout
    is the exact fallback shape to mirror for the interactive path.
  - `ExecSlotGuard::install` RAII + `broadcast_exec_event` no-op-when-empty is reusable as-is
    for routing `ShellOutput`/`ShellExit`/`HostError` back to the interactive relay.
  - `bridge_loop` runs unchanged over any `Transport` impl — the interactive reroute is "swap
    the transport", not "rewrite the loop".
- **Dependencies identified:** bincode (already a dep of exec-core), `std::os::unix::net`,
  `wiredesk-protocol` `Packet` wire codec (encode/decode), `wiredesk-transport::Transport`.

## Development Approach

- **testing approach**: Regular (code first, then tests) — matches the existing crates'
  style; every task still ends with tests as a required deliverable.
- complete each task fully before moving to the next; small, focused changes.
- **CRITICAL: every task MUST include new/updated tests** — success + error/edge scenarios.
- **CRITICAL: all tests must pass before starting next task.** Run
  `cargo test --workspace -- --test-threads=1` (host flakes ~50% SIGABRT on the parallel
  macOS runner — pre-existing baseline; single-thread is mandatory for reliable runs).
- **CRITICAL: `cargo clippy --workspace -- -D warnings` must be clean** (dead-code from the
  panel removal will fail the build otherwise).
- maintain backward compatibility — the `wd --exec` path and the GUI-closed direct-serial
  interactive path must not regress (AC3 is the hard gate).
- Mac-only additions are `#[cfg(target_os = "macos")]`, mirroring the existing IPC.

## Testing Strategy

- **unit tests**: required every task (see Development Approach). Prefer `UnixStream::pair()`
  for socket round-trips and mock `mpsc` channels for relay/owner-lock logic — same style as
  the existing `ipc.rs` / `exec_bridge.rs` tests.
- **integration tests**: a fake-GUI (binds socket + runs the relay against mock
  `outgoing_tx`/`exec_slot`) exercised by an in-process interactive client, covering
  handshake → ShellOpenPty → ShellInput echo → ShellOutput → Ctrl+] teardown.
- **e2e/UI tests**: project has no automated UI e2e harness (egui app, manual live test).
  UI change (panel removal) is covered by compile + clippy + a manual smoke in Post-Completion.

## Progress Tracking

- mark completed items with `[x]` immediately when done.
- add newly discovered tasks with ➕ prefix; blockers with ⚠️ prefix.
- keep this plan in sync with actual work; update on scope change.

## Solution Overview

**Approach A — `Packet` relay over the Unix socket.** The interactive term already speaks the
exact `Message`/`Packet` wire protocol (`ShellOpenPty`/`ShellInput`/`PtyResize`/`ShellOutput`/
`ShellExit`/`Heartbeat`). Instead of inventing a new streaming IPC enum, the socket transparently
carries those `Packet`s:

- **term side:** a new `IpcStreamTransport: Transport` over `UnixStream`. `send(&Packet)`
  encodes the packet (existing wire codec) into a length-prefixed socket frame; `recv()` reads
  a frame and decodes a `Packet`. `try_clone()` clones the `UnixStream` fd (reader/writer split,
  mirroring `SerialTransport::try_clone`). `bridge_loop` runs over it **byte-for-byte unchanged**.
- **GUI side:** a new per-connection *streaming relay* handler. It reads `Packet`s from the
  socket and forwards shell packets into the existing `outgoing_tx` (→ serial writer → host);
  it installs the `exec_slot` to receive `ShellOutput`/`ShellExit`/`HostError` from the reader
  fan-out and writes them back to the socket as `Packet`s. Special cases: a `Hello` from term is
  answered with a **synthesized `HelloAck`** from the GUI's cached session state (host dims;
  never forwarded to the wire — the GUI already handshook); a relayed `Heartbeat` is **dropped**
  (the GUI writer owns heartbeat).
- **connection dispatch:** the first socket frame is an `IpcConnect` discriminator —
  `Exec(IpcRequest)` (existing one-shot path, now wrapped) or `Interactive(IpcInteractiveOpen
  { shell, cols, rows })` (new streaming path). The acceptor routes to the right handler.
- **fallback:** term tries the socket first (`try_interactive_socket`, mirroring
  `try_socket_first`); on `ENOENT`/`ECONNREFUSED`/first-frame-timeout it falls through to the
  current direct `SerialTransport::open`. GUI closed ⇒ behaviour identical to today.

**Key design decisions & rationale:**
- **Reuse the wire codec, not a new enum (YAGNI):** interactive frames are literally the same
  `Packet`s that go on the serial wire; a parallel IPC enum would just re-encode them. The socket
  is a reliable byte-stream, so we use the existing length-prefix framing (not COBS+CRC).
- **Fail-fast single-owner lock:** the host has exactly one shell slot; a second `ShellOpen*`
  is rejected with `Error "shell already open"`. We front-run that with a client-side
  `ShellChannelOwner` state (`Idle`/`ExecBusy`/`InteractiveBusy`). Interactive claims exclusively;
  a competing acquirer gets an immediate "shell busy" terminal frame → term exit 125. No queuing:
  interactive sessions live for minutes, so queuing `--exec` behind one would hang Claude.
- **Delete the dead GUI shell-panel:** it contends for the same host slot and is unused; removing
  it simplifies the owner model and cuts net diff.
- **GUI owns heartbeat:** term-over-IPC suppresses its own heartbeat; the GUI writer already
  emits one every 2 s on the real wire. Prevents double heartbeats.

## Technical Details

- **`IpcConnect` enum** (`crates/wiredesk-exec-core/src/ipc.rs`), length-prefixed bincode, sent
  as the **first** frame of every connection:
  ```rust
  pub enum IpcConnect {
      Exec(IpcRequest),
      Interactive(IpcInteractiveOpen),
  }
  pub struct IpcInteractiveOpen { pub shell: String, pub cols: u16, pub rows: u16 }
  ```
  ⚠ **Atomic cutover (plan-review Critical #1):** three sites read/write the first frame and MUST
  change together or the shipped `wd --exec` path breaks: (a) term exec client `try_socket_first`
  (`main.rs:354`, currently writes a **bare** `IpcRequest`), (b) GUI acceptor (`ipc.rs:160`,
  currently `read_request`), (c) exec-handler decode. All three land in **one task** (Task 7).
  Lock-step rebuild (GUI + `wd` from one workspace) means no version-skew window.
- **Interactive frame codec:** after the `IpcConnect::Interactive` frame, both directions carry
  `Packet`s. Add **free functions** `write_packet_frame`/`read_packet_frame` in `ipc.rs` (match the
  existing `write_frame`/`read_frame` style — no struct) reusing `write_frame`/`read_frame` around
  the `Packet` wire encode/decode. Reject frames > `MAX_FRAME_BYTES` (existing cap).
- **`IpcStreamTransport`** (`apps/wiredesk-term/src/`, new module `ipc_transport.rs`):
  holds a `UnixStream`; `send`/`recv` via the packet-frame helpers; `try_clone` via
  `UnixStream::try_clone`; `is_connected` = last IO ok; `name` = `"ipc-stream"`.
  Two invariants that keep `bridge_loop` **byte-for-byte unchanged** (plan-review Important #3):
  - **`recv` MUST use a read timeout.** `bridge_loop`'s reader checks `stop` only between `recv()`
    calls and depends on a periodic timeout error (`SerialTransport::recv` returns
    `Transport("recv timeout")`, filtered by `m.contains("timeout")` at `main.rs:784`). A blocking
    `UnixStream::recv` would hang the reader `join()` on Ctrl+] until the GUI closes the socket.
    So set `set_read_timeout(Some(~100ms))` and map `WouldBlock`/`TimedOut` →
    `Err(WireDeskError::Transport("recv timeout".into()))` — same shape the loop already tolerates.
  - **`send` drops `Message::Heartbeat`** (returns `Ok(())` without writing). The GUI writer already
    heartbeats the real wire every 2 s; this suppresses the double heartbeat at the transport,
    with zero change to `bridge_loop` (no "skip the heartbeat thread" signature hack).
- **`ShellChannelOwner`** (`apps/wiredesk-client/src/shell_channel.rs`):
  `Arc<Mutex<ShellOwner>>` where `enum ShellOwner { Idle, Exec, Interactive }`, plus a RAII
  `ShellChannelGuard` that resets to `Idle` on drop. `try_acquire(kind) -> Option<Guard>` returns
  `None` (busy) unless `Idle`. **Cross-kind is fail-fast; exec-vs-exec keeps FIFO** (plan-review
  Important #5 + brief): the existing `single_inflight: Arc<Mutex<()>>` is **retained and nested
  under the `Exec` owner-state** — the exec handler first `try_acquire(Exec)` (fail-fast only if an
  `Interactive` session holds the channel), then blocks on `single_inflight` to serialise
  exec-vs-exec FIFO as today. Interactive uses only the owner (no queue — a minutes-long session
  must never queue Claude's `--exec`).
- **Shared host-info cache** (plan-review Important #1): `link.rs` currently forwards
  host_name/screen_w/screen_h from `HelloAck` (`link.rs:497-513`) to `app.rs` via `events_tx` and
  keeps no shared copy; the acceptor thread has no access to `App`. Add
  `SharedHostInfo = Arc<Mutex<Option<HostInfo>>>` (`HostInfo { host_name, screen_w, screen_h }`),
  populated at the reader's `HelloAck` arm (via `LinkContext`), cleared when `link_up` goes false,
  and cloned into `spawn_ipc_acceptor`. The interactive relay reads it to synth an accurate
  `HelloAck`.
- **GUI streaming relay** (`apps/wiredesk-client/src/ipc.rs`, new `handle_interactive_connection`):
  `try_acquire(Interactive)` (busy → write a terminal "shell busy" frame + close), refuse if
  `link_up == false` OR host-info cache is empty (not yet handshook → AC6), install `ExecSlotGuard`,
  **originate the single `ShellOpenPty { shell, cols, rows }`** to `outgoing_tx` (the term does NOT
  send its own — see Task 8), then run two pumps until socket EOF / `ShellExit` / term `ShellClose`:
  - socket → wire: read `Packet`; `Hello` → reply synth `HelloAck` from the host-info cache (NOT
    forwarded to the wire); `Heartbeat` → drop; `ShellInput`/`PtyResize`/`ShellClose`/`Disconnect`
    → `outgoing_tx`.
  - slot → socket: `ExecEvent::ShellOutput`/`ShellExit`/`HostError` → `Packet` → socket frame.
    **Also polls `link_up` each cycle** (plan-review Important #4 / AC6): on `false` mid-session,
    write a synth `Disconnect` frame and close the socket so the term's reader sees EOF and exits
    cleanly instead of hanging.
  On teardown: send `ShellClose` to host, drop guard (owner → Idle), close socket.

## What Goes Where

- **Implementation Steps** (`[ ]`): all code + tests + in-repo docs below.
- **Post-Completion** (no checkboxes): live manual verification on real GUI + Ghostty + Win11 host.

## Implementation Steps

> Task order avoids forward dependencies: protocol/codec (1–2) → term transport (3) → client
> owner-lock (4) + host-info cache (5) → interactive relay function (6, `#[allow(dead_code)]`
> until wired) → **atomic `IpcConnect` cutover wiring both handlers + exec client** (7) → term
> interactive client (8) → e2e (9) → panel removal (10) → verify (11) → docs (12).

### Task 1: `IpcConnect` dispatch frame + interactive open payload

**Files:**
- Modify: `crates/wiredesk-exec-core/src/ipc.rs`

- [x] add `IpcInteractiveOpen { shell: String, cols: u16, rows: u16 }` (Serialize/Deserialize).
- [x] add `enum IpcConnect { Exec(IpcRequest), Interactive(IpcInteractiveOpen) }` + framed
      `write_connect`/`read_connect` free functions reusing `write_frame`/`read_frame`.
- [x] write tests: `IpcConnect::Exec` and `::Interactive` round-trip via `UnixStream::pair()`
      and `Cursor`; oversize-length rejection still applies.
- [x] write tests: decoding a legacy bare-`IpcRequest` frame as `IpcConnect` fails cleanly
      (documents the lock-step contract, mirrors `ipc_request_old_payload_compatibility`).
- [x] run tests - must pass before next task.

### Task 2: Interactive `Packet` frame codec over the socket

**Files:**
- Modify: `crates/wiredesk-exec-core/src/ipc.rs`
- Modify: `crates/wiredesk-exec-core/Cargo.toml` (add `wiredesk-protocol` dep if absent)

- [x] add **free functions** `write_packet_frame`/`read_packet_frame` wrapping the
      `wiredesk-protocol` `Packet` wire encode/decode in the length-prefix framing; enforce
      `MAX_FRAME_BYTES`. (Match existing `write_frame`/`read_frame` style — no struct.)
- [x] write tests: round-trip every interactive `Message` type used
      (`Hello`/`HelloAck`/`ShellOpenPty`/`ShellInput`/`PtyResize`/`ShellOutput`/`ShellExit`/
      `ShellClose`/`Disconnect`/`Heartbeat`) through `UnixStream::pair()`.
- [x] write tests: truncated/oversize frame → `Err`, not panic.
- [x] run tests - must pass before next task.

### Task 3: `IpcStreamTransport: Transport` in `wiredesk-term`

**Files:**
- Create: `apps/wiredesk-term/src/ipc_transport.rs`
- Modify: `apps/wiredesk-term/src/main.rs` (module decl)
- Modify: `apps/wiredesk-term/Cargo.toml` (ensure `wiredesk-exec-core`, `wiredesk-protocol`,
  `wiredesk-transport` deps available)

- [x] implement `IpcStreamTransport` over `UnixStream`: `send`/`recv` via the Task 2 codec,
      `try_clone` via `UnixStream::try_clone`, `is_connected`, `name = "ipc-stream"`.
- [x] **`recv` sets `set_read_timeout(Some(~100ms))`** and maps `WouldBlock`/`TimedOut` →
      `Err(WireDeskError::Transport("recv timeout".into()))` so `bridge_loop`'s reader wakes to
      check `stop` (Ctrl+] exits cleanly — plan-review Important #3).
- [x] **`send` drops `Message::Heartbeat`** (returns `Ok(())` without writing) — GUI owns
      heartbeat; keeps `bridge_loop` unchanged, no double heartbeat.
- [x] add `connect_at(socket_path) -> Result<Option<Self>>` returning `Ok(None)` on
      `ENOENT`/`ECONNREFUSED` (path-parameterized for testability — plan-review Minor #2).
- [x] write tests: `send(Packet)` on one paired stream == `recv()` on the other (all shell types);
      `send(Heartbeat)` writes nothing to the peer.
- [x] write tests: `recv` on an idle socket returns the "recv timeout" `Err` within ~timeout
      (not a hang); `try_clone` yields an independent-decoder handle.
- [x] write tests: `connect_at` to a nonexistent path → `Ok(None)` (fallback signal).
- [x] run tests - must pass before next task.

### Task 4: `ShellChannelOwner` — cross-kind fail-fast, exec-vs-exec FIFO retained

**Files:**
- Create: `apps/wiredesk-client/src/shell_channel.rs`
- Modify: `apps/wiredesk-client/src/main.rs` (module decl + construct the shared owner)

- [ ] implement `enum ShellOwner { Idle, Exec, Interactive }`, `type SharedShellOwner =
      Arc<Mutex<ShellOwner>>`, `try_acquire(&SharedShellOwner, kind) -> Option<ShellChannelGuard>`
      (returns `None` when not `Idle`), RAII `ShellChannelGuard` resetting to `Idle` on drop.
- [ ] document the composition: exec handler `try_acquire(Exec)` (cross-kind fail-fast) THEN the
      retained `single_inflight` mutex for exec-vs-exec FIFO; interactive `try_acquire(Interactive)`
      only (no queue). (Actual wiring in Task 7.)
- [ ] write tests: `Idle`→`Interactive` ok; second acquire (`Exec` or `Interactive`) → `None`;
      drop guard → next acquire ok.
- [ ] write tests: guard resets to `Idle` even on holder-thread panic (mirror
      `panic_in_holder_thread_still_releases_slot`).
- [ ] run tests - must pass before next task.

### Task 5: Shared host-info cache for synth `HelloAck`

**Files:**
- Modify: `apps/wiredesk-client/src/link.rs` (populate at `HelloAck` arm; clear on link-down)
- Modify: `apps/wiredesk-client/src/main.rs` (construct `SharedHostInfo`, thread into `LinkContext`)

- [ ] add `HostInfo { host_name: String, screen_w: u32, screen_h: u32 }` and
      `SharedHostInfo = Arc<Mutex<Option<HostInfo>>>`; add the field to `LinkContext`.
- [ ] populate it in the reader's `HelloAck` arm (`link.rs:497-513`) alongside the existing
      `link_up=true`; set it back to `None` on every link-down (where `link_up` goes false).
- [ ] write tests: reader loop / helper stores `HostInfo` on `HelloAck`; clears on disconnect.
- [ ] run tests - must pass before next task.

### Task 6: GUI interactive relay handler (standalone, not yet dispatched)

**Files:**
- Modify: `apps/wiredesk-client/src/ipc.rs`

- [ ] implement `#[allow(dead_code)] fn handle_interactive_connection(stream, outgoing_tx,
      exec_slot, shell_owner, host_info, link_up)`:
      `try_acquire(Interactive)` (busy → write terminal "shell busy" frame + close); refuse if
      `link_up == false` OR `host_info` empty; install `ExecSlotGuard`; originate the single
      `ShellOpenPty { shell, cols, rows }` to `outgoing_tx`.
- [ ] implement the two pumps: socket→wire (`Hello`→synth `HelloAck` from `host_info`, NOT
      forwarded; `Heartbeat`→drop; `ShellInput`/`PtyResize`/`ShellClose`/`Disconnect`→`outgoing_tx`)
      and slot→socket (`ShellOutput`/`ShellExit`/`HostError`→`Packet`→socket). Poll `link_up` each
      cycle; on `false` write synth `Disconnect` + close socket (AC6). Teardown: `ShellClose`,
      drop guard, close.
- [ ] write tests (call the fn directly with a `UnixStream::pair` + mock `outgoing_tx`/`exec_slot`):
      `Hello`→synth `HelloAck` and NOT forwarded to wire; `Heartbeat` dropped; `ShellInput`/
      `PtyResize` forwarded; staged `ShellOutput`/`ShellExit` reach the socket.
- [ ] write tests: `link_up == false` OR empty `host_info` → refused, no packet queued
      (mirror `handler_link_down_returns_transport_unavailable`).
- [ ] write tests: owner already `Interactive`/`Exec` → "shell busy", no `ShellOpenPty` queued.
- [ ] write tests: mid-session `link_up`→false → synth `Disconnect` written, socket closed (AC6).
- [ ] run tests - must pass before next task.

### Task 7: Atomic `IpcConnect` cutover — acceptor dispatch + exec handler + exec client together

**Files:**
- Modify: `apps/wiredesk-client/src/ipc.rs` (acceptor + exec handler + `spawn_ipc_acceptor` sig)
- Modify: `apps/wiredesk-client/src/main.rs` (callsite: pass `SharedShellOwner` + `SharedHostInfo`)
- Modify: `apps/wiredesk-term/src/main.rs` (`try_socket_first` sends `IpcConnect::Exec`)

- [ ] acceptor: read `IpcConnect` first; dispatch `Exec`→existing handler, `Interactive`→Task 6
      handler (remove its `#[allow(dead_code)]`).
- [ ] exec handler: replace `single_inflight`-first with `try_acquire(Exec)` (busy-by-Interactive →
      `IpcResponse::TransportUnavailable("shell busy")` → term exit 125), THEN keep `single_inflight`
      for exec-vs-exec FIFO (unchanged behaviour).
- [ ] `spawn_ipc_acceptor` signature + `main.rs` callsite: pass `SharedShellOwner` + `SharedHostInfo`
      (keep `single_inflight` as before, now nested under `Exec`).
- [ ] `apps/wiredesk-term/src/main.rs`: `try_socket_first` writes `IpcConnect::Exec(req)` via
      `write_connect` instead of bare `write_request` (`main.rs:354`). **This is the edit the
      cutover cannot omit** (plan-review Critical #1).
- [ ] write/​update tests: exec regression — `handler_round_trip_via_unix_socket` updated to send
      `IpcConnect::Exec` and still passes end-to-end; interactive-owner-held → exec gets 125-class
      frame, no `ShellOpen` queued; two concurrent exec still FIFO-serialise (no false "busy").
- [ ] run tests - must pass before next task.

### Task 8: term `try_interactive_socket` + serial fallback

**Files:**
- Modify: `apps/wiredesk-term/src/main.rs`

- [ ] add `#[cfg(target_os = "macos")] fn try_interactive_socket_at(path, args, …) ->
      Result<Option<i32>>` (path-parameterized): `IpcStreamTransport::connect_at` → `Ok(None)` on
      no socket; send `IpcConnect::Interactive { shell, cols, rows }` (cols/rows from
      `terminal::size()`); perform the `Hello`/`HelloAck` handshake **over the socket**; build the
      `IpcStreamTransport` writer + `try_clone` reader and run the existing `bridge_loop`.
- [ ] **do NOT send the `run()` `ShellOpenPty` block (`main.rs:221-225`) on the IPC path** — the
      GUI relay originates the single `ShellOpenPty` (plan-review Important #2). Guard that block so
      it runs only on the direct-serial path.
- [ ] call `try_interactive_socket` in `run()` on the interactive branch **before**
      `SerialTransport::open` (`main.rs:192`); `Ok(None)` falls through to direct-serial unchanged.
- [ ] print the source in the startup banner ("interactive via GUI IPC" vs "interactive via direct
      serial"), consistent with the resolve banner style.
- [ ] write tests: `try_interactive_socket_at` with no socket present → `Ok(None)` (fallback).
- [ ] run tests - must pass before next task.

### Task 9: End-to-end interactive round-trip integration test

**Files:**
- Create: `apps/wiredesk-client/tests/interactive_ipc.rs` (or a `#[cfg(test)]` integ module)

- [ ] fake-GUI: bind a temp socket, run the interactive relay against mock `outgoing_tx`
      (capturing forwarded packets) + a drivable `exec_slot` + a populated `host_info`.
- [ ] in-process client: connect via `IpcStreamTransport`, drive
      `IpcConnect::Interactive` → `Hello`/synth-`HelloAck` → staged `ShellOutput` echo → `ShellExit`.
- [ ] assert: synth `HelloAck` received; the single `ShellOpenPty` is originated by the relay;
      forwarded `ShellInput`/`PtyResize` match; teardown sends `ShellClose`; owner returns to `Idle`.
- [ ] assert: a second concurrent interactive connect during the first → "shell busy".
- [ ] run tests - must pass before next task.

### Task 10: Remove the dead GUI shell-panel

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs`
- Modify: `apps/wiredesk-client/src/link.rs` (only if the shell `events_tx` fan-out becomes
  fully unused after panel removal)

- [ ] remove shell-panel state (`shell_open`/`shell_output`/`shell_input`/`shell_kind`),
      the UI panel block, and the `shell_open_request`/`shell_send_input`/`shell_close_request`
      handlers.
- [ ] remove the `TransportEvent::ShellOutput/ShellExit/ShellError` consumption in `app.rs`;
      if that leaves the reader's `events_tx` shell arms with no consumer, drop those arms
      (keep `exec_slot` fan-out — the interactive/exec path needs it). Document what stays.
- [ ] update any tests referencing the removed panel; add/adjust a test asserting the app
      builds its update loop without shell-panel state.
- [ ] `cargo clippy --workspace -- -D warnings` clean (no dead-code/unused).
- [ ] run tests - must pass before next task.

### Task 11: Verify acceptance criteria

- [ ] AC1–AC7 walk-through against the brief; confirm each has a covering test or a documented
      manual step (AC1/AC5 are live-only — note in Post-Completion).
- [ ] verify edge cases: GUI mid-reconnect during session (AC6), fallback with GUI closed (AC3),
      concurrent exec-vs-interactive both directions (AC2), exec-vs-exec still FIFO.
- [ ] run full suite: `cargo test --workspace -- --test-threads=1`.
- [ ] `cargo clippy --workspace -- -D warnings` and `cargo build --release --workspace` clean.
- [ ] verify test coverage: new modules (ipc codec, IpcStreamTransport, shell_channel, host-info
      cache, relay) each have success + error tests.

### Task 12: Update documentation

**Files:**
- Modify: `CLAUDE.md`, `README.md`
- Modify: `docs/briefs/interactive-wd-via-gui-ipc.md` (mark SHIPPED after live test)
- Modify: `docs/briefs/wd-exec-via-gui-ipc.md`, `docs/briefs/daemon-multiplex.md` (cross-link)

- [ ] CLAUDE.md: document interactive-`wd`-over-IPC (parallel with GUI + capture), the
      fail-fast shell-owner policy, GUI-panel removal, updated test count.
- [ ] README.md: matching feature note + updated crate/test counts.
- [ ] update memory pointers (`feedback_wd_interactive_session.md`,
      `feedback_serial_terminal_bridge.md` — single-port ownership no longer applies to WireDesk).
- [ ] move this plan to `docs/plans/completed/` (create dir if needed).

## Post-Completion
*Manual / external — no checkboxes.*

**Manual live verification (requires real GUI + Ghostty + Win11 host):**
- AC1: GUI running, capture active, fullscreen → `wd` in Ghostty connects via IPC, PowerShell PTY,
  arrows/Tab/vim work, Ctrl+] exits cleanly, capture mouse/keyboard not disturbed.
- AC2: interactive `wd` active → Claude fires `wd --exec "echo ok"` → immediate "shell busy"
  (exit 125), clear stderr; and the reverse (exec in flight → interactive fail-fast).
- AC5: resize the Ghostty window mid-session → PowerShell repaints (PtyResize reaches host).
- AC6: quit GUI mid-session and vice-versa → graceful disconnect, no hang.
- AC3: quit GUI, run `wd` → direct-serial interactive identical to today.

**Notes:**
- Host (Windows) binary is unchanged; no host redeploy needed for this work.
- Live tests should run on the FT232H @ 3 Mbaud solo setup (real config), not the hardcoded
  fallbacks.
