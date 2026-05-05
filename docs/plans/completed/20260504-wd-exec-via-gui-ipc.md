# `wd --exec` via GUI IPC — implementation plan

## Overview

Embedded IPC поверх GUI client'а позволяет `wd --exec` работать параллельно с активным GUI (clipboard sync, capture). Сейчас оба процесса хотят `open()`'нуть один serial-порт — workflow ломается. После реализации GUI exposes Unix socket; `wd --exec` коннектится к нему и шлёт sentinel-команду через тот же serial, через который GUI делает clipboard sync. Если GUI закрыт — `wd --exec` falls back на текущий direct-open serial, поведение байт-в-байт идентично.

Полный спек — в [`docs/briefs/wd-exec-via-gui-ipc.md`](../briefs/wd-exec-via-gui-ipc.md).

## Context (from discovery)

- **Files involved:**
  - `apps/wiredesk-term/src/main.rs` (1865 строк) — `run_oneshot:381`, `format_command:273`, `parse_sentinel:688`, `clean_stdout:611`, `parse_ready:562`, `format_timeout_diagnostic:574`, `strip_ansi:330`, `OneShotState`, `ShellKind`. ~50 unit/integration-тестов в этом же файле.
  - Test fixtures для `run_oneshot_*`: `make_split_pair`, `ClientWriter`, `ClientReader`, `HostSide`, `extract_uuid_from_payload` (`apps/wiredesk-term/src/main.rs:1391-1511`) — основаны на `wiredesk_protocol::{cobs,Message,Packet}` + `wiredesk_transport::Transport`.
  - `apps/wiredesk-client/src/main.rs` (626 строк) — `outgoing_tx`/`outgoing_rx` mpsc channel (line 78), `reader_thread:472` emit'ит `TransportEvent::ShellOutput(data)` (592) и `ShellExit(code)` (595) в `events_tx` к UI.
  - `crates/wiredesk-protocol`, `crates/wiredesk-transport` — Message enum + Transport trait.
  - `Cargo.toml` workspace — 6 крейтов; добавляем седьмой `wiredesk-exec-core`.
- **Patterns observed:**
  - Pure helpers с unit-тестами рядом — стандарт проекта (`#[cfg(test)] mod tests`).
  - Mac-only код всегда под `cfg(target_os = "macos")` (см. `keyboard_tap.rs`).
  - mpsc-channels между потоками — главная межтопологическая абстракция в client'е.
- **Architectural gap (ключевой):** `run_oneshot` синхронно зовёт `reader.recv()` на own'ed `Box<dyn Transport>`. В client'е reader thread отдельный, его события идут через mpsc. Прямой re-use `run_oneshot` невозможен → требуется небольшая trait-абстракция `ExecTransport { send_input, recv_event }` с двумя impl'ами (serial-direct, mpsc-bridge). Обоснование в Solution Overview.

## Development Approach

- **testing approach**: Regular (code first, then tests) — соответствует существующему стилю проекта.
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** — без исключений
- **CRITICAL: all tests must pass before starting next task** — `cargo test --workspace -- --test-threads=1` зелёный (host parallel-flake обходим через `--test-threads=1`, см. `feedback_macos_test_thread_flake.md`).
- **CRITICAL: update this plan file when scope changes during implementation**
- maintain backward compatibility — AC3 (fallback при закрытом GUI) **критичен**, регрессий быть не должно

## Testing Strategy

- **unit tests:** required для каждой задачи. Pure helpers — table-driven, edge cases.
- **integration tests:** `UnixStream::pair()` для round-trip request/response IPC frame'ов; mock-server в test threads для verify fallback path в `wd --exec`.
- **e2e tests:** проект не имеет UI-based e2e (Playwright/Cypress нет). Live-проверка вручную на реальной паре Mac+Win по сценариям AC1-AC5.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope

## Solution Overview

**Архитектура — embedded IPC поверх GUI без extraction.** Никакого отдельного daemon-процесса, никакого launchd plist'а. GUI client'у добавляется один thread-acceptor + per-connection handler thread, оба cfg(target_os = "macos").

**Reuse `run_oneshot` через trait `ExecTransport`** (отклонение от исходного брифа уточнено). Бриф предлагал «duplicate либо trait» как открытый вопрос. После inspection кода принят trait — duplicate ~160 строк stateful-логики `run_oneshot` (2-state machine, ANSI-stripping, sentinel-walker, partial-prompt peek, timeout, sentinel-glued-to-unterminated-output recovery — это core production-контракт `wd --exec`) в GUI handler'е имеет реальный risk drift'а от term-импла; abstraction на 2-х method trait минимальна.

```
trait ExecTransport {
    fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError>;
    fn recv_event(&mut self, timeout: Duration) -> Result<ExecEvent, ExecError>;
}
enum ExecEvent { ShellOutput(Vec<u8>), ShellExit(i32), HostError(String), Idle }
enum ExecError {
    /// Underlying transport failure (serial IO, mpsc Sender disconnected mid-send).
    Transport(String),
    /// Reader-side disconnected — used by `IpcExecTransport` when its mpsc Sender dropped.
    /// `SerialExecTransport` маппит EOF тоже на `Closed`. `recv_event` differentiates: idle window timeout = `Ok(Idle)`, permanent close = `Err(Closed)`.
    Closed,
}
```

- `SerialExecTransport` (для `wd --exec` standalone-path) — оборачивает существующие `Arc<Mutex<Box<dyn Transport>>>` writer + `Box<dyn Transport>` reader. `send_input` локает writer и шлёт `ShellInput`. `recv_event` зовёт `reader.recv()` с timeout window'ом + фильтрацией Message типов.
- `IpcExecTransport` (для client'ского IPC handler'а) — оборачивает `mpsc::Sender<Packet>` (existing `outgoing_tx`) + новый `mpsc::Receiver<ExecEvent>` от modified reader thread. `send_input` шлёт `Packet::new(Message::ShellInput, ...)` в `outgoing_tx`. `recv_event` читает из mpsc.

**Reader broadcast:** client'ский `reader_thread` сейчас эмитит `ShellOutput`/`ShellExit` только в `events_tx` к UI. Добавляется параллельная отправка тех же event'ов в `Option<mpsc::Sender<ExecEvent>>` slot — заполняется когда IPC connection accept'нут, очищается на disconnect/panic через RAII Drop guard. Fan-out тривиальный (`if let Some(tx) = ... { let _ = tx.send(...); }`), не ломает UI flow.

**Streaming stdout (required, не bundled).** `run_oneshot` после рефакторинга принимает `on_chunk: FnMut(&[u8])` callback и эмитит **post-READY** (mute до первого READY-marker'а) chunks по мере прихода — без аккумулирования и финального `clean_stdout`-slice'а. Phase-tracker в runner'е: `Mute → Streaming → Done`. Standalone term тоже выигрывает: `wd --exec docker logs --tail 200` видит output по мере прихода, не молчит 30s до конца. IPC handler в callback'е пишет `IpcResponse::Stdout(chunk)` в socket; в финале — `IpcResponse::Exit(code)`.

**Single-in-flight enforcement:** один `Mutex<()>` на acceptor; concurrent connections блокируются на lock-acquire. RAII `MutexGuard<()>` обеспечивает panic-safe unlock. Rationale: single serial writer всё равно serializ'ит, одновременная queue команд бесполезна.

**Socket discovery + lifecycle:** path вычисляется единой pub function `wiredesk_exec_core::ipc::default_socket_path() -> PathBuf` (single source of truth — используется и в client'е и в term'е, чтобы не drift'ить). Дефолт: `~/Library/Application Support/WireDesk/wd-exec.sock`. На GUI start — `unlink_or_ignore()` + `bind` + `chmod 0600`. Drop guard в acceptor'е делает `unlink` при exit. Если bind упал (ENOENT на dir, EADDRINUSE) — log warn, GUI продолжает работать без IPC (никакой паники, fallback в term'е и так покрывает кейс).

**Fallback в `wd --exec`:** на любую IO error при connect (`ENOENT`, `ECONNREFUSED`, `connect_timeout` 200ms) идём в текущий direct-serial path. Дополнительно: после успешного connect — `set_read_timeout(2s)` на первом response frame'е. Если за 2s не пришло **никакого** frame'а (handler hung на `single_inflight.lock()` от прошлого зависшего exec'а или panic'нул в accept queue) — fallback на serial с warning'ом в stderr. После первого ответа timeout снимается (long-running команды могут молчать минутами в `Mute`-фазе).

**Cancellation propagation (acceptable behavior):** Ctrl+C на `wd --exec` через IPC закрывает UnixStream → handler thread получает write-error на следующем chunk. Handler **не прерывает** running command на Host'е — ждёт sentinel или timeout, потом освобождает `single_inflight`. Это ожидаемое поведение, документируется в `docs/wd-exec-usage.md`. Reasoning: прерывать host-side shell mid-run опасно (команда могла начать destructive operation), wait-for-sentinel = clean state.

## Technical Details

- **New crate `wiredesk-exec-core`** — `crates/wiredesk-exec-core/`:
  - `Cargo.toml` deps: `thiserror`, `uuid` (workspace), `serde` + `bincode` (для ipc module), `log`. Dev-deps: `wiredesk-protocol`, `wiredesk-transport` (для term-side тестов `SerialExecTransport`).
  - `src/lib.rs` — pub mod exports
  - `src/helpers.rs` — `format_command`, `parse_sentinel`, `clean_stdout`, `parse_ready`, `format_timeout_diagnostic`, `strip_ansi`, `is_remote_prompt`, `is_powershell_prompt`
  - `src/types.rs` — `ShellKind`, `OneShotState`, `ExecEvent`, `ExecError`
  - `src/transport.rs` — `ExecTransport` trait + `MockExecTransport` (cfg-test)
  - `src/runner.rs` — `run_oneshot<T, F>(transport: &mut T, cmd, ssh, timeout, on_chunk: F) -> Result<i32>` (returns exit code; stdout streamed via callback)
  - `src/ipc.rs` — `IpcRequest`/`IpcResponse` + length-prefix codec + `default_socket_path()` pub function
  - `tests/` — новые тесты на MockExecTransport
- **`wiredesk-term`** — Cargo.toml depends on `wiredesk-exec-core`. `main.rs` сокращается ~700 строк (helpers + state-machine уезжают). Остаётся: `Args` parsing, `handshake`, `bridge_loop` (interactive PTY-mode), `run` orchestration, `try_socket_first` (новое), `SerialExecTransport` impl-wrapper, **6 существующих split-pair `run_oneshot_*` integration-тестов** (теперь они покрывают `SerialExecTransport`-impl).
- **`wiredesk-client`** — Cargo.toml depends on `wiredesk-exec-core`. Новый файл `src/ipc.rs` (~250 строк) под `cfg(target_os = "macos")`. Wiring в `main.rs` за тем же cfg.
- **Reader broadcast wiring:** `reader_thread` сигнатура расширяется на `exec_event_slot: Arc<Mutex<Option<mpsc::Sender<ExecEvent>>>>`. IPC handler set'ит slot через RAII `ExecSlotGuard` на accept, automatic clear на drop (включая panic).
- **IPC frame protocol:** length-prefix u32 BE + bincode 1.x. Reject если len > 16 MB. Не используем COBS+CRC из `wiredesk-protocol` — Unix socket уже даёт reliable byte-stream.
- **Logging discipline в IPC:** `log::info` на accept + handler-start/end, `log::warn` на bind failure / parse error / read-timeout-fallback, `log::debug` на single_inflight contention / exec_slot clear / Sender disconnect.

## What Goes Where

- **Implementation Steps** — все задачи здесь, code+tests внутри workspace.
- **Post-Completion** — manual live-test на реальном Mac+Win с активным image-transfer, измерение mouse latency под IPC load.

## Implementation Steps

### Task 0: Bump default `wd --exec` timeout 30 → 90 sec

**Files:**
- Modify: `apps/wiredesk-term/src/main.rs` (Args struct)
- Modify: `docs/wd-exec-usage.md`

- [ ] в `Args` struct изменить `#[arg(long, default_value_t = 30)] timeout: u64` → `default_value_t = 90`
- [ ] grep `30` в `docs/wd-exec-usage.md` и `README.md`, обновить упоминания default'а
- [ ] добавить smoke-тест `Args::parse(["wd", "--exec", "x"])` → `args.timeout == 90` (один `assert_eq!`)
- [ ] существующие `run_oneshot_*_returns_124`-тесты передают timeout явно (`apps/wiredesk-term/src/main.rs:1535,1580,1595,1612,1661`) — менять не надо
- [ ] run `cargo test --workspace -- --test-threads=1` — все 360 + 1 новый тест зелёные

### Task 1: Создать crate `wiredesk-exec-core`, перенести pure helpers + типы

**Files:**
- Create: `crates/wiredesk-exec-core/Cargo.toml`
- Create: `crates/wiredesk-exec-core/src/lib.rs`
- Create: `crates/wiredesk-exec-core/src/helpers.rs`
- Create: `crates/wiredesk-exec-core/src/types.rs`
- Modify: `Cargo.toml` (workspace members + workspace dep)
- Modify: `apps/wiredesk-term/Cargo.toml` (add `wiredesk-exec-core` dep)
- Modify: `apps/wiredesk-term/src/main.rs` (remove migrated symbols, add `use wiredesk_exec_core::...`)

- [ ] создать новый crate с `Cargo.toml`: edition 2021, license MIT, workspace deps: `thiserror`, `uuid` (workspace, features = ["v4"]), `log`. **uuid обязателен** — все helper'ы (`format_command`, `parse_sentinel`, `clean_stdout`, `parse_ready`) принимают `&uuid::Uuid`
- [ ] перенести pure helpers (`format_command`, `parse_sentinel`, `clean_stdout`, `parse_ready`, `format_timeout_diagnostic`, `strip_ansi`, `is_remote_prompt`, `is_powershell_prompt`) из `apps/wiredesk-term/src/main.rs` в `crates/wiredesk-exec-core/src/helpers.rs` как `pub fn`
- [ ] перенести `ShellKind`, `OneShotState` enum'ы в `src/types.rs`
- [ ] добавить `pub use` re-exports в `lib.rs`
- [ ] обновить `apps/wiredesk-term/src/main.rs` — заменить локальные definitions на `use wiredesk_exec_core::{ShellKind, OneShotState, helpers::*}`
- [ ] перенести помеченные unit-тесты для перенесённых helpers (`parse_sentinel_*`, `clean_stdout_*`, `format_command_*`, `format_timeout_*`, `strip_ansi_*`, `is_powershell_prompt_*`, `is_remote_prompt_*` — все ~30 шт.) в `crates/wiredesk-exec-core/src/helpers.rs::tests`
- [ ] **оставить `run_oneshot_*` integration-тесты в term** — они переедут как покрытие `SerialExecTransport`-impl в Task 3 (НЕ переносить сейчас)
- [ ] run `cargo test --workspace -- --test-threads=1` — все 360+ тестов зелёные (просто в новых местах для helpers)

### Task 2: Определить `ExecTransport` trait + `ExecEvent` + `ExecError`

**Files:**
- Create: `crates/wiredesk-exec-core/src/transport.rs`
- Modify: `crates/wiredesk-exec-core/src/lib.rs` (re-export)
- Modify: `crates/wiredesk-exec-core/src/types.rs` (add `ExecEvent`, `ExecError`)

- [ ] добавить enum `ExecEvent { ShellOutput(Vec<u8>), ShellExit(i32), HostError(String), Idle }` в `types.rs`
- [ ] добавить enum `ExecError { Transport(String), Closed }` через `thiserror`. Документировать explicit mapping: idle window timeout = `Ok(Idle)`, permanent close = `Err(Closed)` (для обоих impl'ов)
- [ ] определить `pub trait ExecTransport { fn send_input(&mut self, data: &[u8]) -> Result<(), ExecError>; fn recv_event(&mut self, timeout: std::time::Duration) -> Result<ExecEvent, ExecError>; }`
- [ ] написать `MockExecTransport` за `#[cfg(test)]` — берёт `Vec<ExecEvent>` queue, эмитит на recv с замером времени; tracks outbox `Vec<Vec<u8>>` от send_input
- [ ] написать unit-тесты на MockExecTransport: basic enqueue/dequeue, timeout returns Idle, send_input записывается в outbox, empty queue + timeout → `Ok(Idle)`, dropped queue → `Err(Closed)`
- [ ] run tests — must pass before next task

### Task 3: Перенести `run_oneshot` в `wiredesk-exec-core::runner` со streaming + рефакторинг term'а на trait

**Files:**
- Create: `crates/wiredesk-exec-core/src/runner.rs`
- Modify: `crates/wiredesk-exec-core/src/lib.rs` (re-export)
- Modify: `apps/wiredesk-term/src/main.rs` (новый `SerialExecTransport` impl, callback wiring)

- [ ] перенести логику `run_oneshot` в `runner.rs` с **новой сигнатурой**: `pub fn run_oneshot<T: ExecTransport, F: FnMut(&[u8])>(transport: &mut T, cmd: &str, ssh: Option<&str>, timeout_secs: u64, mut on_chunk: F) -> Result<i32, ExecError>`
- [ ] **phase-tracker** в runner: `Mute → Streaming → Done`. Mute до прихода первого READY-marker'а (`__WD_READY_<uuid>__`); после READY вызывается `on_chunk(post_ready_bytes)` per-chunk (после ANSI strip + line-walker'а). На sentinel — Done, return exit code
- [ ] heartbeat остаётся orchestration-заботой caller'а (term'овский `run` сам спавнит heartbeat thread; IPC handler полагается на GUI'ёвый heartbeat который и так работает)
- [ ] в `apps/wiredesk-term/src/main.rs` написать `struct SerialExecTransport { writer: Arc<Mutex<Box<dyn Transport>>>, reader: Box<dyn Transport> }`. Импл `ExecTransport`: `send_input` → лок writer + `Packet::new(Message::ShellInput, ...)`; `recv_event` → `reader.recv()` с timeout window'ом + фильтрация Message типов (`ShellOutput → ExecEvent::ShellOutput`, `ShellExit → ShellExit`, `Error → HostError`, transport-timeout → Idle, EOF/IO-error → `Err(Closed)`)
- [ ] term `run` функция: создаёт `SerialExecTransport`, вызывает `wiredesk_exec_core::run_oneshot(&mut transport, cmd, ssh, timeout, |chunk| { stdout().write_all(chunk).ok(); })`, sets exit-code. На timeout (поведение runner'а — return error variant или special exit?) → eprintln `format_timeout_diagnostic` + exit 124. Выбрать: runner возвращает `Err(ExecError::Closed)` на timeout-с-buffer'ом, caller печатает diagnostic. Либо runner сам печатает — нет, разделение responsibilities — caller печатает
- [ ] **оставить 6 существующих `run_oneshot_*` integration-тестов** в `apps/wiredesk-term/src/main.rs::tests` (через `make_split_pair`) — они **теперь покрывают `SerialExecTransport`-impl**. Минимальные правки: callback parameter (через `|chunk| stdout_buf.lock().unwrap().extend_from_slice(chunk)` для assertion'а)
- [ ] **дополнительно написать** новые equivalent-тесты в `crates/wiredesk-exec-core/src/runner.rs::tests` через `MockExecTransport` (5 case'ов: happy_ps, happy_ssh, timeout, nonzero, unterminated). Это +тесты, не migration — две независимые impl'ы, обе покрыты
- [ ] добавить unit-тест на phase-tracker: pre-READY chunks НЕ попадают в `on_chunk` (assertion `assert!(callback_buf.is_empty())` до READY); post-READY — попадают
- [ ] run `cargo test --workspace -- --test-threads=1` — все тесты зелёные (старые 6 + новые 5+ + helper'ы)

### Task 4: Reader thread в client'е — broadcast `ShellOutput`/`ShellExit` в exec-event mpsc через RAII slot

**Files:**
- Modify: `apps/wiredesk-client/src/main.rs`
- Modify: `apps/wiredesk-client/Cargo.toml` (add `wiredesk-exec-core` dep)

- [ ] добавить `pub type ExecEventSlot = Arc<Mutex<Option<mpsc::Sender<ExecEvent>>>>` (импорт `ExecEvent` из `wiredesk-exec-core::types`)
- [ ] **RAII guard:** `pub struct ExecSlotGuard(ExecEventSlot);` с `Drop impl` который делает `slot.lock().unwrap().take()`. Используется IPC handler'ом — guard живёт на стеке handler thread'а, на panic / normal return slot автоматически очищается
- [ ] modify `reader_thread` — принимает `exec_slot: ExecEventSlot`. На `Message::ShellOutput { data }` после существующего `events_tx.send(TransportEvent::ShellOutput(data.clone()))` добавить `if let Some(tx) = slot.lock().unwrap().as_ref() { let _ = tx.send(ExecEvent::ShellOutput(data.clone())); }` (clone — data уже клонирован для UI; здесь второй clone, минимальный hot-path overhead)
- [ ] то же для `ShellExit`/`Error`
- [ ] в `main.rs`: создать `let exec_slot: ExecEventSlot = Arc::new(Mutex::new(None));` и передать его в `reader_thread` spawn + хранить clone для IPC handler'а (Task 6)
- [ ] **lifecycle unit-тесты** в `main.rs::tests` или новый `tests/exec_slot.rs`:
  - (a) slot=None → reader делает no-op (assertion: events_tx получил event, exec_slot Sender вообще не был вызван)
  - (b) Sender disconnected mid-stream (drop'нули receiver) → reader продолжает работать без panic, send error silently dropped
  - (c) re-set slot после clear (Drop guard сработал) → новый Sender получает свежие events после re-attach
- [ ] run tests — must pass before next task

### Task 5: IPC frame protocol — `IpcRequest`/`IpcResponse` codec + `default_socket_path()`

**Files:**
- Create: `crates/wiredesk-exec-core/src/ipc.rs`
- Modify: `crates/wiredesk-exec-core/Cargo.toml` (add `bincode 1.x`, `serde 1` already в workspace deps)
- Modify: `crates/wiredesk-exec-core/src/lib.rs` (re-export)

- [ ] добавить `bincode = "1"` в `crates/wiredesk-exec-core/Cargo.toml`
- [ ] определить `pub struct IpcRequest { cmd: String, ssh: Option<String>, timeout_secs: u64 }` с `Serialize + Deserialize`
- [ ] определить `pub enum IpcResponse { Stdout(Vec<u8>), Exit(i32), Error(String) }`
- [ ] написать `pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()>` — length-prefix u32 BE + bytes
- [ ] написать `pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>>` — read u32 BE + read N bytes; reject если len > 16 MB
- [ ] написать `pub fn write_request<W: Write>(w: &mut W, req: &IpcRequest) -> io::Result<()>` (frame + bincode encode)
- [ ] написать `pub fn read_request<R: Read>(r: &mut R) -> io::Result<IpcRequest>` (frame + bincode decode)
- [ ] написать `pub fn write_response<W: Write>(w: &mut W, resp: &IpcResponse) -> io::Result<()>` (frame + bincode encode)
- [ ] написать `pub fn read_response<R: Read>(r: &mut R) -> io::Result<IpcResponse>` (frame + bincode decode)
- [ ] написать `pub fn default_socket_path() -> std::path::PathBuf` — на Mac: `~/Library/Application Support/WireDesk/wd-exec.sock` через `home::home_dir()` или `std::env::var("HOME")`; non-Mac: `std::env::temp_dir().join("wd-exec.sock")` (или `unimplemented!()` — non-Mac не использует IPC)
- [ ] unit-тесты: round-trip через `Vec<u8>` reader/writer для каждой пары (Request, Stdout, Exit, Error), edge cases (empty stdout, large 1MB Stdout chunk, malformed length > 16MB rejected)
- [ ] integration-тест через `UnixStream::pair()` (cfg(unix)): writer thread → reader main thread → bytes equal
- [ ] unit-тест на `default_socket_path()`: возвращает path containing "WireDesk/wd-exec.sock" (без assert на абсолютную path)
- [ ] run tests — must pass before next task

### Task 6: IPC server в client (Mac-only) — UnixListener + IpcExecTransport + RAII guards

**Files:**
- Create: `apps/wiredesk-client/src/ipc.rs`
- Modify: `apps/wiredesk-client/src/main.rs` (wire-up)

- [ ] новый файл `ipc.rs` с `pub fn spawn_ipc_acceptor(socket_path: PathBuf, outgoing_tx: mpsc::Sender<Packet>, exec_slot: ExecEventSlot, single_inflight: Arc<Mutex<()>>)`. Весь файл за `#[cfg(target_os = "macos")]`
- [ ] в `spawn_ipc_acceptor`: `let _ = std::fs::remove_file(&socket_path)` (ignore-not-found), `match UnixListener::bind(&socket_path)`. **На Err → `log::warn!("IPC bind failed: {}; wd --exec will use direct serial fallback", e)` + return** — GUI продолжает работать без IPC, никакой паники. На Ok → `chmod 0600` через `std::os::unix::fs::PermissionsExt::set_mode`
- [ ] thread-loop: `for stream in listener.incoming()` → spawn handler thread с `move stream + clones`. `log::info!("IPC connection accepted")` на каждый accept
- [ ] handler thread structure:
  ```
  let _inflight_guard = single_inflight.lock();  // RAII unlock on drop
  let (tx, rx) = mpsc::channel::<ExecEvent>();
  let _slot_guard = ExecSlotGuard::install(&exec_slot, tx);  // RAII clear on drop
  let req = read_request(&mut stream)?;
  let mut transport = IpcExecTransport { outgoing_tx: outgoing_tx.clone(), rx };
  let exit_code = run_oneshot(&mut transport, &req.cmd, req.ssh.as_deref(), req.timeout_secs,
      |chunk| { let _ = write_response(&mut stream, &IpcResponse::Stdout(chunk.to_vec())); }
  )?;
  write_response(&mut stream, &IpcResponse::Exit(exit_code))?;
  ```
  Оба guard'а гарантируют cleanup на panic
- [ ] handler error handling: `read_request` failed → log warn + close stream без emit'а Exit; `run_oneshot` Err → write `IpcResponse::Error(msg)` + close
- [ ] `IpcExecTransport` impl в том же файле: `send_input` шлёт `Packet::new(Message::ShellInput { data: ... }, 0)` в `outgoing_tx`; `recv_event` — `rx.recv_timeout(timeout)` с маппингом (mpsc disconnect → `Err(Closed)`, timeout → `Ok(Idle)`, OK → проброс)
- [ ] в `apps/wiredesk-client/src/main.rs`: cfg(target_os = "macos") → определить `socket_path = wiredesk_exec_core::ipc::default_socket_path()`, создать `single_inflight: Arc<Mutex<()>>`, `spawn_ipc_acceptor(...)`. Drop cleanup на app exit (через atexit-style hook или просто полагаемся на kernel cleanup при process death)
- [ ] **logging:** `log::info` на acceptor start, accept, handler exit. `log::warn` на bind failure, `read_request` parse error. `log::debug` на `single_inflight` contention (если lock acquire > 100ms — log it)
- [ ] unit-тесты:
  - `IpcExecTransport` round-trip: mock outgoing_tx (через `mpsc::channel`) + push events в rx → assert `recv_event` arrived correctly + send_input wrote to outgoing_tx
  - `ExecSlotGuard` panic-safety: spawn thread, install guard, panic, verify slot is empty after thread join
- [ ] integration-тест: spawn acceptor на `tempfile::tempdir`-based socket path + client-side connect через UnixStream + send IpcRequest + pre-loaded ExecEvent queue в slot (через тестовый back-door) + assert IpcResponse stream equals expected
- [ ] integration-тест: bind на занятом socket path → assert acceptor returns gracefully + log warn (через `tracing-test` или log capture)
- [ ] run `cargo test --workspace -- --test-threads=1` — все зелёные

### Task 7: IPC client в `wd --exec` с fallback (try_socket_first) + read-timeout

**Files:**
- Modify: `apps/wiredesk-term/src/main.rs`

- [ ] в `run` функции: если `args.exec.is_some()` → попробовать `try_socket_first(&args)` под `cfg(target_os = "macos")` ДО existing serial path. На non-Mac → skip напрямую к serial
- [ ] `try_socket_first` impl:
  - `let socket_path = wiredesk_exec_core::ipc::default_socket_path();` (single source of truth)
  - `match UnixStream::connect_timeout(&socket_path, Duration::from_millis(200))`:
    - `Err(_)` (ENOENT, ECONNREFUSED, timeout) → `Ok(None)` — caller идёт в serial fallback
    - `Ok(stream)` → продолжаем
  - `stream.set_read_timeout(Some(Duration::from_secs(2)))?` — на первый response frame
  - `write_request(&mut stream, &IpcRequest { cmd, ssh, timeout_secs })`
  - первый `read_response(&mut stream)`:
    - `Err(io::ErrorKind::WouldBlock | TimedOut)` → `eprintln!("wd: GUI IPC unresponsive, falling back to direct serial"); Ok(None)`
    - `Ok(_)` → handle response, **снять read_timeout** (`stream.set_read_timeout(None)?`) для long-running commands в Mute-фазе
  - в loop: `read_response`: `Stdout(data)` → `stdout().write_all(&data)`, `Exit(code)` → return `Ok(Some(code))`, `Error(msg)` → `eprintln!("wd: host error: {msg}"); Ok(Some(1))`
- [ ] non-Mac platforms: `try_socket_first` всегда `Ok(None)` (cfg-gated stub function)
- [ ] unit-тест: fake socket path doesn't exist → `try_socket_first` returns Ok(None) gracefully (test через `tempdir::tempdir().path()` несуществующий)
- [ ] integration-тест: spawn fake-server thread (UnixListener на temp path) emit'ящий predetermined `IpcResponse` stream → invoke `try_socket_first` → assert stdout assembled correctly + correct exit code
- [ ] integration-тест на read-timeout: spawn fake-server который accept'ит но не пишет ничего → `try_socket_first` возвращает `Ok(None)` после 2s + warning в stderr
- [ ] **AC3 byte-equality regression test**: scripted host-script с известным output. Прогнать через (a) текущий `run_oneshot` direct (не используя IPC) и (b) IPC fallback path (когда `try_socket_first` returned Ok(None)). Assert byte-equal stdout. Это — explicit AC3 invariance check
- [ ] run tests — must pass before next task

### Task 8: Verify acceptance criteria

- [ ] AC1 verify: GUI запущен с capture, clipboard polling. `wd --exec --ssh prod "uname -a"` за <5 сек, exit 0. **stdout начинает приезжать через ≤500ms после первого chunk'а от Host'а** (streaming working). Mouse latency не >50ms (subjective)
- [ ] AC2 verify: GUI mid-image-transfer (1MB PNG ~80 sec). `wd --exec "echo ok"` ждёт окончания, exit 0 в пределах 90s timeout
- [ ] AC3 verify: GUI закрыт. `wd --exec "..."` falls back на serial — байт-в-байт идентично pre-implementation (запустить existing `docs/wd-exec-usage.md` примеры). Также: integration test из Task 7 уже это покрыл automatic'ом
- [ ] AC4 verify: два параллельных `wd --exec` процесса в Ghostty. Оба отрабатывают sequentially, sentinel'ы не пересекаются (проверить через unique cmd outputs)
- [ ] AC5 verify: `kill -9` GUI mid-running. Запустить GUI заново — stale socket cleanup'нулся, новый bind OK, `wd --exec` работает
- [ ] AC6 verify: `cargo test --workspace -- --test-threads=1` — 360+ existing + new IPC tests зелёные. `cargo clippy --workspace -- -D warnings` зелёный
- [ ] **Hung-handler scenario verify**: simulate stuck handler (через `gdb` или modified build с `thread::sleep(60s)` в handler) → `wd --exec` через IPC возвращает fallback warning + falls back to direct serial → exit 0 (если GUI закрыт по этому пути serial) или 124 timeout (если GUI всё ещё держит порт). Главное — нет deadlock'а на 90s+

### Task 9: Update documentation

- [ ] update `CLAUDE.md`: добавить раздел про IPC mode в "Run" / "Architecture"
- [ ] update `docs/architecture.md`: новый crate `wiredesk-exec-core`, IPC layer, reader broadcast, RAII guards
- [ ] update `docs/wd-exec-usage.md`:
  - explicit раздел «Параллельная работа с GUI»
  - новый default timeout 90s
  - **поведение Ctrl+C**: handler ждёт sentinel/timeout, не прерывает host-side команду (acceptable behavior, документируется)
- [ ] update `README.md`: одна строка в feature list о параллельном `wd --exec`
- [ ] update memory `project_wiredesk.md` через actualize-docs flow по итогам имплементации
- [ ] move `docs/plans/20260504-wd-exec-via-gui-ipc.md` → `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems — informational only*

**Manual live-test scenarios:**
- Реальная сессия Claude в чате через IPC: пользователь работает в GUI, я делаю `wd --exec --ssh prod "docker logs --tail 100 ..."` параллельно. Проверить что cursor capture не дёргается, что clipboard sync продолжает работать (Cmd+C на Host'е → arrives на Mac).
- Stress: запустить `for i in {1..20}; do wd --exec --ssh prod "echo $i"; done` пока GUI активно делает image-transfer. Verify single-in-flight queue работает корректно.
- Mouse latency измерение под IPC load — субъективно или через простой ping-style hotkey timer.
- Cancellation behavior verify: запустить `wd --exec --ssh prod "sleep 30"`, Ctrl+C через 5 сек. Verify `wd --exec` процесс умер сразу, GUI не зависает, следующий `wd --exec` ждёт sleep до завершения (acceptable).

**Внешних систем не задействовано** — Win-host без изменений, никаких deployment configs, никаких third-party integrations.

**Future migration path** (если когда-нибудь потребуется TUI-client / multi-GUI / restart-без-drop'а serial-link'а):
- Тот же `IpcRequest`/`IpcResponse` protocol уезжает в standalone `wiredesk-daemon` процесс.
- Текущий GUI handler становится proxy-implementation того же protocol'а.
- Migration cost: ~1 неделя поверх этой работы.
