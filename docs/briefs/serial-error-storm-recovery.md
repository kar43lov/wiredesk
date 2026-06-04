# Бриф: auto-recovery канала при frame-error storm + Mac auto-reconnect

**Status:** ready for /planning:make. Branch: `feat/error-storm-recovery`.

**Поглощает** `docs/briefs/mac-auto-reconnect.md` (Mac reconnect loop — компонент 2 этого брифа; старый бриф остаётся как детальная спека компонента, помечен superseded).

## Контекст: live-инцидент 2026-06-04

Дважды за утро (06:33 и 07:08 UTC) канал Mac↔Win внезапно умер с симметричным паттерном:

- Обе стороны одновременно начинают дропать **все** фреймы друг друга: `COBS decode: unexpected zero byte at position 11/12/16`, `CRC mismatch`, `bad magic: 5745`.
- `bad magic 5745`: протокольный magic `0x57 0x44` («WD») пришёл как `0x57 0x45` («WE») — **один бит-флип**. Corruption физическая.
- Одинаковые heartbeat-пакеты бьются систематически **на одной и той же позиции** — не случайный шум; характерно для сбоя clock/сэмплирования одного из FT232H (его TX даёт мусор peer'у, его RX неправильно читает peer'а — поэтому страдают оба направления сразу).
- 1–3 июня на том же сетапе — **ноль** битых фреймов. Шторм — внезапное событие, не деградация.

**Recovery сегодня:** ~10 ручных перезапусков клиента и host'а вслепую (включая `COM5 Access is denied` — старый host-процесс ещё держал порт). Канал ожил в момент, когда рестарт host'а **переоткрыл порт** → реинициализация FTDI-чипа.

**Поведение софта при шторме (обе стороны):**
- Mac client: `reader_thread` логирует `dropping bad frame` и крутится вечно (`apps/wiredesk-client/src/main.rs:803-806`). Disconnect-события нет, reopen'а нет.
- Win host: `session_thread` так же дропает (`apps/wiredesk-host/src/session_thread.rs:150-151`); `Session::tick` делает `heartbeat timeout — disconnecting` → `WaitingForHello` (`apps/wiredesk-host/src/session.rs:231-241`), **но порт не переоткрывает** — продолжает читать мусор тем же handle'ом.

Ни одна сторона не выздоравливает сама, хотя лечение известно и тривиально: close + reopen serial-порта.

## Цель

Канал самовосстанавливается за секунды без участия пользователя:

1. **Storm-детект (обе стороны):** N подряд `Protocol`-ошибок при recv → признать канал умершим.
2. **Win host:** при storm-детекте или heartbeat-timeout с активным штормом — close transport + reopen loop (backoff) + возврат в `WaitingForHello` на свежем порту.
3. **Mac client:** полный reconnect loop по спеке `mac-auto-reconnect.md` (Вариант 1, in-process): Disconnected-event от reader/writer **или** storm-детект → teardown threads → reopen loop с backoff → respawn reader/writer → новый Hello → Connected. UI-статус «Reconnecting…».

Заранее неизвестно, чей чип сбился — поэтому reopen обязателен на **обеих** сторонах: переоткрытие на стороне сбитого чипа чинит канал, вторая сторона переподключается через обычный Hello/HelloAck.

## Архитектурные точки (из исследования 2026-06-04)

| Компонент | Файл | Что менять |
|---|---|---|
| Storm-счётчик client | `apps/wiredesk-client/src/main.rs:803-806` (`reader_thread`) | consecutive Protocol errors; reset на успешный пакет; на threshold → `TransportEvent::Disconnected("frame-error storm")` + return |
| Storm-счётчик host | `apps/wiredesk-host/src/session_thread.rs:150-151` | аналогично; на threshold → выйти из tick-loop в reopen-цикл |
| Host reopen loop | `apps/wiredesk-host/src/session_thread.rs:91-113` | вокруг `open_transport` + `Session` цикл: при выходе по storm/fatal — drop transport, retry open с backoff 2s→30s, новая Session |
| Mac reconnect loop | `apps/wiredesk-client/src/main.rs` (main/UI thread) | спека в `mac-auto-reconnect.md`: ReconnectController, respawn reader/writer (`main.rs:212-224`, `286-305`), re-Hello |
| Transport | `crates/wiredesk-transport/src/serial.rs` | **не трогать** trait: reopen = drop + `open_transport` заново на app-уровне. `try_clone`-механика остаётся |
| IPC во время reconnect | `apps/wiredesk-client/src/ipc.rs` | `IpcResponse::Error("transport reconnecting")` вместо unexpected EOF (AC2 старого брифа) |
| UI | `apps/wiredesk-client/src/app.rs:1399-1416` | состояние `Reconnecting { attempt }` в status-bar; в capture-mode — banner message |

**Threshold:** ~10 подряд Protocol-ошибок (шторм даёт 2 ошибки каждые 2s от heartbeat'ов — детект за ~10s; при плотном потоке — за миллисекунды). Меньше нельзя: одиночные CRC-промахи случаются легитимно (см. 05-12 и 05-28: 13–14 bad frames без шторма — вероятно при подключении/отключении). Ошибки должны быть именно **подряд** — любой успешный фрейм сбрасывает счётчик.

**Безопасность от reopen-петли:** если порт после reopen сразу же штормит снова — backoff (2s→4s→…→30s cap), не busy-loop. Лог каждой попытки.

## Acceptance criteria

1. **AC1 (storm recovery, главный):** симуляция шторма (см. «Как тестировать») → обе стороны логируют storm-детект → host переоткрывает COM-порт, Mac переоткрывает /dev/cu.usbserial → канал восстанавливается **без единого ручного действия** ≤ 60s.
2. **AC2 (host quit/restart):** tray Quit host → 30s → запуск host → Mac client сам проходит Reconnecting → Connected (AC1 старого брифа).
3. **AC3 (in-flight wd --exec):** во время reconnect `wd --exec` получает `IpcResponse::Error("transport reconnecting")`, exit 125; следующий запрос после восстановления проходит.
4. **AC4 (UI):** status-bar «Reconnecting… (attempt N)»; в capture-mode banner с сообщением о потере канала.
5. **AC5 (no false positives):** одиночные bad frames (1–5 подряд) НЕ триггерят recovery; обычная работа (clipboard, мышь, shell) не прерывается.
6. **AC6 (UI alive):** reconnect loop не блокирует UI; Settings открываются, Quit работает.
7. **AC7 (regression):** 634 теста проходят; `wd --exec` через GUI IPC и legacy direct-open работают как раньше.

## Как тестировать storm без железного сбоя

- Unit: счётчик-логика (reset на успех, trigger на threshold) — pure-функции, оба приложения.
- Integration: MockTransport, отдающий поток `Err(Protocol)` → проверить переход в recovery.
- Live-симуляция: открыть порт сторонним процессом и слать мусор нельзя (порт занят), поэтому: (а) выдернуть/воткнуть USB одного FT232H — Mac увидит read error, host увидит тишину → heartbeat timeout; (б) на время теста переставить baud одной стороны (3M vs 2M в config) — гарантированный постоянный COBS-мусор на обеих сторонах = честная симуляция шторма. После проверки вернуть.

## Риски

- **Respawn-механика Mac:** clipboard poll / IPC acceptor / keyboard tap НЕ владеют transport'ом (шлют в `outgoing_tx`) — их не трогаем; пересоздаются только reader/writer. Проверить, что старый writer умер до открытия нового порта (иначе два владельца порта → `Resource busy` на Mac).
- **Host: COM Access denied** при reopen, если handle не освобождён — drop(transport) строго до retry-open; учесть, что serialport close на Win асинхронный (добавить малый sleep между drop и open).
- **Race с clipboard transfer:** disconnect посреди передачи — `clipboard.reset()` уже есть на host'е; на Mac проверить сброс in-flight состояния при reconnect.
- **Reopen не чинит** (чип сбит так, что и reopen не помогает): backoff-петля крутится бесконечно с редкими попытками; UI honest «Reconnecting», юзер может перетыкать USB — после physical re-plug петля сама подцепит порт. Это правильное поведение, не failure.

## Сложность

**medium-high**, ~2-3 дня. Mac reconnect loop — основная масса (по старому брифу), storm-детект и host reopen — по ~0.5 дня.

## Что НЕ входит

- Поиск первопричины сбоя FT232H (EMI/питание/USB suspend) — отдельная hardware-история; софт должен выживать при любом её исходе.
- Снижение baud — не нужно (3 дня чистой работы на 3M; storm — событие, не постоянный шум).
- BLE transport — без изменений.
- Live-reload config при reconnect — config применяется только на restart (как сейчас).

## Связанное

- `docs/briefs/mac-auto-reconnect.md` — детальная спека Mac reconnect loop (Вариант 1), поглощена этим брифом.
- `feedback_wd_exec_timeout_channel_hang.md` (memory) — прошлый класс channel-hang, FIXED PR #20/#21; storm — другой класс (физический).
- Live-логи инцидента: `client.log.2026-06-04` (Mac), `host.log.2026-06-04` (Win) — 06:33–06:50 и 07:08–07:10 UTC.
