# Бриф: Daemon mode — одновременная работа GUI + `wd --exec`

**Status:** не начат, в roadmap. Активируется когда живой workflow реально требует обеих сторон одновременно.

## Проблема

Сейчас `WireDesk.app` (GUI) и `wd --exec` (CLI) **взаимоисключающие** — оба процесса хотят open()'нуть один и тот же serial-порт, OS-level эксклюзивный. Workflow ломается так:
- GUI открыт, активный capture, мышь/клавиатура работают на Host.
- В терминале нужно быстро отстрелить `wd --exec --ssh prod-mup "..."` для триажа.
- Quit GUI → запустить `wd --exec` → дождаться → перезапустить GUI → восстановить fullscreen + Cmd+Esc engage. Каждый цикл — 30+ секунд.

`docs/wd-exec-usage.md` пункт 1 это явно документирует. `feedback_serial_terminal_bridge.md` (memory) — section 5 «Single-port ownership».

## Подходы

### A. Daemon-mode (recommend)

Выделить процесс `wiredesk-daemon`, который владеет serial-портом и держит `Session`. GUI (`wiredesk-client`) и CLI (`wiredesk-term`) становятся IPC-клиентами к daemon'у через Unix socket (`~/Library/Application Support/WireDesk/daemon.sock`). Daemon ставит heartbeat, owns clipboard sync, маршрутизирует ShellOpen/ShellInput/ShellOutput между клиентами.

**Pros:**
- Множественные клиенты одновременно. GUI capture'ит ввод, term делает `--exec` параллельно.
- Чистый shape: heartbeat / session / clipboard sync — в одном месте.
- Открывает дорогу для других клиентов: TUI, status menu и т.д.

**Cons:**
- Большой refactor: ~2-3 недели. Затрагивает `apps/wiredesk-client/src/main.rs` (5 потоков → IPC client), `apps/wiredesk-term/src/main.rs` (3 потока → IPC client + локальная терм-логика остаётся).
- Daemon lifecycle: launchd plist на macOS, autostart, restart-on-crash — всё новое.
- IPC protocol — новый layer (либо переиспользовать существующий `Message` enum поверх Unix socket, либо JSON-RPC).
- Mac-only пока (Win-host остаётся как есть; daemon — только на client-стороне).

### B. Расширение GUI shell-panel

GUI уже имеет Settings → Terminal collapsing с command line. Расширить его до `--exec`-эквивалента: addr'есс input → submit → сентинел-driven execution через тот же serial → output в textarea. Поддержать `--ssh ALIAS` / `--timeout N` как параметры в той же UI.

**Pros:**
- Effort ~3-5 дней.
- Никаких изменений в архитектуре — GUI остаётся primary owner.

**Cons:**
- Не Ghostty/iTerm — это egui textarea без полноценного ANSI rendering, без zsh-history через стрелки, без alias `wd`, без pipe-friendly (`wd --exec ... | grep`).
- `--exec` теряет свой smysl — он сделан именно как Bash-tool drop-in для AI-агентов. Через GUI это другая UX.

### C. Time-sharing handover

Костыль: hotkey в GUI «release port for 60s», CLI ловит окно. Не решает «одновременно», просто механизирует cycle. Не делать.

## Рекомендация

**A (daemon)** — правильный shape. Активировать когда workflow реально требует одновременности (сейчас это hypothetical: пользователь только что закончил активный wd --exec triage и работает в основном через GUI capture). Если боль появится — браться за daemon.

**B (GUI panel)** — fallback если активность параллельной работы вдруг поднимется, но daemon-effort всё ещё too much. Не запускаем превентивно.

## Acceptance criteria (для будущего planning'а)

- **AC1.** `WireDesk.app` запущен, capture активен. `wd --exec --ssh prod "uname -a"` в Ghostty в это же время — отрабатывает за ~3 сек, exit 0, мышь/клавиатура в GUI не дёргаются.
- **AC2.** Параллельные `wd --exec` (e.g. два terminal'а одновременно) — оба отрабатывают, sentinel'ы не путаются (UUID per call защищает).
- **AC3.** Daemon-crash: client'ы видят disconnect, переподключаются после daemon-restart без потери session state (best-effort — heartbeat seq может перезапуститься).
- **AC4.** Старая команда `wiredesk-host.exe` на Win-стороне работает без изменений. Daemon — только Mac-side.
- **AC5.** `cargo test --workspace -- --test-threads=1` зелёный, все 359+ существующих тестов проходят. Новые tests для IPC layer'а (mock socket, message-routing).

## Не в scope

- Win-host тоже не daemonized (там уже tray-agent, и единственный client — serial driver наш).
- Кросс-машинный daemon (Linux client тоже через daemon) — Mac-only пока.
- TUI-client как отдельный пользовательский case — daemon делает это возможным, но строить TUI не в scope.
- Compression / wire optimization — отдельная тема (FT232H upgrade).

## Связанные

- `docs/briefs/ft232h-upgrade.md` — апгрейд канала ×100 (другая ось — speed, не concurrency).
- `apps/wiredesk-client/src/main.rs` — 5 потоков сейчас, после daemon'а станет 2 (UI + IPC).
- `apps/wiredesk-term/src/main.rs::run_oneshot` — sentinel-driven exec; в daemon-mode логика не меняется, только transport переезжает с serial на IPC.
- Memory `feedback_serial_terminal_bridge.md` (section «Single-port ownership») — это правило перестанет применяться к WireDesk после A; останется применимо для других non-PTY bridge'ей.

## Сложность

**Medium-high.** Без architectural surprises (паттерн известный — Docker daemon, Mosh server, и т.д.), но широкий blast radius: GUI client, term client, autostart на macOS, daemon-lifecycle на crash/upgrade.
