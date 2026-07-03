# Бриф: интерактивный `wd` через GUI IPC — PTY-стрим параллельно с открытым GUI

**Status:** brainstorm закончен 2026-07-03, готов к `/planning:make`. Расширяет `wd-exec-via-gui-ipc.md` (SHIPPED) на **interactive**-кейс, который тот бриф явно вывел из scope. Закрывает последний путь в WireDesk, требующий терминирования GUI.

## Цель

Интерактивный `wd` (PowerShell PTY в Ghostty/iTerm) работает пока `WireDesk.app` запущен, **включая активный capture мыши/клавиатуры** — без Quit GUI и без контенции за serial-порт.

## Контекст: реальный workflow

GUI на Mac постоянно открыт (capture/clipboard sync). Сейчас единственный путь запустить интерактивный `wd` — Quit GUI (порт эксклюзивен). `wd --exec` эту боль уже обошёл через embedded-IPC (`wd-exec.sock`); интерактив — нет, потому что прошлый брейншторм посчитал его «параллельно не нужным». Теперь нужен: краевой, но реальный кейс «зашёл руками поковырять Host, пока GUI держит capture».

**Ключевой факт из research (2026-07-03):** на host'е инъекция ввода и shell — **ортогональные ветки одного tick-loop** (`session.rs handle_packet` match: `Mouse*`/`Key*` трогают только `injector`, `Shell*` — только `self.shell`). PTY-shell + активный capture сосуществуют **по построению**. Значит host менять не нужно — вся работа на Mac-стороне (term + GUI).

## Выбранный подход: `Packet`-релей поверх сокета (Approach A)

Интерактивный term и так гоняет по проводу ровно `Message`-протокол (`ShellOpenPty`/`ShellInput`/`PtyResize`/`ShellOutput`/`ShellExit`/`Heartbeat`). Вместо изобретения нового стримингового IPC-enum — GUI-сокет **прозрачно проксирует те же `Packet`'ы**:

- В term: `bridge_loop` уже работает над `Transport`-трейтом (`Arc<Mutex<Box<dyn Transport>>>` writer + `Box<dyn Transport>` reader). Добавляем `IpcStreamTransport: Transport` — `send(Packet)` пишет `Packet` в `wd-exec.sock`, `recv()` читает `Packet` из сокета. `bridge_loop` остаётся **байт-в-байт тем же** — меняется только чем инстанцируется transport.
- В GUI: новый per-connection стриминговый хэндлер-релей. Читает `Packet`'ы из сокета → форвардит shell-пакеты в `outgoing_tx` (writer-поток → провод → host). Устанавливает `exec_slot` → получает `ShellOutput`/`ShellExit`/`HostError` от reader-fanout'а → конвертит в `Packet` → пишет в сокет.
- Fallback: term пробует сокет (mirror `try_socket_first`), на `ENOENT`/`ECONNREFUSED`/timeout первого фрейма → текущий direct-open serial. Backward-compatible: GUI закрыт → поведение идентично сегодняшнему.

**Почему A, а не альтернативы:**
- **B (новый IPC-enum `InteractiveOpen`/`Frame`/`Out`/`Exit`)** — дублирует то, что уже есть в `Message`-протоколе, добавляет лишний слой перекодировки. Medium-High effort, ниже confidence.
- **C (full daemon extraction)** — прошлый брейншторм отверг как overkill (~2-3 нед); ничего не изменилось.

## Три решённые развилки (из брейншторма 2026-07-03)

1. **Политика конкуренции за единственный shell-слот хоста — fail-fast single-owner lock.** Host держит ровно один shell (`self.shell: Option<...>`; второй `ShellOpen*` → `Error "shell already open"`). Кто первым занял shell-канал (интерактив ИЛИ `--exec`), тот держит; второй **мгновенно** получает понятное «shell busy» → exit 125, без зависаний/очередей. Интерактив живёт минутами — очередь для `--exec` неприемлема (Claude завис бы на весь сеанс).
2. **GUI shell-панель — удаляется.** Мёртвый UI (не используется), делит тот же слот. Убираем: state-поля (`shell_open`/`shell_output`/`shell_input`/`shell_kind` в `app.rs`), UI-панель (`app.rs:1805-1875`), обработчики (`shell_open_request`/`shell_send_input`/`shell_close_request`), потребление `TransportEvent::Shell*` в `app.rs`. Reader-fanout в `events_tx` для shell-событий становится неиспользуемым — вычистить или оставить безвредным (решает planning).
3. **Heartbeat — за GUI.** Term-над-IPC свой heartbeat **не шлёт** (GUI-writer уже шлёт каждые 2с на реальный провод). `IpcStreamTransport`-путь подавляет отправку `Heartbeat`; GUI-релей дропает любой релеенный `Heartbeat` из сокета.

## Требования

**Функциональные:**
- `IpcStreamTransport: Transport` в `wiredesk-term` — `send`/`recv`/`try_clone` поверх `UnixStream` к `wd-exec.sock`, framing consistent с существующим IPC (length-prefixed; payload — сериализованный `Packet` через bincode ИЛИ COBS+CRC, решает planning ради consistency).
- Term: новая ветка перед `SerialTransport::open` в `run()` — `try_interactive_socket()` (mirror `try_socket_first`): connect → индикация interactive-mode → `bridge_loop` над `IpcStreamTransport`; на fail → fall through к direct serial.
- Индикация режима на сокете: term сообщает GUI «interactive» vs «exec» первым фреймом (новый вариант в IPC-хэндшейке или mode-байт; **не** ломать существующий `IpcRequest`-путь `--exec`).
- GUI: стриминговый релей-хэндлер. Hello от term → GUI отвечает **синтезированным** `HelloAck` из своего session-state (host_name/screen dims уже известны из GUI-хэндшейка), НЕ форвардит Hello на провод. `ShellOpenPty`/`ShellInput`/`PtyResize`/`ShellClose`/`Disconnect` → `outgoing_tx`. `Heartbeat` → drop. Shell-события из `exec_slot` → `Packet` → сокет.
- Shell-channel owner lock: расширить/переосмыслить `single_inflight` в shared owner-state (Idle | ExecBusy | InteractiveBusy). Интерактив claim'ит эксклюзивно fail-fast; `--exec` при InteractiveBusy — fail-fast «shell busy» (exit 125). `--exec`-vs-`--exec` — сохранить текущее поведение (короткие, FIFO допустимо).
- Ctrl+] / stdin-EOF в интерактиве → term шлёт `ShellClose`+`Disconnect` (как сейчас) через сокет; GUI релеит `ShellClose` на host, освобождает owner-lock, закрывает сокет-сессию.

**Нефункциональные:**
- **AC3-эквивалент критичен:** GUI закрыт → интерактивный `wd` через direct serial работает **байт-в-байт** как сегодня. Ноль регрессий.
- Латенси нажатий в интерактиве через IPC не хуже serial-пути на глаз (IPC — локальный Unix socket, накладные ~µs).
- Mac-only (`cfg(target_os = "macos")`), как existing IPC. Host (Win) не трогаем вообще.

## Acceptance criteria

- **AC1.** `WireDesk.app` запущен, capture **активен** (мышь/клава инжектятся на Host). `wd` в Ghostty подключается через IPC, открывает PowerShell PTY, интерактив работает (стрелки/Tab/vim), Ctrl+] выходит чисто. Мышь/клава в capture не дёргаются.
- **AC2.** Интерактивный `wd` активен → Claude стреляет `wd --exec "echo ok"` → мгновенный fail «shell busy» (exit 125), понятный stderr, БЕЗ зависания. И наоборот: `--exec` в полёте → интерактивный `wd` fail-fast с тем же сообщением.
- **AC3.** GUI закрыт → `wd` (интерактив) fallback на direct serial, поведение **идентично** сегодняшнему (handshake, PTY, resize, Ctrl+]). Регрессий нет.
- **AC4.** GUI-панель shell удалена: компилируется, `cargo clippy -D warnings` чист (нет dead-code/unused), UI не содержит shell-панели.
- **AC5.** Resize терминала (Ghostty окно) в IPC-режиме → `PtyResize` доходит до host'а, PowerShell перерисовывается.
- **AC6.** GUI mid-reconnect (`link_up=false`) во время интерактивной сессии → term видит понятный disconnect (не виснет), сессия завершается gracefully.
- **AC7.** Все существующие тесты зелёные на `cargo test --workspace -- --test-threads=1`. Новые тесты — см. ниже.

## Тестирование

**Unit:**
- `IpcStreamTransport` round-trip через `UnixStream::pair()` — `send(Packet)` на одном конце = `recv()` на другом, все shell-типы.
- `IpcStreamTransport::try_clone` — два хэндла на один сокет, независимый recv (mirror serial split).
- Interactive-mode индикация — GUI различает interactive vs exec первым фреймом; exec-путь не сломан.
- Owner-lock state machine — Idle→InteractiveBusy блокирует Exec (fail-fast); Idle→ExecBusy блокирует Interactive; освобождение на teardown; RAII на panic.
- GUI релей-логика (mock сокет + mock `outgoing_tx`/`exec_slot`): Hello→синтез HelloAck (не форвардится на провод); Heartbeat→drop; ShellInput/PtyResize→форвард; ShellOutput из slot→сокет.
- Term fallback: `ENOENT`/`ECONNREFUSED`/first-frame-timeout → `Ok(None)` → serial path.

**Integration:**
- Fake-GUI (bind сокет + релей) ↔ `wd` interactive: handshake → ShellOpenPty → ShellInput echo → ShellOutput → Ctrl+] teardown.
- Stale socket после crash — rebind (уже покрыто existing тестами, проверить что не сломано).

**Live (manual):**
- Реальный GUI + активный capture + fullscreen → `wd` в Ghostty параллельно, PowerShell, vim, Ctrl+].
- Параллельный `wd --exec` во время интерактива → fail-fast.
- GUI Quit во время интерактива и наоборот.

## Риски

- **Двойной shell-lifecycle owner:** интерактив держит shell минутами, `--exec` — секунды. Owner-lock должен корректно fail-fast, а не queue, иначе Claude виснет. Митигация: явная state-machine + тесты на обе стороны.
- **Синтез HelloAck на GUI:** GUI должен знать актуальные host dims (из своего HelloAck). Если ещё не хэндшукнулся (`link_up=false`) — интерактив должен fail-fast, не отдавать протухший HelloAck. Покрыто AC6.
- **Heartbeat suppression:** если term-над-IPC случайно шлёт heartbeat и GUI его форвардит → двойной на проводе. Тест на drop.
- **Backward-compat `--exec`:** новый interactive-путь не должен затронуть `IpcRequest`/`IpcResponse` one-shot протокол. AC3 + отдельный exec-регресс-тест.
- **exec_slot single-consumer:** интерактив переиспользует `exec_slot`. Т.к. owner-lock делает interactive и exec взаимоисключающими, контенции за slot нет — но это инвариант, который надо удержать (тест: нельзя install slot дважды).

## Первые шаги

1. `IpcStreamTransport` в `wiredesk-term` (impl `Transport` поверх `UnixStream`) + unit-тесты round-trip/try_clone.
2. Interactive-mode индикация в IPC-протоколе (`wiredesk-exec-core::ipc`) — расширить хэндшейк, не ломая exec-путь.
3. GUI стриминговый релей-хэндлер (`apps/wiredesk-client/src/ipc.rs` — новая ветка/модуль) + owner-lock state-machine.
4. Term: `try_interactive_socket()` перед `SerialTransport::open`, fallback на serial.
5. Удалить GUI shell-панель (`app.rs`), почистить unused reader-fanout.
6. Тесты (unit + integration) + live-прогон через реальный GUI + Ghostty.

## Сложность

**Medium (~4-6 дней).** Host не трогаем (главный источник риска исключён). `bridge_loop` transport-agnostic — переиспользуется целиком. Основной новый код: `IpcStreamTransport` (~120 строк), GUI релей-хэндлер (~200 строк), owner-lock state-machine (~80 строк), mode-индикация (~40 строк) + тесты. Удаление GUI-панели уменьшает net-diff.

## Связанные

- `docs/briefs/wd-exec-via-gui-ipc.md` — SHIPPED, этот бриф его продолжает на interactive-кейс.
- `docs/briefs/daemon-multiplex.md` — SUPERSEDED, рассматривал full-daemon (не нужен).
- `apps/wiredesk-term/src/main.rs` — `bridge_loop` (transport-agnostic), `try_socket_first` (паттерн для mirror).
- `apps/wiredesk-client/src/ipc.rs` — existing exec-хэндлер; `exec_bridge.rs` — `ExecEventSlot`/`ExecSlotGuard`.
- `apps/wiredesk-host/src/session.rs` — НЕ меняется (shell + injection уже ортогональны).
- Memory: `feedback_wd_interactive_session.md` (гочи порта/выхода), `feedback_serial_terminal_bridge.md` (single-port ownership — после реализации перестаёт применяться к WireDesk целиком).
