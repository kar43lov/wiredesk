# Plan: Auto-recovery канала при frame-error storm + Mac auto-reconnect

## Overview

Канал Mac↔Win умирает при «frame-error storm» — внезапном физическом сбое одного из FT232H (clock/сэмплирование уезжает, оба направления систематически корраптятся: COBS-ошибки на фиксированной позиции, CRC mismatch, бит-флип в magic `0x44`→`0x45`). Live-инцидент 2026-06-04: два эпизода (06:33 и 07:08 UTC), вылечено ~10 ручными перезапусками вслепую. Лечение известно: close + reopen serial-порта (реинициализация чипа) — но ни одна сторона этого не делает сама: Mac reader вечно дропает мусор, Win host уходит в `WaitingForHello`, не трогая порт.

Делаем: (1) storm-детект на обеих сторонах — N подряд Protocol-ошибок recv = канал мёртв; (2) Win host — reopen loop с backoff; (3) Mac client — полный in-process reconnect loop (закрывает заодно старый бриф `mac-auto-reconnect.md`: host quit, кабель, любой disconnect). Поскольку неизвестно, чей чип сбился, reopen обязателен на обеих сторонах: переоткрытие на стороне сбитого чипа чинит канал, вторая сторона переподключается через обычный Hello/HelloAck.

Бриф: `docs/briefs/serial-error-storm-recovery.md`. Детальная спека Mac reconnect: `docs/briefs/mac-auto-reconnect.md` (Вариант 1, superseded-заголовок).

## Context

- **Протокол/ошибки:** `crates/wiredesk-protocol/src/cobs.rs:33-72` (decode → UnexpectedZero/Truncated), `crates/wiredesk-protocol/src/packet.rs:91-140` (`from_bytes` → bad magic / CRC mismatch / unknown message type). Все всплывают как `WireDeskError::Protocol`.
- **Transport:** `crates/wiredesk-transport/src/serial.rs:75-133` (`recv()`, есть счётчик `partial_timeouts`), `transport.rs:4-14` (trait: send/recv/is_connected/name/try_clone — close/reopen НЕТ и не добавляем; reopen = drop + `open_transport` заново на app-уровне).
- **Mac client:** `apps/wiredesk-client/src/main.rs` — транспорт открывается в main (108-136, `open_transport` + `try_clone` → reader+writer пары), writer_thread спавн 212-224 (fn 522-625: шлёт Hello, heartbeat каждые 2s, на send-ошибку → `TransportEvent::Disconnected` + return), reader_thread спавн 286-305 (fn 637-815: на `Err(Protocol)` → `warn!("dropping bad frame")` + continue — строки 803-806, на прочие ошибки → Disconnected + return). Clipboard poll / IPC acceptor / synthetic dispatcher / keyboard tap транспортом НЕ владеют — шлют в `outgoing_tx` (клоны).
- **Mac UI:** `apps/wiredesk-client/src/app.rs:1399-1416` — обработка `TransportEvent::Disconnected`, `ConnectionState` enum, `status_text()` (1333-1351, pure, тестируется).
- **Mac IPC:** `apps/wiredesk-client/src/ipc.rs` — handler `wd --exec`, `single_inflight`, embedded runner.
- **Win host:** `apps/wiredesk-host/src/session_thread.rs:91-113` (спавн: `open_transport` один раз → `Session` → вечный tick-loop; 150-151 — `Err(Protocol)` → warn + continue), `apps/wiredesk-host/src/session.rs:221-264` (`tick()`: heartbeat send/timeout-check/recv/handle_packet; 231-241 — heartbeat timeout → `WaitingForHello`, порт не переоткрывается), 323-342 (Hello → HelloAck → Connected).
- **Паттерны проекта:** тесты в каждом таске; `cargo test --workspace -- --test-threads=1` (host-пакет флакает параллельно на macOS); clippy чистый на `-D warnings` + cross-target check на `x86_64-pc-windows-gnu` для Win-cfg кода; feature-ветка, мерж после live-теста.
- **Threshold-выбор:** 10 подряд Protocol-ошибок. Шторм даёт ≥2 ошибки каждые 2s (битые heartbeat'ы) — детект ≤10s; легитимные одиночные bad frames (логи 05-12, 05-28: 13-14 за день при подключениях) штормом не считаются, потому что перемежаются валидными пакетами (любой валидный пакет сбрасывает счётчик). Timeout/idle recv-ошибки счётчик НЕ инкрементят и НЕ сбрасывают — сброс только на успешно декодированный пакет.

## Validation Commands

- `cargo build --workspace`
- `cargo test --workspace -- --test-threads=1`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo clippy -p wiredesk-host --all-targets --target x86_64-pc-windows-gnu -- -D warnings`
- `cargo clippy -p wiredesk-client --all-targets --target x86_64-pc-windows-gnu -- -D warnings`

### Task 1: StormCounter в wiredesk-core

- [x] Создать `crates/wiredesk-core/src/storm.rs`: `pub struct StormCounter { consecutive: u32, threshold: u32 }` с методами `new(threshold: u32)`, `on_protocol_error(&mut self) -> bool` (инкремент; возвращает `true` когда `consecutive >= threshold` — шторм), `on_valid_packet(&mut self)` (сброс в 0), `count(&self) -> u32`. Doc-комментарий: что считается штормом, почему timeout'ы не участвуют (см. Context).
- [x] Добавить `pub const DEFAULT_STORM_THRESHOLD: u32 = 10;` там же.
- [x] Подключить модуль в `crates/wiredesk-core/src/lib.rs` (`pub mod storm;`).
- [x] Написать тесты в `storm.rs`: (1) threshold-1 ошибок → false, threshold-я → true; (2) сброс на `on_valid_packet` посреди серии; (3) после срабатывания продолжает возвращать true на следующих ошибках; (4) threshold=1 edge case.
- [x] Запустить `cargo test -p wiredesk-core` и `cargo clippy -p wiredesk-core -- -D warnings` — должны проходить.

### Task 2: Win host — storm-детект в Session + reopen loop в session_thread

- [x] В `apps/wiredesk-host/src/session.rs`: добавить поле `storm: StormCounter` в `Session` (инициализация `StormCounter::new(DEFAULT_STORM_THRESHOLD)` во всех конструкторах). Вызывать `self.storm.on_valid_packet()` СТРОГО в одном месте: после успешного `self.handle_packet(packet)?` в `tick()` (строка 262, путь `Ok(true)` где пакет реально декодирован). НЕ вызывать на других Ok-путях `tick()` — `Ok(false)` возвращается и на heartbeat-timeout (строка 240), и на recv-timeout (строка 257), там пакета нет, и сброс счётчика сломает правило «timeout не сбрасывает» (см. Context).
- [x] `tick()` сейчас возвращает `Err(WireDeskError::Protocol(...))` наружу (warn в session_thread). Добавить в `Session` метод `pub fn note_protocol_error(&mut self) -> bool` (делегирует `storm.on_protocol_error()`); вызывать его из `apps/wiredesk-host/src/session_thread.rs` в arm `Err(WireDeskError::Protocol(ref msg))` (строки 150-151): если вернул `true` — `log::warn!("frame-error storm detected ({} consecutive) — reopening transport", ...)` и выйти из tick-loop с маркером reopen.
- [x] Добавить в `Session` метод `pub fn into_injector(self) -> I` (разборка: возвращает injector, transport дропается внутри). Нужен потому, что `make_injector` — `FnOnce` (`session_thread.rs:89`, вызывается один раз), а `Session` владеет injector по значению (`session.rs:48-49`): при reopen старую Session разбираем, injector переезжает в новую.
- [x] В `apps/wiredesk-host/src/session_thread.rs:91-113`: injector создаётся ОДИН раз до цикла (он не зависит от порта); затем обернуть `open_transport → Session → tick-loop` во внешний `'reopen: loop`. При выходе из tick-loop по storm: `let injector = sess.into_injector();` (transport-handle COM-порта освобождается здесь), `thread::sleep(500ms)` (Win serialport close асинхронный — иначе reopen ловит `Access is denied`), затем retry `open_transport` с backoff 1s→2s→4s→8s→16s→30s (cap), лог каждой попытки `INFO reopening transport attempt=N`; на успех — новая `Session` с этим injector, продолжить tick-loop. Существующий путь «open failed на старте процесса» (status_tx + return) заменить тем же backoff-циклом, только status `Disconnected` оставлять между попытками.
- [x] Heartbeat-timeout путь (`session.rs:231-241`) НЕ менять — `WaitingForHello` на живом порту остаётся нормальным сценарием (Mac сам переподключится). Reopen — только по storm-сигналу.
- [x] Написать тесты в `session.rs`: (1) `note_protocol_error` возвращает true после 10 подряд; (2) валидный пакет между ошибками сбрасывает (использовать существующий тест-харнесс Session с MockTransport/MockInjector — посмотреть существующие тесты session и переиспользовать setup); (3) счётчик не сбрасывается на пустом tick'е без пакета (если это наблюдаемо через `count()`).
- [x] Запустить `cargo test -p wiredesk-host -- --test-threads=1` и `cargo clippy -p wiredesk-host --all-targets --target x86_64-pc-windows-gnu -- -D warnings` — должны проходить.

### Task 3: Mac client — LinkSupervisor (respawn reader/writer + reopen с backoff)

- [x] Создать `apps/wiredesk-client/src/link.rs` с struct `LinkContext` — контейнер всех shared-значений, переживающих реконнект и нужных reader/writer-потокам (все ~20 Arc/каналов из сигнатур `writer_thread` main.rs:522-531 и `reader_thread` main.rs:637-653: `clipboard_state`, `exec_slot`, `outgoing_progress`, `receive_*`, `*_cancel`, `current_outgoing_label`, и т.д.; `LinkContext: Clone` — все поля Arc/Sender). Это снимает будущий `too_many_arguments` на `spawn_link`.
- [x] Там же `fn spawn_link(reader_t: Box<dyn Transport>, writer_t: Box<dyn Transport>, outgoing_rx: Receiver<Packet>, events_tx: Sender<TransportEvent>, shutdown: Arc<AtomicBool>, ctx: LinkContext) -> LinkHandles` где `LinkHandles { writer: JoinHandle<Receiver<Packet>>, reader: JoinHandle<()> }`. Изменить `writer_thread` (main.rs:522-625) так, чтобы при выходе он **возвращал `outgoing_rx`** (канал переживает реконнект — клоны `outgoing_tx` у clipboard poll / IPC / keyboard tap остаются валидными).
- [x] Shutdown-флаг reader'а — ОБЯЗАТЕЛЕН (не опция): `reader_thread` проверяет `shutdown.load()` на каждой итерации recv-loop (в т.ч. после каждого recv-timeout) и выходит при true. Без этого supervisor зависнет на `join()` в сценарии «тихого» disconnect'а (host quit / unplug: `recv timeout` матчится раньше fatal-arm'а и continue'ится вечно — main.rs:802 vs 807-812).
- [x] В `reader_thread` (main.rs:637-815, arm `Err(Protocol)` на 803-806): добавить `StormCounter::new(DEFAULT_STORM_THRESHOLD)`; на Protocol-ошибку — если `on_protocol_error()` вернул true → `log::error!("frame-error storm ...")`, `events_tx.send(TransportEvent::Disconnected("frame-error storm — reopening port"))`, return. На каждый успешно полученный пакет — `on_valid_packet()`. Прочие arm'ы recv (timeout/idle) счётчик не трогают.
- [x] Реализовать в `link.rs` supervisor-поток `fn spawn_supervisor(...)`: принимает фабрику открытия `open_fn: impl FnMut() -> Result<Box<dyn Transport>> + Send` (в проде — замыкание над `open_transport(&cfg)`; в тестах — mock-фабрика) и слушает `reconnect_request_rx: Receiver<()>`; на запрос: (1) ставит `link_up: Arc<AtomicBool>` в false и `shutdown` в true, (2) join'ит старые `LinkHandles` (writer возвращает `outgoing_rx`, reader выходит по shutdown-флагу), (3) drop обоих transport-хэндлов, сброс `shutdown` в false, (4) цикл `open_fn()` с backoff 1s→2s→4s→8s→16s→30s cap, перед каждой попыткой `events_tx.send(TransportEvent::Reconnecting { attempt })` (новый вариант enum), (5) на успех — `try_clone`, `spawn_link(...)` с возвращённым `outgoing_rx`, `link_up = true`, drain накопившихся `reconnect_request_rx.try_recv()` (гасит дубли запросов за время цикла). Writer при старте шлёт Hello как сейчас — re-handshake происходит сам. (Backoff вынесен в pure `backoff_delay(attempt)` + инъекция `backoff_fn` ради тестируемости; shutdown — свежий `Arc<AtomicBool>` на каждый link.)
- [x] В `apps/wiredesk-client/src/main.rs`: создать `reconnect_request_tx/rx` и `link_up`, передать в supervisor; стартовое открытие транспорта переиспользует тот же supervisor-путь (первый «reconnect request» при запуске — устраняет дублирование кода открытия; сохранить текущее поведение «UI поднимается даже при failed transport» — см. memory `feedback_ui_recovery_on_transport_failure`).
- [x] В `apps/wiredesk-client/src/app.rs`: при обработке `TransportEvent::Disconnected` слать `reconnect_request_tx.send(())` (однократно на эпизод — guard от повторного запроса пока supervisor работает: `link_up == false && reconnect_in_flight` флаг или просто пусть supervisor игнорирует запросы во время работы, выбрав drain `try_recv` после завершения цикла). (Реализован drain-вариант: supervisor гасит дубли `try_recv` после успешного реконнекта.)
- [x] Добавить `TransportEvent::Reconnecting { attempt: u32 }` в enum (там же, где Connected/Disconnected определены) и пробросить компиляцию по всем match'ам.
- [x] Написать тесты в `link.rs`: (1) storm-логика reader'а с MockTransport, отдающим серию `Err(Protocol)` → проверить, что events канал получает Disconnected("frame-error storm…") ровно после threshold; (2) writer возвращает rx при выходе (отправить в outgoing_tx после смерти writer'а и убедиться, что сообщение доступно новому потребителю rx); (3) supervisor-цикл с mock `open_fn`, фейлящей первые N попыток → events получают серию `Reconnecting { attempt: 1..=N }` и затем Connected-путь (новый link поднят), `link_up` переходит false→true; (4) reader выходит по shutdown-флагу на тихом transport'е (Mock с вечным timeout). (Плюс: ниже-threshold не дисконнектит; валидный пакет сбрасывает storm.)
- [x] Запустить `cargo test -p wiredesk-client` и `cargo clippy -p wiredesk-client --all-targets -- -D warnings` — должны проходить.

### Task 4: Mac client — IPC во время reconnect

- [x] Добавить в enum `IpcResponse` (`crates/wiredesk-exec-core/src/ipc.rs`) НОВЫЙ вариант `TransportUnavailable(String)` — не перегружать существующий `Error` (правило расширения hand-rolled протоколов: новый opcode/вариант, не новая семантика старого). Оба конца IPC (`wiredesk-client` GUI и `wd`/`wiredesk-term`) собираются из одного workspace одной командой — lock-step совместимость обеспечена обычной пересборкой.
- [x] В `apps/wiredesk-client/src/ipc.rs`: пробросить `link_up: Arc<AtomicBool>` в IPC acceptor/handler. В начале обработки запроса (до захвата `single_inflight`/запуска runner'а): если `!link_up.load()` → немедленно ответить `IpcResponse::TransportUnavailable("transport reconnecting — retry shortly".into())` и закрыть соединение.
- [x] Term-side маппинг: ВАЖНО — сейчас `apps/wiredesk-term/src/main.rs:355-357` маппит любой `IpcResponse::Error` в exit 1; exit 125 (строки 471-479) живёт только на direct-open пути. Добавить arm для `IpcResponse::TransportUnavailable` → stderr `wd: transport reconnecting — retry shortly` + **exit 125** (transport-класс, AC3 брифа). Без этой правки AC3 не выполняется. (Реализовано через pure-хелпер `classify_terminal_response`.)
- [x] Написать тесты: (1) IPC handler с `link_up=false` возвращает `TransportUnavailable` без попытки писать в outgoing (mock/фейковый stream по образцу существующих ipc-тестов); (2) term-side: `TransportUnavailable` → exit 125 (unit на функцию маппинга, если она выделена; иначе выделить pure-хелпер).
- [x] Запустить `cargo test -p wiredesk-client -p wiredesk-exec-core` — должны проходить.

### Task 5: Mac client — UI-статус Reconnecting

- [x] В `apps/wiredesk-client/src/app.rs`: добавить `ConnectionState::Reconnecting` (хранить `reconnect_attempt: u32` полем приложения); обработать `TransportEvent::Reconnecting { attempt }` → state + attempt; `TransportEvent::Connected` уже сбрасывает в Connected.
- [x] Обновить `status_text()` (app.rs:1333-1351): для Reconnecting вернуть `"Reconnecting… (attempt N)"`.
- [x] Capture-mode: в `render_capture_overlays` banner-текст при `state != Connected` показывать «● HOST LINK LOST — reconnecting…» (жёлтый tint вместо красного), чтобы юзер в fullscreen видел потерю канала.
- [x] Обновить тесты `status_text()` (есть существующие unit-тесты pure-хелпера) — добавить кейс Reconnecting.
- [x] Запустить `cargo test -p wiredesk-client` и `cargo clippy -p wiredesk-client -- -D warnings` — должны проходить.

### Task 6: Полная верификация + документация

- [ ] Прогнать все Validation Commands (build, full test suite с `--test-threads=1`, clippy workspace, оба cross-target clippy) — всё зелёное.
- [ ] Обновить `CLAUDE.md`: краткий пункт про auto-recovery (storm-детект 10 подряд Protocol-ошибок → reopen на обеих сторонах; Mac reconnect loop с backoff 1s→30s; статус Reconnecting в UI) + актуализировать счётчик тестов в разбивке по крейтам (вырастут как минимум wiredesk-core за счёт нового `storm.rs`, client, host, exec-core/term).
- [ ] Обновить `README.md`: user-facing описание (канал самовосстанавливается; ручной перезапуск больше не нужен).
- [ ] В `docs/briefs/serial-error-storm-recovery.md` и `docs/briefs/mac-auto-reconnect.md` проставить SHIPPED-заголовки (по факту мержа — оставить TODO-маркер для post-merge).
- [ ] Финальный smoke: `cargo build --release --workspace` собирается; убедиться, что число тестов выросло и все проходят.
