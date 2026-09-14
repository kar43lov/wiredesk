# Интерактивный `wd` и `wd --exec` одновременно — два shell-слота на хосте

> **SHIPPED в `main` 12.09.2026** — PR #30, merge-коммит `f1018e1` (код `7b7e618`,
> результаты приёмки `07ad442`). CI зелёный на macOS и Windows. Живая приёмка пройдена:
> медиана короткой команды **0.241 → 0.168 с**, выгрузка 407 КБ — 15.3 с соло против
> 61.2 с при открытой консоли. Полная таблица AC — в конце файла.
>
> **Долг закрыт 14.09.2026** (PR #31, `d5327da`): AC2, AC3, AC6 пройдены
> на пересобранном хосте — выгрузка 407 КБ теперь 1.60 с соло и 1.61 с при открытой консоли.
> ×4 оказался не разменом, а дефектом бюджета: он считался в чтениях, а не в байтах.
> Разбор — «Доприёмка 14.09.2026» в конце файла.
>
> **AC8 (RFCOMM) пройден 14.09.2026** с фиксом `fix/rfcomm-exec-flow-control`: без него эхо
> под выгрузкой ждало 0.84 с — Bluetooth-стек копил очередь, и бюджет такта ничего не
> дозировал. Лечение — подтверждения приёма `RxProgress`; эхо стало 0.127 с. Раздел «AC8».

## Overview

Сегодня у хоста ровно один shell-слот (`session.rs:58` `shell: Option<ShellProcess>`), а на
проводе ни один shell-опкод не несёт адресата. Поэтому клиент держит политику fail-fast
(`shell_channel.rs`): пока открыт интерактивный `wd`, любой `wd --exec` получает
`shell busy` → exit 125, и наоборот. Практически это значит: пока владелец сидит в консоли
хоста, агент не может выполнить ни одной команды.

Цель — снять взаимоисключение так, чтобы **`wd --exec` не стал медленнее ни на миллисекунду,
а канал не стал менее стабильным**. Не «сделать мультиплексор», а расшить одно узкое место
минимальным числом новых понятий.

**Ключевое решение:** не `session_id` в payload, а **два фиксированных слота** — `exec`
(pipe-mode, `wd --exec`) и `pty` (ConPTY, интерактивный `wd`). Больше двух шеллов хосту не
нужно. На проводе это пять новых опкодов для PTY-слота; ни один существующий опкод не меняет
payload (правило из `feedback_binary_protocol_extension`: новый опкод, не расширять старый).

**Совместимость в обе стороны — обязательное требование, не бонус.** Хост на Windows
обновляется руками и регулярно отстаёт от Mac (12.09.2026 — шесть коммитов). Новый клиент со
старым хостом обязан вести себя ровно как сегодня (fail-fast, exit 125), а не ронять линк:
неизвестный опкод на старом хосте — это `Protocol`-ошибка, десять подряд — переоткрытие
порта (`session_thread.rs:186`, `DEFAULT_STORM_THRESHOLD = 10`). Старый клиент с новым
хостом тоже должен работать — это путь отката Mac-стороны без похода к Windows-машине.

## Context (изучено 12.09.2026)

- **Хост** — `apps/wiredesk-host/src/session.rs`: `shell: Option<ShellProcess>` (21 место
  использования), `warm: Option<(argv, ShellProcess)>` (прогретый pipe-шелл для exec),
  `heartbeat_timeout_for(clipboard_busy, shell_open)` (:32, 30 с при открытом шелле),
  `pump_shell_events` (:338 — до `MAX_PER_TICK = 16` чанков по ≤4096 байт **блокирующим**
  `transport.send` за тик), `shell_kill` (:386), `take_warm_or_spawn` (:402), арм-обработчики
  `ShellOpen` (:544), `ShellOpenPty` (:571), `PtyResize` (:594), `ShellInput` (:602),
  `ShellClose` (:610 — kill + ack `ShellClosed`), `Disconnect` (:640), re-`Hello` (:650),
  heartbeat-timeout в `tick` (:259). `ShellProcess` (`shell.rs`): `spawn(requested,
  pty: Option<(cols, rows)>)`, `events_rx`, `write`, `close`, `resize`, `try_exit_code`,
  `kill`, `drain_pending`. Spill-файлы уже per-instance (`shell.rs:425`, UUID в имени) —
  два pipe-шелла не мешают друг другу. Глобального состояния в `shell.rs` нет.
- **Протокол** — `crates/wiredesk-protocol/src/message.rs`: опкоды 0x40–0x47 заняты
  (`ShellOpen`…`ShellClosed`), `ShellOpenPty = 0x45`, `PtyResize = 0x46`. `VERSION = 1`;
  хост отвергает `Hello` с `version != VERSION` (`session.rs:446`), `HelloAck.version`
  клиент **игнорирует** (`link.rs:581` связывает `..`). Payload'ы: `ShellInput`/`ShellOutput`
  — сырые байты; `ShellExit` — `i32 LE`; `ShellOpenPty` — `[cols u16][rows u16][string]`.
  `needs_ack` только у clipboard.
- **Клиент, приём** — `apps/wiredesk-client/src/exec_bridge.rs`: `ExecEventSlot =
  Arc<Mutex<Option<mpsc::Sender<ExecEvent>>>>` — ровно **один** подписчик; `ExecSlotGuard`
  RAII; `broadcast_exec_event`. `link.rs:660–680` reader кладёт `ShellOutput`/`ShellExit`/
  `Error{msg содержит "shell"}` в этот слот; `link.rs:465` `transfer_in_flight` считает
  «шелл открыт» по `exec_slot.is_some()` (расширяет recv-таймаут до 30 с). `HostInfo`
  (`link.rs:56`) — `host_name/screen_w/screen_h`, заполняется в арме `HelloAck` (:598).
- **Клиент, политика** — `shell_channel.rs`: `ChannelState { exec_refs, interactive }`,
  `try_acquire(owner, kind)`: `Interactive` только при полном простое, `Exec` — если нет
  interactive (стек по счётчику). Кросс-вид fail-fast — **это и есть запрет**.
- **Клиент, IPC** — `ipc.rs`: exec-хендлер `handle_connection` (:300+): `try_acquire(Exec)`
  → keepalive → `single_inflight` (FIFO exec-vs-exec) → `ExecSlotGuard::install` →
  `ShellOpen` → `run_oneshot` → `ShellClose` → post-run drain (:534–600, ждёт
  `ShellClosed`/`ShellExit` или тишины в **своём** rx). Интерактивный релей
  `handle_interactive_connection` (:705+): `try_acquire(Interactive)` → синхронный
  `Hello`/synth-`HelloAck` → **сам** порождает `ShellOpenPty` (:783) → reader-поток
  форвардит от term'а только `ShellInput`/`PtyResize` (:845), `ShellClose`/`Disconnect`
  **не** форвардит; главный памп пишет в сокет `ShellOutput`/`ShellExit`/`Error` (:881–915);
  teardown шлёт один `ShellClose` (:939). `spawn_ipc_acceptor` (:128) получает
  `exec_slot`, `shell_owner`, `single_inflight`, `host_info`, `link_up`; проводка в
  `main.rs:189–218`.
- **Term** — `apps/wiredesk-term/src/main.rs`: говорит только `Hello`/`HelloAck`/`ShellOpen`/
  `ShellOpenPty`/`ShellInput`/`ShellOutput`/`ShellExit`/`ShellClose`/`PtyResize`/`Heartbeat`/
  `Disconnect`/`Error`. Через IPC он не порождает `ShellOpenPty` (это делает релей). **Term
  в этом плане не меняется** — вся трансляция опкодов живёт в релее.
- **Тесты, закрепляющие запрет** (переписать, не удалять): `shell_channel.rs`
  `second_acquire_cross_kind_fails_fast`, `exec_acquires_stack_and_channel_stays_exec_until_last_release`;
  `ipc.rs` `exec_refused_when_interactive_holds_channel` (:1755),
  `interactive_refused_when_channel_busy` (:1579), `concurrent_exec_fifo_no_false_busy` (:1828);
  `interactive_ipc_e2e.rs` `e2e_second_interactive_connect_is_busy` (:262). Хост:
  `shell_open_pty_on_non_windows_returns_error_to_client` (:1061),
  `pty_resize_without_shell_is_silent_noop` (:1031), `has_shell()` (:198).
- **Скорости канала** (для бюджета пампа): serial FT232H 3 Mbaud ≈ 300 КБ/с, RFCOMM ≈ 120 КБ/с,
  BLE 4–5 КБ/с. Один чанк 4096 байт = 14 мс / 34 мс / ~1 с. Сегодняшние 16 чанков за тик =
  64 КБ = 218 мс / 533 мс / 13 с блокировки, в течение которых `recv` не вызывается.
- **Базовая производительность** (`project_wd_exec_latency`): `wd --exec` ≈ 0.14 с на команду
  после PR #26. Это число нельзя ухудшить.

## Development Approach

- Код, затем тесты — в стиле крейтов; каждая задача заканчивается тестами на успех и на
  ошибочный/краевой путь.
- **Каждая задача завершается зелёным `cargo test --workspace` и чистым
  `cargo clippy --workspace -- -D warnings` + `cargo fmt --check`.** Параллельный прогон
  тестов на маке с 11.09.2026 стабилен (`feedback_macos_test_thread_flake`), `--test-threads=1`
  не нужен.
- **Платформенный код проверять обеими кросс-командами** (CLAUDE.md): 
  `cargo clippy -p wiredesk-client --target x86_64-pc-windows-gnu --all-targets -- -D warnings`
  и `cargo build -p wiredesk-client --target x86_64-pc-windows-gnu`; хост —
  `cargo clippy -p wiredesk-host --target x86_64-pc-windows-gnu --all-targets -- -D warnings`.
  `link.rs` и `exec_bridge.rs` собираются под Windows; `ipc.rs`/`shell_channel.rs` —
  macOS-only по `cfg`, но их типы кросс-платформенные (см. атрибуты в файлах).
- **Перед началом — `rustup update stable`** (CI на 1.98; локальное отставание уже роняло
  `main`, `feedback_rust_clippy_version_skew_cross`).
- Порядок задач подобран так, что **после каждой задачи дерево деплоябельно**: хост с
  новыми опкодами, но старым клиентом ведёт себя как сегодня; клиент с новым роутингом, но
  старой политикой — тоже.
- Ничего не коммитить и не пушить без явной просьбы владельца. Хост-крейт меняется →
  после мержа Windows-хост пересобирать по `docs/setup.md` «Обновление host'а на Windows»
  с тройной проверкой (HEAD / `LastWriteTime` exe / `StartTime` процесса).

## Testing Strategy

- **unit**: протокол (roundtrip новых опкодов, `try_from`, ошибки длины); хост —
  `Session` с `MockTransport`/`MockInjector` и фикстурой `setup()` (`session.rs:677`)
  (на маке PTY-спавн недоступен: тестировать роутинг через pipe-шелл `/bin/sh` в exec-слоте
  и через `Error`-ответ в pty-слоте, а порядок пампа — чистой функцией без процессов);
  клиент — `shell_channel`, `exec_bridge` (роутинг по двум слотам с fallback), `ipc.rs`
  (`UnixStream::pair()` + staged events, как сейчас).
- **integration**: `interactive_ipc_e2e.rs` (`FakeGui`) — новый сценарий «интерактив + exec
  одновременно в dual-режиме» и «legacy-режим = сегодняшний fail-fast».
- **live** (Post-Completion, без чекбоксов): реальный Mac + Ghostty + Win11-хост, замеры до/после.

## Progress Tracking

- отмечать выполненное `[x]` сразу; новые задачи — `➕`, блокеры — `⚠️`.
- держать план в соответствии с реальной работой.

## Solution Overview

### Провод

Пять новых опкодов, все — только для **не-legacy** PTY-слота:

| Опкод | Код | Payload | Направление | Зеркало |
|---|---|---|---|---|
| `PtyOpen` | 0x48 | как у `ShellOpenPty`: `[cols u16 LE][rows u16 LE][shell string]` | client→host | `ShellOpenPty` |
| `PtyInput` | 0x49 | сырые байты | client→host | `ShellInput` |
| `PtyOutput` | 0x4A | сырые байты | host→client | `ShellOutput` |
| `PtyClose` | 0x4B | пусто | client→host | `ShellClose` (ack **не** нужен — релей его и сегодня игнорирует) |
| `PtyExit` | 0x4C | `i32 LE` | host→client | `ShellExit` |

`PtyResize` (0x46) остаётся общим для legacy- и нового PTY-слота. `ShellOpenPty` (0x45)
**остаётся** и означает «legacy PTY»: хост открывает pty-слот с флагом `legacy = true` и
общается на нём старыми опкодами (`ShellInput`/`ShellOutput`/`ShellExit`/`ShellClose`), а
exec-слот при этом отказывает `"shell already open"` — ровно сегодняшнее поведение. Так
старый клиент (и direct-serial режим term'а, когда GUI закрыт) работает с новым хостом без
единого изменения.

Коды `Message::Error` от хоста (сегодня: 1 — версия, 2 — `shell already open`, 3 — spawn):
добавить **4 — `pty shell already open`**, **5 — `pty shell spawn: …`** (в т.ч. «PTY-mode shell is
only supported on Windows host»). В тексте обоих есть слово `shell`: тест `ipc.rs:1728`
проверяет его наличие в сообщении интерактивного отказа. Клиент маршрутизирует `Error` по коду, а не по подстроке
`"shell"` в тексте. Legacy-PTY-слот продолжает отвечать кодами 2/3.

### Согласование версий

`VERSION = 1` в `Hello` **не меняется** — клиент всегда шлёт 1, хост принимает 1. Новая
константа `HOST_PROTO_VERSION: u8 = 2` уходит в `HelloAck.version`; клиент **впервые читает**
это поле и кладёт в `HostInfo.proto_version`. `pty_slot_supported(v) = v >= 2`. Только при
`true` клиент использует `PtyOpen`/`PtyInput`/`PtyClose` и dual-политику; со старым хостом
(`HelloAck.version == 1`) остаётся `ShellOpenPty` + сегодняшний fail-fast. Синтетический
`HelloAck` для term'а (`SYNTH_HELLO_ACK_VERSION`) не трогать — term поле игнорирует.

### Хост

`shell: Option<ShellProcess>` → два поля:

```rust
exec: Option<ShellProcess>,            // pipe-mode, wd --exec; берёт warm
pty:  Option<PtySlot>,                 // struct PtySlot { proc: ShellProcess, legacy: bool }
```

Роутинг входящих (`handle_packet`):
- `ShellOpen` → exec; отказ `Error 2`, если `exec.is_some()` **или** `pty` открыт как legacy.
  Литералы `code: 2`/`3` сегодня в `session.rs:548,563,575,585`.
- `ShellOpenPty` → pty с `legacy = true`; отказ `Error 2`, если `pty.is_some()` **или**
  `exec.is_some()` (старый клиент не умеет два слота — сохраняем его картину мира).
- `PtyOpen` → pty с `legacy = false`; отказ `Error 4`, если `pty.is_some()`; `exec` не
  мешает. Ошибка спавна → `Error 5`.
- `ShellInput` → exec; если `exec` пуст, а `pty` legacy — в pty (старый клиент).
- `PtyInput` → pty (только не-legacy; иначе игнор + warn).
- `PtyResize` → pty любого вида; нет pty → тихий no-op (как сегодня).
- `ShellClose` → закрыть exec (close + kill + `ShellClosed`, если был); если `exec` пуст, а
  `pty` legacy — закрыть pty и ответить `ShellClosed` (сегодняшняя семантика).
- `PtyClose` → закрыть pty (close + kill), без ack.
- `Disconnect`, re-`Hello`, heartbeat-timeout → убить **оба** слота и warm.

Исходящие (`pump_shell_events`): exec-слот → `ShellOutput`/`ShellExit`; pty legacy →
`ShellOutput`/`ShellExit`; pty не-legacy → `PtyOutput`/`PtyExit`.

**Справедливость канала** (главный риск по скорости):

```rust
const PUMP_BUDGET_PTY: usize = 16;          // интерактив качается первым, объёмы мизерные
const PUMP_BUDGET_EXEC_ALONE: usize = 16;   // как сегодня — пропускная способность не меняется
const PUMP_BUDGET_EXEC_SHARED: usize = 4;   // пока открыт pty: 16 КБ/тик = 55 мс serial, 136 мс RFCOMM
```

Памп обходит слоты в порядке `[pty, exec]`; бюджет exec выбирается по `pty.is_some()`. Без
pty поведение и цифры **байт в байт как сегодня**. Вынести выбор бюджета в чистую функцию
`exec_pump_budget(pty_open: bool) -> usize` и порядок — в чистую `pump_order()` для тестов
без процессов.

`heartbeat_timeout_for(clipboard_busy, shell_open)` — `shell_open = exec.is_some() ||
pty.is_some()`. `has_shell()` (test-only) → `has_exec_shell()` + `has_pty_shell()`.

### Клиент

- **`exec_bridge.rs`**: `ExecEventSlot` остаётся типом одного слота. Новый
  `ShellSlots { exec: ExecEventSlot, pty: ExecEventSlot }` (`Clone`, оба `Arc`) и
  `route(&ShellSlots, SlotKind, ExecEvent)`: доставить в целевой слот; **если он пуст и
  `slots.legacy_fallback` установлен — во второй, если тот установлен** (legacy-хост стримит
  PTY старыми опкодами, а legacy-политика гарантирует ровно один установленный слот); иначе
  drop. `legacy_fallback: AtomicBool` выставляет reader в арме `HelloAck` как
  `!pty_slot_supported(version)` и сбрасывает при потере линка. 🔴 В dual-режиме fallback
  **запрещён**: запоздалый `ShellExit`/`ShellClosed` от exec-слота (drain упёрся в 30-с cap,
  хост ответил позже) при пустом exec-слоте улетел бы в pty-слот, а интерактивный релей на
  `ShellExit` **завершает сессию** (`ipc.rs:890`) — консоль владельца умерла бы от чужого
  хвоста.
  `ExecSlotGuard::install` не меняется — просто зовётся на нужном слоте.
- **`link.rs`**: `HostInfo.proto_version: u8` из `HelloAck.version` (арм :581 перестаёт
  связывать `..`). Reader: `ShellOutput`/`ShellExit`/`ShellClosed` → `route(Exec)`;
  `PtyOutput`/`PtyExit` → `route(Pty)`; `Error{code: 2|3}` → `route(Exec, HostError)`,
  `Error{code: 4|5}` → `route(Pty, HostError)`, прочие коды — только лог.
  `transfer_in_flight` — «любой из двух слотов установлен». `LinkContext.exec_slot` →
  `shell_slots: ShellSlots`.
- **`shell_channel.rs`**: `try_acquire(owner, kind, dual: bool)`. При `dual == false` —
  сегодняшняя логика без изменений. При `dual == true`: `Interactive` отказывает только
  другому `Interactive`; `Exec` не смотрит на `interactive`. `exec_refs`-стек и `single_inflight`
  остаются как есть. Тесты на обе матрицы.
- **`ipc.rs`**: `dual = host_info.proto_version >= 2` вычисляется в обоих хендлерах **в момент
  acquire** (после проверки `link_up`/`host_info_ready`) и не меняется в течение сессии.
  Exec-хендлер: только `try_acquire(.., dual)` и `install` на `slots.exec`; всё остальное —
  `ShellOpen`, drain, `ShellClosed` — без изменений (это гарантия «не медленнее»).
  Интерактивный релей: `install` на `slots.pty`; при `dual` порождает `PtyOpen`, reader
  переписывает `ShellInput`→`PtyInput` (`PtyResize` как есть), teardown шлёт `PtyClose`;
  при `!dual` — сегодняшние `ShellOpenPty`/`ShellInput`/`ShellClose`. Входящие в сокет
  term'а пишутся как сегодня (`ShellOutput`/`ShellExit`/`Error`) — term ничего не знает.
  `spawn_ipc_acceptor` принимает `ShellSlots` вместо `ExecEventSlot`; `main.rs` — проводка.

### Что сознательно НЕ делается

- Приоритеты в исходящей очереди клиента (`outgoing_tx` — один FIFO): exec шлёт мало
  (команда ≤ единиц КБ; длинная 48 КБ = 160 мс на serial), нажатия встанут за ней на доли
  секунды. Замеряется в Post-Completion; если мешает — отдельный follow-up с двумя очередями
  у writer'а.
- Больше одного интерактива и больше одного слота на exec (exec-vs-exec остаётся FIFO).
- GUI-индикация двух слотов.
- Изменения в `wiredesk-term`.

## Technical Details

- Константы кодов ошибок вынести в `message.rs`: `ERR_VERSION = 1`, `ERR_SHELL_BUSY = 2`,
  `ERR_SHELL_SPAWN = 3`, `ERR_PTY_BUSY = 4`, `ERR_PTY_SPAWN = 5` — и использовать их на обеих
  сторонах вместо литералов (сегодня литералы `code: 2`/`3` в `session.rs`).
- `PtySlot::legacy` выбирается **только** по опкоду открытия. Никаких эвристик по версии
  клиента: хост версии клиента не знает (Hello всегда 1).
- В `handle_packet` для `ShellInput` при `exec == None && pty.legacy` — доставить в pty, но
  при `exec == None && pty` не-legacy — **дропнуть с warn**: это пакет старого протокола от
  клиента, который уже открыл новый pty; смешивать нельзя.
- `pump_shell_events` не должен держать `&self.pty` и `&mut self` одновременно — собрать
  выходы в локальные `Vec`, как сделано сейчас, отдельно для каждого слота.
- Fallback-роутинг — единственное место, где режим (legacy/dual) нужен reader'у; всё
  остальное решается по опкоду. Оба флага — `legacy_fallback` у слотов и `dual` у хендлеров —
  берутся из одного источника (`HostInfo.proto_version`), расходиться не могут.
- `ExecEvent` (`wiredesk-exec-core/src/types.rs`) не меняется: pty-слот получает те же
  `ShellOutput`/`ShellExit`/`HostError` — релей и `run_oneshot` не различают источник.
- В `link.rs` reader ветка `Message::Error` больше не проверяет `msg.contains("shell")` —
  только код. Проверить, что нет других потребителей этой подстроки (`/usr/bin/grep -rn
  'contains("shell")' apps/`).
- Логи: хост при открытии pty пишет `opening pty shell '…' ({cols}x{rows}, legacy={bool})`;
  клиент при acquire — `shell channel: dual={bool} (host proto v{N})` один раз на сессию
  линка (не на каждую команду — 950 exec'ов за три недели в логах, `feedback_log_template_mining`).

## Implementation Steps

> Порядок без forward-зависимостей: протокол (1) → хост (2) → клиент приём/роутинг (3) →
> политика (4) → IPC-хендлеры + проводка (5) → e2e (6) → кросс-проверки (7) → доки (8).
> После задач 1–2 хост деплоябелен со старым клиентом; после 3–4 клиент деплоябелен со
> старым хостом (поведение = сегодняшнее).

### Task 1: Протокол — пять опкодов, версия хоста, коды ошибок

**Files:** `crates/wiredesk-protocol/src/message.rs`

- [x] `MessageType`: `PtyOpen = 0x48`, `PtyInput = 0x49`, `PtyOutput = 0x4A`, `PtyClose = 0x4B`,
      `PtyExit = 0x4C` + `try_from`.
- [x] `Message`: `PtyOpen { shell, cols, rows }`, `PtyInput { data }`, `PtyOutput { data }`,
      `PtyClose`, `PtyExit { code }`; `msg_type`, `serialize`, `deserialize` — зеркально
      существующим; `needs_ack` — `false`.
- [x] `pub const HOST_PROTO_VERSION: u8 = 2;` + `pub fn pty_slot_supported(host_version: u8)
      -> bool`. `VERSION` не трогать. Док-комментарий: почему `Hello` остаётся 1.
- [x] Константы `ERR_*` (см. Technical Details).
- [x] Тесты: roundtrip каждого нового сообщения (пустой и максимальный payload для
      `PtyInput`/`PtyOutput`, отрицательный код в `PtyExit`), `try_from(0x48..=0x4C)`,
      `try_from(0x4D)` → ошибка, `PtyOpen` с пустым `shell`, `pty_slot_supported(1) == false`,
      `(2) == true`, `(3) == true`. Расширить `message_type_pty_opcodes_roundtrip` (`message.rs:697`) новыми кодами.

### Task 2: Хост — два слота, legacy-флаг, роутинг, справедливый памп

**Files:** `apps/wiredesk-host/src/session.rs`

- [x] `PtySlot { proc, legacy }`; поля `exec`/`pty`; все 21 использование `self.shell`
      переразнесены по таблице роутинга из Solution Overview.
- [x] `HelloAck.version = HOST_PROTO_VERSION`.
- [x] `exec_pump_budget(pty_open) -> usize`, `pump_shell_events` обходит `[pty, exec]`,
      pty-слот выбирает опкод вывода по `legacy`.
- [x] `heartbeat_timeout` по обоим слотам; `shell_kill` → `kill_all_shells` (exec + pty);
      `Disconnect`/re-`Hello`/heartbeat-timeout зовут его.
- [x] `Error` — константы `ERR_*` вместо литералов; новые ветки 4/5.
- [x] Тесты (все — с pipe-шеллом `/bin/sh` в exec и `Error` в pty, т.к. PTY на маке
      недоступен; там, где нужен «открытый pty», подменять через `#[cfg(test)]`-хелпер
      `inject_pty_for_test(legacy: bool)`, который кладёт в pty-слот pipe-процесс):
  - `exec_and_new_pty_coexist`: `ShellOpen` + `PtyOpen` → оба открыты, `has_exec_shell` и
    `has_pty_shell`; `ShellClose` закрывает только exec, pty жив; `PtyClose` — наоборот.
  - `legacy_pty_still_excludes_exec`: `ShellOpenPty` (через inject legacy=true) + `ShellOpen`
    → `Error 2 "shell already open"`; и `ShellOpen` + `ShellOpenPty` → `Error 2`.
  - `pty_open_twice_is_error_4`; `pty_open_on_non_windows_is_error_5` (переписать
    `shell_open_pty_on_non_windows_returns_error_to_client`: legacy-путь остаётся с кодом 3,
    новый — 5).
  - `shell_input_falls_back_to_legacy_pty_only`: при пустом exec и legacy pty `ShellInput`
    доходит до pty; при не-legacy pty — дропается (проверять через события/лог-заглушку или
    отсутствие вывода).
  - `shell_close_closes_legacy_pty_with_ack`: `ShellClose` при пустом exec и legacy pty →
    pty закрыт, `ShellClosed` отправлен; при не-legacy pty — ничего не закрыто, ack нет.
  - `pump_order_and_budget_pure`: `pump_order() == [Pty, Exec]`, `exec_pump_budget(false) == 16`,
    `exec_pump_budget(true) == 4`.
  - `pty_output_uses_pty_opcodes_when_not_legacy` и `..._uses_shell_opcodes_when_legacy`
    (через inject + запись байт в pipe-процесс и чтение первого пакета).
  - `heartbeat_busy_with_either_slot`; `re_hello_kills_both_slots`;
    `heartbeat_timeout_kills_both_slots`.
  - Обновить `pty_resize_without_shell_is_silent_noop` и warm-shell тест (warm по-прежнему
    только для exec).
- [x] `cargo clippy -p wiredesk-host --target x86_64-pc-windows-gnu --all-targets -- -D warnings`.

### Task 3: Клиент — два слота приёма и роутинг в reader

**Files:** `apps/wiredesk-client/src/exec_bridge.rs`, `apps/wiredesk-client/src/link.rs`,
`apps/wiredesk-client/src/main.rs` (только тип поля)

- [x] `ShellSlots`, `SlotKind { Exec, Pty }`, `route(..)` с fallback-правилом; док-комментарий
      объясняет, почему fallback безопасен только в legacy-режиме.
- [x] `HostInfo.proto_version`; арм `HelloAck` читает `version`, лог `connected to '…' (WxH,
      proto v{N})`.
- [x] Reader: таблица роутинга по опкодам и кодам ошибок; убрать `msg.contains("shell")`.
- [x] `transfer_in_flight` по обоим слотам; `LinkContext.exec_slot` → `shell_slots`.
- [x] Тесты `exec_bridge`: `route_to_installed_target`, `route_falls_back_to_other_slot_when_
      target_empty`, `route_drops_when_both_empty`, `route_strict_when_both_installed`
      (событие для Pty не попадает в exec и наоборот), `route_no_fallback_in_dual_mode`
      (exec пуст, pty установлен, `legacy_fallback == false` → `ShellExit` дропается),
      панические/RAII-тесты — без изменений.
      Тесты `link.rs`: `reader_routes_pty_output_to_pty_slot`, `reader_routes_error_by_code`
      (2→exec, 4→pty, 1→никуда), `hello_ack_version_is_cached`.
- [x] Обе Windows-кросс-команды для `wiredesk-client`.

### Task 4: Клиент — dual-политика владения каналом

**Files:** `apps/wiredesk-client/src/shell_channel.rs`

- [x] `try_acquire(owner, kind, dual)`; документация в шапке модуля переписана: legacy vs dual.
- [x] Тесты: существующие переименовать в `*_legacy` и вызывать с `dual = false` (поведение
      неизменно); новые с `dual = true`: `dual_exec_while_interactive_ok`,
      `dual_interactive_while_exec_ok`, `dual_second_interactive_still_busy`,
      `dual_exec_stack_still_counts`, `drop_releases_in_dual`.

### Task 5: IPC-хендлеры, релей, проводка

**Files:** `apps/wiredesk-client/src/ipc.rs`, `apps/wiredesk-client/src/main.rs`

- [x] `spawn_ipc_acceptor(.., slots: ShellSlots, ..)`; `main.rs:189–218`.
- [x] Exec-хендлер: `dual` из `host_info`, `try_acquire(.., dual)`, `install(&slots.exec, ..)`.
      Больше ничего — сверить `git diff` этой функции: не должно быть изменений в `ShellOpen`,
      `run_oneshot`, drain.
- [x] Интерактивный релей: `dual` из `host_info` (после проверки готовности); `install(&slots.pty,
      ..)`; открытие `PtyOpen`/`ShellOpenPty`; reader-поток: `ShellInput` → `PtyInput` при
      `dual`; teardown `PtyClose`/`ShellClose`. Комментарий в шапке функции (:671–700)
      актуализировать.
- [x] Тесты `ipc.rs`: `exec_refused_when_interactive_holds_channel` → `..._legacy` (host_info
      с `proto_version: 1`) + новый `exec_runs_while_interactive_holds_channel_dual`
      (`proto_version: 2`; проверить, что на «провод» ушли `PtyOpen` и затем `ShellOpen`, а
      staged `PtyOutput`-событие в pty-слот не попало в exec-rx); `interactive_refused_when_
      channel_busy` — остаётся (interactive-vs-interactive), плюс `interactive_ok_while_exec_
      running_dual`; `interactive_relay_speaks_pty_opcodes_when_dual` (originate `PtyOpen`,
      term'овский `ShellInput` уходит как `PtyInput`, `PtyResize` как есть, teardown —
      `PtyClose`) и `..._speaks_legacy_opcodes_when_host_v1`; `concurrent_exec_fifo_no_false_busy`
      — гонять в обоих режимах.
- [x] Обе Windows-кросс-команды (ipc.rs под `cfg(macos)`, но сигнатуры/типы в `main.rs` общие).

### Task 6: e2e через `FakeGui`

**Files:** `apps/wiredesk-client/src/interactive_ipc_e2e.rs`

- [x] `FakeGui::spawn_with_proto(version)`; существующие тесты — на `1` (поведение неизменно).
- [x] `e2e_interactive_and_exec_concurrently_dual`: открыть интерактив (ждать `PtyOpen` на
      проводе), затем `IpcConnect::Exec` → на проводе `ShellOpen` + `ShellInput` с sentinel'ом,
      staged `ShellOutput` с `__WD_DONE_…__0` → exec получает `Exit(0)`, **интерактив при этом
      жив**: staged `PtyOutput` доходит до term-сокета как `ShellOutput`; teardown интерактива
      → `PtyClose`; owner → `Idle`.
- [x] `e2e_second_interactive_connect_is_busy` — гонять на обоих `version`.
- [x] `e2e_exec_refused_while_interactive_legacy` (version 1) — сегодняшний контракт закреплён
      явно, чтобы откат Mac-стороны не сломал его молча.

### Task 7: Кросс-проверки и CI

- [x] `cargo fmt`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace |
      /usr/bin/grep -E "^test result|^error|FAILED"` (не `tail` — он показывает только
      doc-тесты последнего крейта). Записать итоговое число тестов (было 904).
- [x] Четыре кросс-команды (`clippy` + `build`, client и host, `x86_64-pc-windows-gnu`).
- [x] `gh workflow run ci.yml --ref <branch>` и дождаться `gh run watch` — feature-ветка сама
      не линтуется на Windows.
- [x] `/pg.review` (Claude → Codex → pi). Ожидаемо в выводе Codex 15 падений тестов
      буфера обмена/сокетов — это его песочница (`feedback_codex_sandbox_test_failures`),
      сверять своим прогоном.

### Task 8: Документация

- [x] `docs/known-limitations.md`: убрать «единственный shell-слот», добавить два пункта —
      (а) **порядок обновления: сначала Mac, потом хост** — новый клиент со старым хостом
      работает по-старому, старый клиент с новым хостом тоже; (б) интерактив под тяжёлым
      `wd --exec` делит канал (цифры бюджета), на BLE это заметно.
- [x] `CLAUDE.md` (индекс ограничений) — одна строка вместо строки про PTY-mode/слот.
- [x] `README.md:281`, `docs/setup.md:231`, `docs/wd-exec-usage.md:18` и таблица exit-кодов
      `:99` — `shell busy` теперь только interactive-vs-interactive или старый хост.
- [x] `docs/architecture.md`: `:95` число типов сообщений (пересчитать по факту), раздел
      «Shell-over-serial» `:248–256` — два слота, legacy-флаг, таблица опкодов, версия в
      `HelloAck`; `:283` heartbeat по обоим слотам; `:284` drain — теперь по своему слоту.
- [x] `docs/run.md:91` — уточнить «параллельно».

## Post-Completion (live, руками владельца и агента)

**Сначала базовые замеры на текущем `main`, до обновления хоста** — иначе сравнивать не с чем:

1. `for i in $(seq 10); do /usr/bin/time -p wd --exec 'echo hi' 2>&1 | grep real; done` —
   медиана. Ожидание после: **не хуже** (цель ±0.01 с).
2. `wd --exec` с выводом ≥400 КБ (`1..8000 | % { "line $_ " + ("x" * 40) }`) — время до
   `Exit`.

**Порядок обновления:** собрать и запустить новый Mac-клиент **при старом хосте** →
проверить, что `wd`, `wd --exec` и их взаимный `shell busy` ведут себя как сегодня, а в
`host.log` нет `unknown message type` → только потом пересобрать хост (`docs/setup.md`,
тройная проверка).

**Приёмка на новом хосте:**

- AC1 — интерактивный `wd` открыт, в нём набирается текст; параллельно `wd --exec 'echo hi'`
  возвращает `hi` и exit 0, консоль не прерывается. Повторить 30 раз циклом — 0 отказов.
- AC2 — интерактив открыт; `wd --exec` из п. 2 (≥400 КБ). Эхо нажатий в консоли остаётся
  живым (субъективно < 300 мс на serial); heartbeat-таймаута и реконнекта в логах нет;
  время exec не хуже п. 2 более чем на 10 %.
- AC3 — обратное: в интерактиве `1..100000 | % { $_ }`; параллельно `wd --exec 'echo hi'`
  укладывается в 2 с.
- AC4 — `wd` второй раз при открытом первом → `shell busy` (exit 1), как сегодня.
- AC5 — Ctrl+] в интерактиве при живом exec → консоль вышла, exec дошёл до конца, следующий
  `wd` открывается сразу.
- AC6 — GUI закрыт: `wd` через direct serial (legacy `ShellOpenPty`) работает с новым хостом.
- AC7 — п. 1 повторно: медиана не хуже базовой.
- AC8 — RFCOMM (если под рукой): AC1–AC3, ожидание — те же результаты с поправкой на канал.

Результаты (числа до/после) записать в шапку этого файла при переносе в
`docs/plans/completed/`.

### Результаты приёмки 12.09.2026 (Mac M4 ↔ Win11, serial 3 Мбод)

Хост пересобран владельцем на `7b7e618`, сверено тремя фактами (HEAD, время файла exe,
StartTime процесса — время старта позже времени файла).

| AC | Итог | Факт |
|---|---|---|
| порядок обновления | ✅ | новый клиент против старого хоста: `HELLO … v1`, ноль `unknown message type` |
| AC1 | ✅ | `wd --exec` ×3 при открытой консоли, все rc=0; консоль не прервалась |
| AC2 | ⚠️ **частично** | эхо под 407-КБ выгрузкой — 0.50 с (живое, критерий взят); **но время exec 61.22 с против 15.32 с соло — ×4, а критерий требовал ≤10 %** |
| AC3 | ⛔ не проверялся | тяжёлый вывод в консоли + параллельный exec |
| AC4 | ✅ | второй интерактив → `shell busy`, rc=1 |
| AC5 | ✅ | Ctrl+] освободил слот, следующий `wd` открылся сразу |
| AC6 | ⛔ не проверялся | direct-serial при закрытом GUI |
| AC7 | ✅ **лучше базовой** | медиана короткой команды 0.241 с → **0.168 с** (попутно исправлен дефект: клиент не разбирал `ShellClosed`) |
| AC8 | ⛔ не проверялся | RFCOMM |

Дополнительно проверено: `connected to 'wiredesk-host' (2560x1440, proto v2)`;
`opening pty shell '' (120x40, legacy=false)` без ошибок спавна; защитные ветки
(`PtyInput with no dedicated pty shell — dropped`, `PtyClose … ignored`) отработали.

По AC2 владелец решил чинить — см. ниже.

### Доприёмка 14.09.2026 (хост пересобран на `4226907`)

Причина ×4 оказалась глубже урезанного бюджета. Бюджет такта считался в **чтениях** из
шелла, а PowerShell, печатающий построчно, отдаёт одну строку ≈50 байт на чтение: такт
отправлял меньше килобайта, и скорость определялась частотой тактов (~32/с за 10-мс
таймаутом `recv`), а не проводом. Тот же объём одной строкой шёл в 12 раз быстрее
(200 КБ: 31.0 с строками против 2.5 с куском, замер до фикса). Отношение бюджетов 16:4
и дало ровно ×4.

Фикс (`session.rs`): чтения склеиваются и режутся на полные пакеты, бюджет в байтах —
64 КБ соло, 16 КБ рядом с консолью; урезание — только если за последние 2 с у консоли был
ввод, resize или вывод. Замеры на serial 3 Мбод, harness на `pty.fork`:

| Проверка | Было (12.09) | Стало |
|---|---|---|
| 407 КБ соло | 15.3 с | **1.60 с** |
| 407 КБ при открытой молчащей консоли (AC2) | 61.2 с | **1.61 с** |
| 4.15 МБ соло / при непрерывном наборе | — | 15.2 с / 18.4 с (+22 %, консолью пользуются) |
| эхо под выгрузкой | 0.50 с | медиана **0.108 с**, max 0.190 с; первое нажатие после паузы 0.095 с |
| короткая команда соло / рядом с консолью (AC7) | 0.168 с | 0.160 с / 0.163 с |
| AC1: 30 × `echo hi` при открытой консоли | ×3 | **30/30**, эхо между ними ≤ 0.064 с |
| AC3: `echo hi` при `1..100000 \| % { $_ }` в консоли | не проверялся | **0.12 / 0.22 / 0.11 с** (критерий ≤ 2 с) |
| AC6: GUI закрыт, прямой serial | не проверялся | ✅ `HELLO from 'wiredesk-term'`, `legacy=true`, эхо ≈0.05 с, команда выполнилась |

AC2 по букве теперь выполнен: при молчащей консоли разница 0.6 %. AC8 (RFCOMM) не
проверялся. Гоча приёмки AC6: `osascript -e 'quit app "WireDesk"'` закрыл GUI, но через
4 с он поднялся снова, и первый прогон ушёл через IPC-релей (`legacy=false`). Закрытость
GUI подтверждается логом хоста, а не `pgrep` сразу после `quit`.

### AC8 — RFCOMM (14.09.2026)

Первый прогон, на `main` после #31: функционально всё работало (407 КБ за 3.68 с, AC1 30/30,
AC3 ≤ 0.22 с), но эхо консоли под выгрузкой 4.15 МБ — медиана **0.842 с**, max 1.18 с, а сама
выгрузка при наборе шла 36.6 с, как соло (36.7 с): троттлинг не срабатывал вовсе. Причина —
`send` на RFCOMM возвращается, как только байты принял Bluetooth-стек Windows, и тот копит
~95 КБ; бюджет такта ограничивал скорость наполнения этой очереди, а не провод. До #31 exec
давал ~27 КБ/с, очередь не набиралась, и проблема была не видна.

Фикс (ветка `fix/rfcomm-exec-flow-control`): клиент на буферизующем транспорте при открытой
консоли раз в ≤50 мс шлёт `RxProgress` (0x4D, поколение хоста 3) — сколько байт shell-вывода
декодировал; хост держит exec-вывода «в пути» не больше 16 КБ. Хост и Mac пересобраны, замеры
— тот же harness, переключение хоста на RFCOMM задачей планировщика с автооткатом на serial:

| Проверка (RFCOMM) | До фикса | После |
|---|---|---|
| эхо под выгрузкой 4.15 МБ | медиана 0.842 с, max 1.18 с | медиана **0.127 с**, p90 0.195 с, max **0.247 с** |
| 4.15 МБ соло / при непрерывном наборе | 36.7 с / 36.6 с | 37.4 с / 38.8 с (+4 %) |
| 407 КБ соло / при молчащей консоли | 3.68 с / — | 3.82 с / 3.79 с |
| короткая команда соло / рядом с консолью | — | 0.156 с / 0.164 с |
| AC1: 30 × `echo hi` при открытой консоли | 30/30 | 30/30, эхо ≤ 0.133 с |
| AC3: `echo hi` при `1..100000 \| % { $_ }` в консоли | ≤ 0.22 с | 0.14 / 0.28 / 0.15 с |

Serial на том же хосте не изменился: 407 КБ 1.61 с соло и рядом с консолью, 4.15 МБ при
наборе 18.5 с с эхом 0.110 с, AC1 30/30, AC3 0.11–0.22 с.

Ctrl+C под выводом самой консоли (`1..3000000 | % { … }`, прерывание через 5 с): на RFCOMM
вывод идёт ещё 20 с (2.29 МБ), на serial 4.8 с (1.36 МБ). Прерывание доходит сразу — всего
до остановки консоль выдала 2.85 МБ против 2.74 МБ на serial, — а хвост это уже
накопленный хостом вывод шелла, который канал отдаёт со своей скоростью. Отчёты `RxProgress`
перед Ctrl+C не копятся: хост читает их вне своего одного пакета за такт.

Гоча harness'а: новая консоль через ~200 мс после закрытия предыдущей получает
`shell busy` — GUI-релей отпускает слот на следующем витке своего 100-мс цикла. Человек в это
окно не попадает; скрипту между сценариями нужна пауза.
