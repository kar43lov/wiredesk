# Plan: ConPTY for interactive `wd` (host-side TTY)

Бриф: `docs/briefs/host-conpty.md` (одобрен).
Ветка: `feat/host-pty` (создана от master).

## Overview

В interactive `wiredesk-term` (`wd` без `--exec`) host'овский shell живёт в настоящем TTY. Это даёт vim/htop/ssh без `-tt`/PSReadLine prompt с history+Tab autocomplete. Текущий `Stdio::piped()` flow остаётся для `wd --exec` (PR #9 — pipe-based sentinel detection) и GUI shell-panel (egui без ANSI parser'а).

Per-session toggle через **новый opcode** `ShellOpenPty = 0x45`: старый `ShellOpen = 0x40` (pipe-mode) остаётся бинарно совместимым без изменений payload-формата. Resize'ом управляет новый `PtyResize = 0x46 { cols: u16, rows: u16 }`.

## Context (from discovery)

- Workspace Rust crates: `wiredesk-protocol` (бинарный protocol, не serde — manual `serialize`/`deserialize` в `crates/wiredesk-protocol/src/message.rs`), `wiredesk-transport`, host (`apps/wiredesk-host`), client GUI (`apps/wiredesk-client`), terminal CLI (`apps/wiredesk-term`).
- Существующие message-коды: `ShellOpen = 0x40` … `ShellExit = 0x44`. Свободны 0x45+, далее идут только 5 input + 5 shell + 4 clipboard, 0x45/0x46 вписываются в shell-блок естественно.
- Текущий host'овский shell flow: `apps/wiredesk-host/src/shell.rs::ShellProcess::spawn(requested: &str)` → `Command::new` + `Stdio::piped()` для stdin/stdout/stderr, два reader-thread'а (stdout/stderr → mpsc<ShellEvent>), один writer-thread (mpsc<ShellInput> → stdin).
- Term'овский `bridge_loop` (`apps/wiredesk-term/src/main.rs:766+`): cooked-mode line discipline — `line_buf: Vec<u8>`, `line_cells: usize`, BS-erase через `pop_utf8_char`, `\n`→`\r\n` через `translate_output_for_terminal`. На TTY всё это надо отключить — host echo'ит сам.
- `run_oneshot` (`apps/wiredesk-term/src/main.rs:393`) — pipe-based, sentinel detection с UUID. Шлёт обычный `ShellOpen { shell }` (не PTY). Trogamus *не трогаем*.
- GUI shell-panel в `apps/wiredesk-client/src/main.rs` — egui ScrollArea + label, без ANSI-parser'а. Шлёт обычный `ShellOpen` (pipe-mode). Не трогаем.
- `portable-pty 0.9.0` (Wez Furlong / wezterm) — кросс-платформенный: native ConPTY на Windows (Win10 1809+ → Win11 ОК), native forkpty на Unix → unit tests на Mac dev-cycle компилятся.

## Development Approach

- **testing approach**: Regular (test после кода, не TDD)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
  - tests are not optional — they are a required part of the checklist
  - write unit tests for new/modified functions/methods
  - tests cover both success and error scenarios
- **CRITICAL: all tests must pass before starting next task** — no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- run tests after each change
- maintain backward compatibility — **bilateral deployment**: старый `ShellOpen = 0x40` бинарно identicalен и продолжит работать, поэтому old client + new host работают (host видит pipe-request → existing flow). Обратное направление (new client + old host) **не поддерживается** — old host вернёт `Error("unknown message type: 0x45")` через существующий `MessageType::try_from`. Fallback на 0x40 в client'е **не делаем** (deployment предполагается bilateral, fallback усложняет код без пользы для нашего solo-сетапа).

## Testing Strategy

- **unit tests**: required for every task (см. Development Approach)
- **e2e тесты**: project не имеет UI-based e2e (Playwright/Cypress нет). Live-проверка на Win11 — финальный manual verification (Task 7).
- тесты живут в одной директории с кодом через `#[cfg(test)] mod tests` — паттерн всего workspace.

## Progress Tracking

- mark completed items с `[x]` сразу как сделано
- add newly discovered tasks с `➕` префиксом
- document blockers с `⚠️` префиксом
- update plan если scope меняется по ходу реализации

## Solution Overview

- **Diverges from brief** (`docs/briefs/host-conpty.md`): бриф упоминал `Message::ShellOpen { shell, pty: bool }` с serde-default'ом. План вместо этого делает **новый opcode** `ShellOpenPty = 0x45`. Причина — protocol hand-coded (manual `serialize`/`deserialize` в `message.rs`, не serde): расширение payload'а 0x40 байтом флага молча ломало бы парсеров старых hosts'ов (читали бы 1-й байт флага как первый символ shell-name'а). Opcode-discriminator делает изменение wire-binary additive.
- **Per-session pipe-vs-pty выбор через opcode-discriminator.** Чище чем флаг в payload'е. Новые два opcode'а — изолированы, старые message-types байтово identicalными.
- **Host: ConPTY-ветка в `ShellProcess::spawn`** через `portable-pty::native_pty_system().openpty(PtySize{...})`. Reader/writer берутся через `master.try_clone_reader()` / `master.take_writer()`, дальше тот же mpsc-pattern что у pipe-flow. Pipe-flow остаётся буквально без изменений — две ветки в одном `spawn` через `if pty { ... } else { ... }`.
- **Host: `ShellProcess::resize(cols, rows)`** — `master.resize(PtySize{...})` при PTY, no-op при pipe. Хранит `Option<Box<dyn MasterPty>>` или enum `Backend::{Pty(MasterPty), Pipe}` для maintenance ясности.
- **Term: `bridge_loop` шлёт `ShellOpenPty` с initial cols/rows из `crossterm::terminal::size()`.** Cooked-mode discipline удалить — на TTY-side host echoes сам, double-echo иначе. Stdin байты forward'ятся в host без накопления (raw pass-through).
- **Term: `bridge_loop` слушает SIGWINCH (Unix) / periodic poll** для resize-events → `PtyResize` packet. SIGWINCH proper handling требует `signal_hook` — для MVP опционально poll'им size раз в 500ms (cheap), за фактический resize отвечает client'овский terminal.
- **`run_oneshot` и GUI shell-panel — без изменений.** Шлют обычный `ShellOpen = 0x40`, host идёт по pipe-flow, sentinel-protocol работает как раньше (0 регрессий PR #9).

## Technical Details

### Protocol changes (`crates/wiredesk-protocol/src/message.rs`)

```rust
pub enum MessageType {
    // ... existing ...
    ShellOpen = 0x40,
    ShellInput = 0x41,
    ShellOutput = 0x42,
    ShellClose = 0x43,
    ShellExit = 0x44,
    ShellOpenPty = 0x45,   // NEW
    PtyResize = 0x46,      // NEW
}

pub enum Message {
    // ... existing ShellOpen { shell: String } unchanged ...
    ShellOpenPty { shell: String, cols: u16, rows: u16 },  // NEW
    PtyResize { cols: u16, rows: u16 },                     // NEW
}
```

`ShellOpenPty` payload: `[cols: u16 LE][rows: u16 LE][shell: utf8]` — 4 байта + variable.
`PtyResize` payload: `[cols: u16 LE][rows: u16 LE]` — 4 байта fixed.

### Host (`apps/wiredesk-host/src/shell.rs`)

```rust
pub fn spawn(requested: &str, pty: Option<(u16, u16)>) -> Result<Self> {
    // Old call sites (run_oneshot, GUI) → spawn(name, None) → pipe path.
    // New call site (bridge_loop / ShellOpenPty) → spawn(name, Some((cols, rows))).
    if let Some((cols, rows)) = pty {
        spawn_pty(argv, cols, rows)
    } else {
        spawn_pipe(argv)  // existing flow extracted here
    }
}

pub fn resize(&self, cols: u16, rows: u16) {
    // no-op when self is pipe-mode
    if let Backend::Pty { master, .. } = &self.backend {
        let _ = master.resize(PtySize { cols, rows, pixel_width: 0, pixel_height: 0 });
    }
}
```

### Session routing (`apps/wiredesk-host/src/session.rs`)

```rust
Message::ShellOpen { shell } => {
    self.shell = Some(ShellProcess::spawn(&shell, None)?);
}
Message::ShellOpenPty { shell, cols, rows } => {
    self.shell = Some(ShellProcess::spawn(&shell, Some((cols, rows)))?);
}
Message::PtyResize { cols, rows } => {
    if let Some(sh) = &self.shell { sh.resize(cols, rows); }
}
```

### Term bridge_loop (`apps/wiredesk-term/src/main.rs`)

- Replace `Message::ShellOpen { shell }` send by `Message::ShellOpenPty { shell, cols, rows }` where `cols`/`rows` from `crossterm::terminal::size()` (fallback to 80×24).
- Drop `line_buf`, `line_cells`, `pop_utf8_char`-erase, `translate_output_for_terminal` — TTY на host'е делает echo и render.
- Stdin reader: `read(&mut [u8; N])` → каждый chunk → `Message::ShellInput { data }` immediately.
- Output handler: пишет `data` напрямую в stdout без `\n`→`\r\n` translate (raw bytes).
- Periodic 500ms-tick: если `crossterm::terminal::size()` изменился — `PtyResize` packet.

## What Goes Where

- **Implementation Steps** (`[ ]`): code/protocol/test changes в repo
- **Post-Completion** (no checkboxes): live-Win11 manual verification (нужен железный Win-host с CH340 для AC7 / latency-eval)

## Implementation Steps

### Task 1: Protocol — add ShellOpenPty + PtyResize message types

**Files:**
- Modify: `crates/wiredesk-protocol/src/message.rs`

- [ ] add `ShellOpenPty = 0x45` and `PtyResize = 0x46` variants to `MessageType` enum + corresponding `TryFrom<u8>` arms
- [ ] add `Message::ShellOpenPty { shell: String, cols: u16, rows: u16 }` and `Message::PtyResize { cols: u16, rows: u16 }` variants + `msg_type()` arms
- [ ] implement `serialize` arms: `ShellOpenPty` writes `cols.to_le_bytes() | rows.to_le_bytes() | shell.as_bytes()`; `PtyResize` writes `cols.to_le_bytes() | rows.to_le_bytes()`
- [ ] implement `deserialize` arms: `ShellOpenPty` reads first 4 bytes as cols/rows + remaining as utf8 shell-string (reject if payload < 4 bytes); `PtyResize` reads exactly 4 bytes (reject if payload != 4)
- [ ] write `roundtrip` tests: `ShellOpenPty { shell: "powershell", cols: 100, rows: 40 }`, `ShellOpenPty { shell: "", cols: 1, rows: 1 }`, `PtyResize { cols: 80, rows: 24 }`, `PtyResize { cols: 0xFFFF, rows: 0xFFFF }`
- [ ] write deserialize-error tests: `ShellOpenPty` payload of length 3 → error; `PtyResize` payload of length 5 → error; `ShellOpenPty` payload `[0x40, 0x00, 0x18, 0x00, 0xff, 0xfe]` (cols=64, rows=24, then invalid UTF-8 in shell field) → error
- [ ] verify backward compat: `ShellOpen { shell }` roundtrip still passes unchanged
- [ ] run tests: `cargo test -p wiredesk-protocol` — must pass before next task

### Task 2: Host — portable-pty dep + ShellProcess pty/pipe split

**Files:**
- Modify: `apps/wiredesk-host/Cargo.toml`
- Modify: `apps/wiredesk-host/src/shell.rs`

- [ ] add `portable-pty = "0.9"` to `apps/wiredesk-host/Cargo.toml` `[dependencies]`
- [ ] refactor `ShellProcess` to hold `enum Backend { Pipe { child: Child }, Pty { child: Box<dyn Child + Send>, master: Box<dyn MasterPty + Send> } }` (or equivalent — wrap in `Mutex` if portable-pty traits require `&mut self` for resize)
- [ ] split `spawn(requested: &str)` into `spawn(requested: &str, pty: Option<(u16, u16)>) -> Result<Self>`. If `pty.is_none()` → existing pipe flow (extract to `spawn_pipe(argv)` for clarity). If `pty.is_some()` → new `spawn_pty(argv, cols, rows)`:
  - `let pty_system = portable_pty::native_pty_system();`
  - `let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;`
  - `let cmd = portable_pty::CommandBuilder::new(&argv[0]); cmd.args(&argv[1..]);` + `cmd.cwd(...)` если нужен (можно пропустить, default = parent's CWD)
  - `let child = pair.slave.spawn_command(cmd)?;`
  - `drop(pair.slave);`
  - reader thread: `let reader = pair.master.try_clone_reader()?;` → `stream_to_channel(reader, events_tx)`
  - writer thread: `let writer = pair.master.take_writer()?;` → `writer_thread_pty(writer, stdin_rx)` (то же что existing `writer_thread` но `Box<dyn Write>` вместо `ChildStdin`)
  - return `ShellProcess { backend: Backend::Pty { ... }, ... }`
- [ ] add `pub fn resize(&self, cols: u16, rows: u16)` — match `self.backend`: `Pty { master, .. } → master.lock().resize(...)`, `Pipe { .. } → no-op`
- [ ] update `try_exit_code` and `kill` to dispatch on `Backend`. **Verify** `portable_pty::Child` trait API: `kill(&mut self) -> io::Result<()>`, `try_wait(&mut self) -> io::Result<Option<ExitStatus>>`, `wait(&mut self) -> io::Result<ExitStatus>`. `ExitStatus` от portable-pty имеет `exit_code() -> u32` (не Option<i32> как у `std::process::ExitStatus`) — нужна явная конверсия `as i32` или `i32::try_from(code).unwrap_or(-1)`. Boxed как `Box<dyn portable_pty::Child + Send + Sync>` (Send/Sync нужны для thread-passing).
- [ ] update existing host-side call site (only one: `apps/wiredesk-host/src/session.rs::handle Message::ShellOpen`) → `ShellProcess::spawn(&shell, None)`. GUI/term-side call sites не существуют — они шлют packet'ы, не вызывают spawn.
- [ ] keep `#[cfg(target_os = "windows")] CREATE_NO_WINDOW` flag only for pipe path (ConPTY не показывает console сам)
- [ ] write test `pty_echo_through_shell` (`#[cfg(not(target_os = "windows"))]`): spawn `/bin/sh` with `Some((40, 100))`, write `echo wiredesk-pty-test\nexit\n`, drain output, assert contains `wiredesk-pty-test`. Live timeout 3 sec.
- [ ] write test `resize_no_op_on_pipe_mode` (`#[cfg(not(target_os = "windows"))]`): spawn `/bin/sh` with `None`, call `resize(80, 24)`, assert no panic
- [ ] preserve existing `echo_through_shell` test (pipe-mode regression coverage) — should compile after spawn signature change
- [ ] run tests: `cargo test -p wiredesk-host` — must pass before next task

### Task 3: Host session — route ShellOpenPty + PtyResize

**Files:**
- Modify: `apps/wiredesk-host/src/session.rs`

- [ ] add match arm `Message::ShellOpenPty { shell, cols, rows } => { kill leftover shell; self.shell = Some(ShellProcess::spawn(&shell, Some((cols, rows)))?); }`
- [ ] add match arm `Message::PtyResize { cols, rows } => if let Some(sh) = &self.shell { sh.resize(cols, rows); }`
- [ ] verify `Message::ShellOpen` arm still compiles after spawn signature change (passes `None`)
- [ ] write unit test (using `MockTransport` + `MockInjector`): handshake → send `ShellOpenPty { shell: "/bin/sh", cols: 40, rows: 100 }` → tick session → assert `self.shell.is_some()`. Use `#[cfg(not(target_os = "windows"))]` для unix-side spawn.
- [ ] write unit test: после ShellOpenPty session принимает `PtyResize { cols: 80, rows: 24 }` без error
- [ ] run tests: `cargo test -p wiredesk-host` — must pass before next task

### Task 4: Term bridge_loop — switch to ShellOpenPty + pass-through raw

**Files:**
- Modify: `apps/wiredesk-term/src/main.rs`
- Modify: `apps/wiredesk-term/Cargo.toml` (verify `crossterm` dep present; уже есть для raw-mode)

- [ ] in `run()` → branch для interactive (non-`--exec`) replace `Message::ShellOpen { shell }` send with:
  - `let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 40));`
  - `Message::ShellOpenPty { shell: args.shell.clone(), cols, rows }`
- [ ] in `bridge_loop` remove cooked-mode infra: delete `line_buf`, `line_cells`, BS local-erase, `\n`-erase + flush. Stdin handler: read into `[u8; 4096]`, send each non-empty chunk as `ShellInput { data }` immediately. **Сохранить** existing Ctrl+] (0x1D) byte-level detection — на нём bridge_loop корректно exit'ит (telnet-style); detection делается на raw bytes до отправки, как и было.
- [ ] in `bridge_loop` reader thread: stop calling `translate_output_for_terminal`; write `data` as-is to stdout. Remove `last_was_cr` state.
- [ ] add periodic resize-poll: **cadence 500ms** (heartbeat thread tick'ает раз в 2s — слишком медленно для resize'а посреди vim'а). Spawn dedicated `resize_poll` thread that shares `writer` via `Arc<Mutex<...>>`. Каждые 500ms — `crossterm::terminal::size()` vs last sent → on diff `Message::PtyResize { cols, rows }`. Stops via shared `stop: AtomicBool`. SIGWINCH-via-`signal_hook` — отдельный follow-up (см. Post-Completion).
- [ ] keep `format_connected_banner` and Ctrl+] exit hotkey detection (still useful)
- [ ] update existing tests around `bridge_loop` cooked-mode helpers (pop_utf8_char, translate_output_for_terminal) — if their helpers are deleted, delete the tests too. **Do not** delete tests that exercise non-cooked behavior (banner, sentinel parsing, etc.)
- [ ] write test `bridge_loop_sends_shell_open_pty`: build via SplitPair fixture (см. existing `run_oneshot` integration tests), spawn `bridge_loop` in thread, assert first packet on wire after Hello/Ack is `ShellOpenPty` (not `ShellOpen`)
- [ ] write test `bridge_loop_forwards_stdin_byte_by_byte`: feed `[b'a', b'b', b'\n']` → assert sequence of `ShellInput` packets carries those bytes (no buffering)
- [ ] note (no test): `pop_utf8_char` and `translate_output_for_terminal` get deleted as dead code post-cooked-mode-removal. PTY-side CRLF handling — ConPTY/conhost emits CRLF natively; forkpty depends on remote shell echo. AC1 (`vim` smoke) in Task 7 verifies CRLF correctness end-to-end on Win11.
- [ ] verify `run_oneshot` path unchanged: `run_oneshot` still uses `Message::ShellOpen { shell }`, no PtyResize
- [ ] run tests: `cargo test -p wiredesk-term` — must pass (all 48+ должны зеленеть, plus 2 new) before next task

### Task 5: Verify --exec and GUI shell unchanged (regression coverage)

**Files:** (verify-only — no edits expected)
- Read: `apps/wiredesk-term/src/main.rs::run_oneshot`
- Read: `apps/wiredesk-client/src/main.rs` (shell-panel send-site)

- [ ] grep `apps/wiredesk-term/src/main.rs::run_oneshot` — confirm sends `Message::ShellOpen { shell }` (NOT `ShellOpenPty`)
- [ ] grep `apps/wiredesk-client/src/main.rs` — confirm GUI shell-panel sends `Message::ShellOpen { shell }` (NOT `ShellOpenPty`)
- [ ] run all 48 `wiredesk-term` tests — must pass without regression. Особое внимание `run_oneshot_*` tests (4 шт.)
- [ ] run all `wiredesk-host` tests including ClipboardSync regression — must pass
- [ ] run `cargo clippy --workspace -- -D warnings` — must be green
- [ ] run `cargo build --release --workspace` — must compile clean for both Mac (host on macOS uses MockInjector under cfg) and Windows (cross-check via `cargo check --target x86_64-pc-windows-gnu` if mingw available, else skip — Win build verified в Task 7)

### Task 6: Acceptance criteria verification

- [ ] AC8: `cargo test --workspace -- --test-threads=1` — green. ⚠️ Default parallel runner на macOS flakes (~50%) с SIGABRT в host'овом test-binary'е — это **pre-existing** baseline issue (не вызвано моими изменениями), proverено через `git stash` + master-side test runs. Решение: `--test-threads=1` для host; alternativ — пометить host'-тесты `serial_test::serial`. Для CI (если будет) — фиксированно `--test-threads=1`.
- [ ] record exact test counts after changes: `wiredesk-protocol N`, `wiredesk-host N`, `wiredesk-term N`. Comparison vs pre-change CLAUDE.md baseline (`148 client + 93 host + 48 term`). Записать здесь для Task 8 doc-update.
- [ ] AC8: `cargo clippy --workspace -- -D warnings` — green
- [ ] AC8: `cargo build --release --workspace` — green на Mac
- [ ] verify all requirements from Overview implemented (ShellOpenPty wire-path, PtyResize, pass-through bridge_loop, --exec untouched)
- [ ] verify edge cases handled (zero-byte shell name, max u16 cols/rows, payload-length errors, invalid UTF-8 in shell field)
- [ ] verify backward compat: workflow with old client (sending plain `ShellOpen = 0x40`) still works on new host

### Task 7: Live-Win11 verification (manual)

**Files:** none (manual smoke-tests on hardware)

- [ ] sync `feat/host-pty` to Win11 host machine, build via `cargo build --release -p wiredesk-host`
- [ ] AC1: `wd` → `vim /tmp/test.txt` (через WSL on host, или ssh dev из под host PS) → edit + `:q!` без артефактов
- [ ] AC2: `wd` → `ssh dev` (без `-tt`) → assert `.bashrc` загружен (alias-проверка), цвета работают; `exit` → host PS clean
- [ ] AC3: `wd` → стрелка вверх в host PS → last command подставлен (PSReadLine)
- [ ] AC4: `wd` → `git commit` (без `-m`) на dummy repo → vim editor работает, save+quit → commit applied
- [ ] AC5: `wd --exec "Get-ChildItem"` exit 0 + clean stdout без ANSI escapes — повторно запустить все 8 ACs из `feat/wd-exec` (см. `docs/plans/completed/20260503-wd-exec.md`); все зелёные
- [ ] AC5b: cross-mode contamination — после interactive `wd` сессии (PTY) → exit → сразу `wd --exec "Get-ChildItem"` (pipe). Каждый `wd*` invocation создаёт новый Hello/handshake → host kill'ает старый shell-slot → новый shell spawn'ится с правильным backend'ом. Verify: stdout `wd --exec` чистый, без leftover PTY ANSI sequences.
- [ ] AC6: GUI `WireDesk.app` shell-panel — типичная команда (`Get-Process | head 5`) рендерится без ANSI escape мусора
- [ ] AC7: resize окна `wd` (Ghostty/iTerm) во время запущенного `vim` → vim reflow корректный; на host PS `[Console]::WindowWidth` reports new value
- [ ] measure PSReadLine keystroke latency на 115200 — note in plan if visible. If unacceptable — file ⚠️ blocker, consider `Set-PSReadLineOption -EditMode None` runtime toggle (out-of-scope follow-up)

### Task 8: Update documentation + close out

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md` (only if user-facing change worth mentioning)
- Move: `docs/plans/20260503-host-conpty.md` → `docs/plans/completed/`

- [ ] update `CLAUDE.md` "Status" paragraph: mention ConPTY support for interactive `wd`
- [ ] update `CLAUDE.md` "Shell-over-serial" subsection: PTY-mode flow, `ShellOpenPty` opcode, when pipe vs PTY is chosen
- [ ] update `CLAUDE.md` "Known limitations": убрать "Shell без PTY" пункт или переформулировать (PTY теперь есть, но GUI shell-panel и --exec остаются pipe — это всё ещё ограничение)
- [ ] update `CLAUDE.md` test-count line: `148 client + 93+N host + 48+M term` (see Task 6 actual numbers)
- [ ] verify `README.md` — обновить run-section если pre-PTY flow упоминается, иначе skip
- [ ] `mkdir -p docs/plans/completed && git mv docs/plans/20260503-host-conpty.md docs/plans/completed/`
- [ ] commit accumulated work на ветке `feat/host-pty` (по логическим коммитам — protocol, host, term, docs)

## Post-Completion

*Items requiring manual intervention or external systems — no checkboxes, informational only*

**Manual verification (live Win11)** — handled in Task 7 above. Все AC1-AC8 повторяются на железе. PSReadLine latency на 115200 baud baud — subjective, известный risk.

**External system updates** — нет. Standalone change в нашем repo'у. Никаких consumer'ов, никаких deployment configs.

**Follow-up'ы вне scope'а** (если найдём по ходу):
- ConPTY для GUI shell-panel (egui terminal emulator) — отдельная задача.
- SIGWINCH-handler через `signal_hook` (вместо poll'а) для term-side resize'а — оптимизация.
- Multi-shell sessions (несколько одновременных PTY) — сейчас один shell-slot per session.
