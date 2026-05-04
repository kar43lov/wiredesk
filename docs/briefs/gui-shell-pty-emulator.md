# Бриф: ConPTY-эмулятор в egui GUI shell-panel

**Status:** не начат, в roadmap. Активируется когда нужно гонять vim/htop/colored команды прямо из `WireDesk.app` без отдельного `wd` процесса.

## Проблема

GUI shell-panel в `WireDesk.app` (Settings → Terminal collapsing) сейчас pipe-mode без ANSI parser'а. Symptom:
- `git status` через GUI panel — output идёт с literal escape-кодами `\x1b[1m\x1b[32m` вместо цветов.
- `vim`, `htop`, `nano`, `less` — ломаются (все ANSI-cursor sequences показываются как текст).
- `psql`, `node` REPL — частично работают (echo есть от host'а), но prompt + cursor positioning не рендерятся.

Сравнение с CLI `wd` (PR #10, `feat/host-pty`): там interactive `wd` уже использует ConPTY на host'овой стороне (`portable-pty`) и Ghostty/iTerm рендерят ANSI нативно. GUI остаётся на pipe-mode по дизайн-причине: «egui без ANSI parser'а» (CLAUDE.md строка 135).

В roadmap'е этот пункт упоминается как «отдельный follow-up» — теперь записан полноценно.

## Подходы

### A. Full ANSI terminal emulator (recommend)

Wire ShellOpenPty в GUI client'е (как CLI делает): шлёт `ShellOpenPty { shell, cols, rows }`, host спавнит через `portable-pty`. GUI получает ShellOutput с raw ANSI escape codes. Parse → render через ANSI parser:

- Crate options: `vt100` (~simple, парсит CSI/SGR/CUP в text-grid), `wezterm-term` (full-featured, тяжелее, тянет много deps), `anstream`/`anstyle` (только parsing, не grid-state).
- Recommend `vt100` для start'а — light, известный, использует grid-based screen model которая мапится на egui-grid render.

GUI render:
- Custom egui widget `TerminalView` — monospace grid (cols × rows), каждая cell — char + style (fg/bg/bold/underline). Ridge через `egui::FontId::monospace` + `Painter::text` per-cell, либо `LayoutJob` с per-segment styling если cells contiguous.
- Resize handler: GUI panel size → cols/rows → шлёт `PtyResize` на host (как `wiredesk-term` делает на 500ms cadence или resize event'е).
- Cursor: blinking block через `Animation`, position from vt100 state.
- Scrollback: vt100 имеет history buffer, expose в egui ScrollArea.
- Input: keystrokes из egui → `Message::ShellInput { data }` (без cooked-mode line discipline — host ConPTY сам делает echo).

**Pros:**
- Vim/htop/colored bash работают так же как в нативном Ghostty.
- Унификация: GUI shell-panel и CLI `wd` оба используют ShellOpenPty path. `wd --exec` остаётся pipe-based (sentinel detection).
- Fallback если daemon-multiplex (см. `docs/briefs/daemon-multiplex.md`) сделан позже — GUI остаётся primary frontend для shell.

**Cons:**
- Effort: ~1-2 недели. Основное время — `TerminalView` widget'у. ANSI parser library делает 60% работы, остальное — grid render + cursor + scrollback + resize wiring.
- Новый dep `vt100` (или эквивалент) — ~3K LOC.
- Performance: per-frame render всей grid'ы в egui — 80×24 = 1920 cells × 60 fps. Нужно `egui::Image`-cache или dirty-tracking.

### B. Strip ANSI + show plain text

Проще: reuse уже существующий `strip_ansi` helper из `wiredesk-term/src/main.rs`, применять к ShellOutput перед добавлением в textarea. Колоры теряются, ANSI cursor sequences (используемые vim/htop) показываются как мусор → ломается всё что не plain output.

**Pros:**
- ~3 часа работы.
- No new deps.

**Cons:**
- Не решает proблему: vim, htop, less, nano, psql REPL — всё ещё ломается. Цвета git status / ls --color теряются. Это палиатив, не fix.
- Если потом делать A — придётся переделать textarea на TerminalView, B становится throwaway работой.

### C. Embed external terminal emulator (Alacritty/Wezterm)

Wezterm имеет `wezterm-term` (без UI), Alacritty — `alacritty_terminal`. Они полностью реализуют terminal model + grid + scrollback. Rendering — наша задача.

**Pros:**
- Battle-tested против real-world apps (Alacritty используется как daily-driver терминал миллионами).
- Вся terminal model готова.

**Cons:**
- Тяжёлые deps. `wezterm-term` тянет ~5+ crates, alacritty-terminal — похожее.
- Overkill для shell-panel в Settings collapsing — это не full terminal app.

## Рекомендация

**A (vt100 + custom egui widget)** — правильный размер. Light dep, известный shape, мапится на egui без heroics. Работа в основном render — она прямолинейная (drawing monospace grid в egui — solved-problem pattern).

**B (strip ANSI)** не запускать — это throwaway работа, которая не решает основной use-case. Если кому-то нужен colored `git status` в GUI прямо сейчас — лучше не делать ничего и использовать `wd` в Ghostty (там ANSI рендерится нативно).

## Acceptance criteria (для будущего planning'а)

- **AC1.** GUI shell-panel: `ls --color=always` → output с цветными именами файлов.
- **AC2.** `git status` → красные/зелёные строки рендерятся (SGR codes 31/32).
- **AC3.** `vim test.txt` → открывается, можно редактировать, save через `:wq` корректно exit'ит, terminal-state восстанавливается (alt-screen + cursor restoration).
- **AC4.** `htop` → top-bar + process list рендерятся, cursor-positioning работает, `q` выходит.
- **AC5.** Resize GUI panel → `PtyResize { cols, rows }` шлётся на host, vim reflow'ит.
- **AC6.** `wd --exec` через CLI продолжает работать без регрессий (он — pipe-mode, его не трогаем).
- **AC7.** `cargo test --workspace -- --test-threads=1` зелёный, AC1-AC4 verified live на CH340 + Win11.

## Не в scope

- ConPTY-эмулятор в `wd --exec` — design choice, exec остаётся pipe-mode (clean stdout для sentinel detection).
- Mouse-tracking SGR codes — vim/htop/less не критично, не делаем.
- 256-color / true-color SGR (38;2;r;g;b) — поддержать только 16 базовых ANSI colors на старте, остальные — fallback к ближайшему. True-color — follow-up если кому-то нужен.
- Bracketed paste — `wezterm-term` это умеет, vt100 — нет; не делаем сейчас.
- Daemon-mode (см. `docs/briefs/daemon-multiplex.md`) — независимо. GUI panel может работать в обоих режимах — single-process (как сейчас) или daemon-client (потом).

## Связанные

- `docs/briefs/daemon-multiplex.md` — другой ось (concurrency, не rendering). Independent — можно делать в любом порядке.
- `apps/wiredesk-host/src/shell.rs::Backend::Pty` — ConPTY backend на host'е уже работает (PR #10).
- `apps/wiredesk-term/src/main.rs::bridge_loop` — CLI клиент для PTY shell, useful как reference для input/resize wiring.
- `crates/wiredesk-protocol/src/message.rs` — `ShellOpenPty`, `PtyResize` уже определены, переиспользовать.
- Memory `project_conpty_followup.md` — описание CLI ConPTY реализации (PR #10).
- Memory `feedback_serial_terminal_bridge.md` — обзор cooked-mode discipline (не нужна в PTY-mode).

## Сложность

**Medium.** Известная архитектура (terminal emulator поверх PTY). Effort в основном render-side egui: ~1 неделя на TerminalView widget с базовым SGR/cursor + scrollback, ещё несколько дней на resize/input wiring и регрессии. Нет архитектурных surprises — host-side готов, protocol готов, остался GUI render.
