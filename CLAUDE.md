# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Что это, какую задачу решает и чего осознанно не делает — `README.md` (разделы «Problem», «Solution», «What WireDesk does / does NOT do»); развёрнутый обзор — `docs/project-overview.md`.

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo build --release --workspace

# Один крейт / один тест по имени (substring-фильтр):
cargo test -p wiredesk-client                       # все тесты крейта
cargo test -p wiredesk-client decide_text_send      # тесты с этой подстрокой в имени
cargo test -p wiredesk-host -- --test-threads=1     # host флакает на parallel runner'е macOS (~50% SIGABRT) — для надёжности
```

Host компилируется и на macOS (с MockInjector), и на Windows (`WindowsInjector` за `cfg(target_os = "windows")` через crate `windows`). На macOS реальный SendInput не вызывается — для dev-цикла без Windows это нормально.

## Run

Команды запуска и сценарии — `README.md` («Build», «Run»); полная версия со всеми режимами и отладкой — `docs/run.md`.

## Architecture

Слои, модули и потоки данных — `docs/architecture.md` (краткая версия — `README.md`, раздел «Architecture»).

## Известные ограничения (индекс)

Полные формулировки с причинами — `docs/known-limitations.md`.

- Ctrl+Alt+Del через SendInput не сработает на Windows (защищено ядром, нужен SAS API в SYSTEM-сервисе или…
- macOS Secure Input — поля паролей в любом приложении на Mac отключают CGEventTap системно
- Accessibility permission требуется и привязана к binary
- Файлы — single-file, ≤20 MB. Multi-file selection silently skip'ается (Phase 2 follow-up:…
- Видео — никогда
- Save+Restart pattern: большинство changes в settings UI требуют перезапуск процесса
- Mac autostart — не реализован (только manual launch из дока / Spotlight)
- Outbound text debounce — ~400ms окно для physical Cmd+V (accepted limitation): debounce задерживает…
- Outbound text debounce — mixed-format clipboard, image case (accepted limitation): если ОДИН clipboard-item…
- Тот же race для файлов — FIXED (`main` `bf47aae`, 2026-07-01): Finder-копия файла лениво (200ms–9s…
- Code signing / нотарификация .app — не делается
- Single-instance на Win'е: при втором запуске exe — открывается Settings существующего процесса (через named…
- App icon в .exe embed'ится только при сборке на Windows (rc.exe / windres needed)
- PTY-mode только для interactive `wd`, не для `wd --exec` — он остаётся pipe-based (design choice:…
- PTY-mode только на Windows host'е
- Параллельный cargo test флакает на macOS для host'-пакета (~50% SIGABRT) — это pre-existing baseline issue…
- macOS menu bar reveal в native fullscreen — в native (Spaces-style) fullscreen…

## Hardware setup

```
Host HDMI → splitter → monitor + capture card → Mac (QuickTime/VLC for video)
Host USB-Serial ←→ null-modem (TX-RX crossed, GND-GND, VCC isolated) ←→ Mac USB-Serial
```

CH340 USB-to-TTL кабели: красный=VCC (изолировать), синий=GND, зелёный=TX, белый=RX. Полная инструкция: `docs/setup.md`.

## Channel speed upgrade

Разбор апгрейда канала (варианты транспорта, замеры, что выбрано) — `docs/bluetooth-transport.md`.

## Plan

`docs/plans/wiredesk-mvp.md` — full MVP plan with protocol spec, etapes, and risk analysis.

`docs/briefs/ft232h-upgrade.md` — бриф апгрейда канала (**SHIPPED 2026-05-28** @ 3 Mbaud verified live; см. шапку файла).

`docs/briefs/interactive-wd-via-gui-ipc.md` + `docs/plans/completed/20260703-interactive-wd-via-gui-ipc.md` — interactive `wd` через GUI IPC (**SHIPPED в main 2026-07-03, live-verified**; 730 тестов; последний direct-serial-путь устранён). Live-приёмка на реальном Mac+Ghostty+Win11: `wd` при открытом GUI подключился через IPC, промпт PowerShell не потерялся, `wd --exec` при активном интерактиве → «shell busy» exit 125. Host не менялся (wire-совместим, переустанавливать не нужно). 3 Codex P2 из `/pg.review` пофикшено (см. memory `feedback_ipc_relay_ordering_races`).

`docs/briefs/daemon-multiplex.md` — SUPERSEDED roadmap-бриф: full `wiredesk-daemon`-extraction больше не нужен — embedded-IPC-мост покрыл и `wd --exec`, и interactive `wd`.

`docs/briefs/gui-shell-pty-emulator.md` — устаревший roadmap-бриф (vt100 egui TerminalView для shell-panel): сама GUI shell-panel удалена, interactive `wd` через IPC-релей закрыл потребность.
