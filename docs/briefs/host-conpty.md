# Бриф: ConPTY на host (proper TTY для interactive `wd`)

**Цель:** в interactive `wiredesk-term` host'овский shell живёт в настоящем TTY — vim/htop/ssh без `-tt`/PSReadLine prompt с history+Tab autocomplete работают как через нативный ssh.

## Контекст

`apps/wiredesk-host/src/shell.rs::ShellProcess::spawn` сейчас стреляет `std::process::Command` с `Stdio::piped()`. Это **не TTY**. Любой child видит stdin как pipe и идёт в non-interactive ветки:

- `ssh dev` (без `-tt`) → "Pseudo-terminal will not be allocated", `.bashrc` не грузится, нет alias'ов и цветов;
- `git commit` (без `-m`) — vim editor падает / висит;
- `vim`, `htop`, `less`, `sudo` — рендерят неправильно;
- PSReadLine на host'е выключен — стрелка вверх и Tab autocomplete недоступны.

Только что merged'нутый `wd --exec` (PR #9) специально pipe-based — sentinels с UUID требуют чистого stdout без TTY escape sequences. ConPTY его не должен затрагивать.

## Выбранный подход

**A + resize сразу.** Per-session toggle `pty: bool` в `ShellOpen` + новое `Message::PtyResize { cols, rows }`. Host'овский `ShellProcess::spawn` per-session выбирает `portable-pty` (`pty=true`) или текущий `Stdio::piped()` (`pty=false`).

- `wd` (interactive `bridge_loop`) → `pty=true` + переключается в **pass-through raw** (никакого cooked-mode local echo).
- `wd --exec` (`run_oneshot`) → `pty=false`. **0 регрессий** относительно master'а.
- GUI shell-panel в `wiredesk-client` → `pty=false` (egui без ANSI parser'а; ConPTY для GUI — отдельная задача вне scope'а).

**Почему не B (всегда ConPTY):** перепиcать sentinel-detection в `--exec` под TTY-output (PSReadLine echoes, bracket-paste sequences вокруг payload'а, ANSI цвета) — high risk регрессий PR #9, который только что прошёл AC1-AC8 на железе.

## Требования

### Protocol (`crates/wiredesk-protocol`)
- `Message::ShellOpen { shell, pty: bool }` — добавить поле; serde default `false` для backward compat.
- `Message::PtyResize { cols: u16, rows: u16 }` — новый message-type.

### Host (`apps/wiredesk-host/src/shell.rs`)
- `Cargo.toml`: добавить `portable-pty = "0.9"`.
- `ShellProcess::spawn(requested, pty: bool)` — если `pty=true`:
  ```rust
  let pty_system = portable_pty::native_pty_system();
  let pair = pty_system.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;
  let cmd = portable_pty::CommandBuilder::new(...);
  let child = pair.slave.spawn_command(cmd)?;
  drop(pair.slave);
  let reader = pair.master.try_clone_reader()?;
  let writer = pair.master.take_writer()?;
  ```
  Иначе — текущий `Stdio::piped()` flow без изменений.
- `ShellProcess::resize(cols, rows)` — no-op для pipe-mode, `master.resize(PtySize { ... })` для PTY.
- На Win + ConPTY: `CREATE_NO_WINDOW` уже не нужен (ConPTY сам не показывает console).

### Client terminal (`apps/wiredesk-term/src/main.rs`)
- `bridge_loop`: шлёт `ShellOpen { pty: true }`. **Удаляет** cooked-mode line discipline (`line_buf`, `line_cells`, local-echo BS-erase, `\n`→`\r\n` translate). Каждый byte stdin → writer без обработки. Host'овский TTY echoes сам.
- `bridge_loop`: после ShellOpen ack — считает initial `cols×rows` через `crossterm::terminal::size()` и шлёт `PtyResize`. Опционально SIGWINCH-handler / periodic poll для resize'а во время сессии.
- `run_oneshot`: шлёт `ShellOpen { pty: false }` — без изменений.

### GUI client (`apps/wiredesk-client/src/main.rs`)
- Shell-panel: шлёт `ShellOpen { pty: false }` — без изменений.

## Acceptance criteria

| # | Критерий |
|---|---|
| AC1 | `wd` (без `--exec`) → `vim /tmp/test.txt` → edit + `:q!` без артефактов |
| AC2 | `wd` → `ssh dev` (без `-tt`) → remote `.bashrc` загружен, alias'ы + цвета работают; `exit` → host PS |
| AC3 | `wd` → стрелка вверх в host PS → last command подставлен (PSReadLine) |
| AC4 | `wd` → `git commit` (без `-m`) → vim editor работает |
| AC5 | `wd --exec "Get-ChildItem"` — exit 0, чистый stdout без ANSI. **Все 8 ACs из `feat/wd-exec` повторно зелёные.** |
| AC6 | GUI shell в `wiredesk-client` отвечает как раньше — никаких ANSI escape в render'е, 0 регрессий |
| AC7 | Resize окна `wd` (Ghostty/iTerm) → vim/htop reflow корректно; `[Console]::WindowWidth` на host видит новое значение |
| AC8 | `cargo test --workspace` + `cargo clippy -- -D warnings` — green на Mac. Live build на Win11 — green |

## Тестирование

- `apps/wiredesk-host/src/shell.rs`: новый `pty_echo_through_shell` (unix-side portable-pty: `/bin/sh` echo round-trip). Сохранить existing `echo_through_shell` для pipe-mode regression coverage.
- `apps/wiredesk-host/src/shell.rs`: тест `resize_no_op_on_pipe_mode` — `ShellProcess::resize` не паникует при `pty=false`.
- `apps/wiredesk-protocol`: serde round-trip `ShellOpen { pty: bool }` + `PtyResize`. Backward-compat: `ShellOpen` JSON без `pty` field парсится в `pty=false`.
- Если оборачиваем `crossterm::terminal::size()` в helper — unit test на ok/err.

## Риски

| Risk | Severity | Mitigation |
|---|---|---|
| PSReadLine escape-codes на keystroke (~20-40 байт/keypress) дают visible latency на 115200 | medium | Live measure. Если плохо — fallback на `--no-readline` через `Set-PSReadLineOption -EditMode None` (рантайм-toggle). |
| `portable-pty` ConPTY edge case на конкретной Win11 build'е | low | Win11 (мейнстрим Win10 1809+) — full-supported. Wez Furlong (wezterm) maintains. |
| Двойной echo если client забыл переключиться в pass-through raw | low | Спецификация требует pass-through когда `pty=true`; integration AC1-AC4 ловят. |
| Resize race с активным renderer'ом vim'а | low | Норма для ssh-tty; vim сам redraw'ит на SIGWINCH. |
| Регрессия `--exec` если case'ом изменим shared code path | high | `pty=false` оставляет существующий код path в spawn полностью intact; AC5 явно покрывает. |

## Первые шаги

1. Создать ветку `feat/host-pty` (отдельно от `master`, в master сидит `--exec`).
2. `crates/wiredesk-protocol`: добавить `pty: bool` в `ShellOpen` (serde default false) + `PtyResize` message + tests.
3. `apps/wiredesk-host/Cargo.toml`: `portable-pty = "0.9"`.
4. `apps/wiredesk-host/src/shell.rs`: ввести `pty: bool` параметр в `ShellProcess::spawn`; реализовать ConPTY-ветку; написать `pty_echo_through_shell` тест.
5. Дёрнуть live на Win11: `cargo run -p wiredesk-host` + `wd` с одного из Mac'ов → `vim`/`ssh dev`/`git commit` smoke-test (AC1-AC4).

## Сложность

**Medium.** Effort 5-7h (5h код + 1-2h live-Win debug PSReadLine/latency quirks).

## Что НЕ входит в scope

- ConPTY для GUI shell-panel (egui terminal emulator) — отдельная задача.
- Переписывание `--exec` под TTY-output — оно pipe-based stays.
- Mac autostart, code signing, FT232H baud upgrade — независимые follow-up'ы.
- Multiplexing GUI shell + terminal-binary одновременно — взаимоисключающие, оба открывают serial-port (известное ограничение).

## Memory ссылки

- `project_conpty_followup.md` — original follow-up note
- `feedback_serial_terminal_bridge.md` — cooked-mode pattern который ConPTY заменяет
- `feedback_ps_pipe_exec_quirks.md` — почему `--exec` остаётся pipe-mode (sentinel detection)
