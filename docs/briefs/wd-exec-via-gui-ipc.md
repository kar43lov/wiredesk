# Бриф: `wd --exec` через GUI IPC — параллельная работа без терминирования GUI

**Status:** brainstorm закончен 2026-05-04, готово к `/planning:make`. Заменяет более широкий `daemon-multiplex.md` (тот scope не нужен — interactive `wd` параллельно не требуется).

## Цель

`wd --exec` работает пока `WireDesk.app` запущен, не break'ая clipboard sync и не требуя терминирования GUI.

## Контекст: реальный workflow

GUI на Mac постоянно открыт — пользователь работает с Host через capture/clipboard (копирует тексты задач, картинки → отдаёт Claude в чате). Claude параллельно использует `wd --exec --ssh prod "..."` для триажа. Сейчас второе невозможно: serial-порт эксклюзивный, оба процесса хотят `open()`. Workflow: каждый запрос Claude'а требует Quit GUI → exec → restart → restore capture, ~30 сек × N раз в день.

Interactive `wd` (PowerShell PTY в Ghostty) пользователем сам почти не используется. Краевой случай — закрыл GUI, поработал руками, открыл обратно.

## Выбранный подход: embedded IPC поверх GUI

GUI на старте поднимает Unix-сокет `~/Library/Application Support/WireDesk/wd-exec.sock`. `wd --exec` пробует socket → шлёт `Request { cmd, ssh, timeout }` → читает stream `Stdout` chunks + `Exit { code }`. Socket нет → fallback на текущий direct-open serial (backward-compatible).

Внутри GUI: новый IPC-acceptor поток + per-connection handler. Handler формирует sentinel-команду (та же логика что в `wiredesk-term::run_oneshot`), пушит в существующий serial-writer mpsc, читает `ShellOutput` от reader'а, slice'ит по sentinel'у, шлёт обратно в IPC client.

**Pros:** Effort ~5–7 дней. GUI lifecycle = lifetime IPC, никакого launchd / restart-on-crash. Backward-compatible. Не трогает Win-host. Логика `run_oneshot` reusable — переезжает в shared crate.

**Cons:** clipboard image transfer (50–100 сек на 1 MB PNG) блокирует exec. Mitigation: дефолтный timeout `wd --exec` поднимаем 30 → 90 сек.

**Отвергнутая альтернатива:** stub-daemon extraction (отдельный процесс `wiredesk-daemon`). +5–7 дней, добавляет lifecycle/restart-on-crash. YAGNI: пока никто не пользуется multi-client; embedded IPC легко мигрирует на daemon позже (тот же protocol = proxy-implementation).

## Требования

- GUI exposes socket `~/Library/Application Support/WireDesk/wd-exec.sock`. Cleanup при exit + `unlink`-перед-`bind` для stale-после-crash.
- Socket protocol — binary, COBS+CRC framing (consistency с serial protocol'ом). Payload через `postcard` или `bincode`. Минимальный enum:
  - `Request { cmd: String, ssh: Option<String>, timeout_secs: u64 }`
  - `Response::Stdout(Vec<u8>)` — chunked
  - `Response::Exit { code: i32 }` — terminal frame
- `wd --exec` (`apps/wiredesk-term/src/main.rs`): новая ветка `connect_socket()` — пробует UnixStream::connect; на `ENOENT`/`ECONNREFUSED` → текущий serial-path. Без socket fallback'а нет (sometimes GUI down).
- GUI IPC handler: один in-flight exec, остальные queued (FIFO, max 4 в очереди — больше → reject `EAGAIN`-style error). Один writer на serial = serialization бесплатна.
- Default timeout `wd --exec` 30 → 90 сек (покрывает worst-case image transfer на 11 KB/s wire).
- `run_oneshot` логика: вынести в новый crate `wiredesk-exec-core` (или модуль в `wiredesk-protocol`?), reusable между `wiredesk-term` (serial-path) и GUI handler (mpsc-path). Параметризовать transport через trait.

## Acceptance criteria

- **AC1.** GUI active, capture engaged, clipboard polling active. `wd --exec --ssh prod "uname -a"` отрабатывает <5 сек, exit 0. Mouse latency в capture не растёт >50 ms (нет visible jank).
- **AC2.** GUI active, mid-image-transfer (1 MB PNG → ~80 сек). `wd --exec "echo ok"` ждёт окончания transfer, отрабатывает в пределах 90s timeout. Не false-fail'ит.
- **AC3.** GUI закрыт. `wd --exec "..."` falls back на direct serial — поведение **байт-в-байт** идентично сегодняшнему. Регрессии нет.
- **AC4.** Два параллельных `wd --exec` процесса (rare). Оба отрабатывают sequentially, sentinel'ы не пересекаются (UUID per call), exit-codes не путаются.
- **AC5.** GUI crash mid-running (kill -9). Следующий start GUI чистит stale socket file и rebind'ит без ошибки.
- **AC6.** Все 360 существующих тестов зелёные на `cargo test --workspace -- --test-threads=1`. Новые: IPC frame round-trip (mock UnixStream pair), GUI handler routing (mock serial writer mpsc), fallback в `wd --exec` (ENOENT path).

## Тестирование

**Unit:**
- IPC frame encode/decode — round-trip Request/Stdout/Exit через UnixStream pair (`UnixStream::pair()`).
- `connect_socket()` fallback — `ENOENT`/`ECONNREFUSED` → возвращает signal «no daemon», caller идёт в serial-path.
- `run_oneshot` через generic transport trait — verify same behavior на serial и на mpsc.

**Integration:**
- Spawn fake-GUI process (binding socket) → `wd --exec` ↔ fake → verify roundtrip.
- Stale socket: bind, drop без unlink → spawn опять → проверить что rebind'ится.

**Live (manual):**
- Реальный GUI + active capture + image-transfer mid-flight → `wd --exec` через Claude'а в чате.
- Live-replay существующих `wd --exec` use-case'ов из `docs/wd-exec-usage.md` — должны работать без изменений.

## Риски

- **Stale socket после crash** — mitigated `unlink`-перед-`bind` в acceptor init.
- **Concurrent `wd --exec` процессы** — single in-flight queue, max 4 queued (rare case всё равно).
- **Permissions** — socket file mode 0600 после bind (`chmod` syscall сразу после `bind`).
- **Backward compat для AC3** — критично: ни один сегодняшний `wd --exec` use-case не должен сломаться. Test plan покрывает.
- **Обнаружение что GUI fronzen (UI hang)** — IPC handler не respond'ит → клиентский timeout срабатывает, выдаёт diagnostic. Не наш immediate concern.

## Первые шаги

1. Создать crate `wiredesk-exec-core` (или модуль `wiredesk-protocol::exec`) — вытащить `run_oneshot` из `wiredesk-term::main`, параметризовать через trait `ExecTransport { send_command, recv_output_chunk, recv_exit }`.
2. В `wiredesk-term` имплементировать `SerialExecTransport` (текущее поведение через serial), переключить main на новый crate.
3. В GUI (`apps/wiredesk-client/src/`) добавить новый модуль `ipc.rs` — UnixListener acceptor thread, per-connection handler thread, frame codec. Mac-only (`cfg(target_os = "macos")`).
4. GUI handler имплементирует `MpscExecTransport` (через существующий outgoing_tx → serial writer; читает `ShellOutput` из reader'а через новый mpsc-channel).
5. В `wiredesk-term` добавить `try_socket_first()` ветку — UnixStream::connect, фейл → серийный fallback.
6. Поднять default timeout `wd --exec` 30 → 90 сек.
7. Тесты + live-test через Claude'а в чате с реальным GUI.

## Сложность

**Medium.** ~300 строк нового кода (IPC server, IPC client, generic exec-transport trait + 2 impl'а), ~150 строк тестов. Bóльшая часть — Unix socket boilerplate. Core sentinel-detection логика уже работает (в `wiredesk-term::run_oneshot`), просто переезжает.

## Связанные

- `docs/briefs/daemon-multiplex.md` — superseded этим брифом. Тот рассматривал full daemon extraction (~2-3 нед); scope не нужен потому что interactive `wd` параллельно не требуется.
- `docs/briefs/wd-exec-compression.md` — независимая задача, но complement: IPC + compression вместе делают `wd --exec` через активный GUI'ёвый клиент быстрым на больших dump'ах.
- `docs/briefs/ft232h-upgrade.md` — другая ось (speed, не concurrency).
- `apps/wiredesk-client/src/main.rs` — main thread берёт IPC acceptor; outgoing_tx и reader-thread без изменений.
- `apps/wiredesk-term/src/main.rs::run_oneshot` — переезжает в shared crate, обвязка остаётся.
- Memory: `feedback_serial_terminal_bridge.md` (правило «single-port ownership») — после реализации становится частично нерелевантным для `wd --exec`; для interactive `wd` остаётся.
