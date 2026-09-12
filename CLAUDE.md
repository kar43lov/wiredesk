# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

Что это, какую задачу решает и чего осознанно не делает — `README.md` (разделы «Problem», «Solution», «What WireDesk does / does NOT do»); развёрнутый обзор — `docs/project-overview.md`.

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt                                           # CI проверяет `--check`; дерево приведено к rustfmt 2026-09-04
cargo build --release --workspace

# Один крейт / один тест по имени (substring-фильтр):
cargo test -p wiredesk-client                       # все тесты крейта
cargo test -p wiredesk-client decide_text_send      # тесты с этой подстрокой в имени

# Windows-код (host И клиент) с мака — clippy проверяет типы, build проверяет линковку:
cargo clippy -p wiredesk-client --target x86_64-pc-windows-gnu --all-targets -- -D warnings
cargo build  -p wiredesk-client --target x86_64-pc-windows-gnu    # нужен `brew install mingw-w64`

# CI идёт только на push в main и на PR — feature-ветка НЕ линтуется на Windows сама:
gh workflow run ci.yml --ref <branch>               # и потом `gh run list --branch <branch>`

# Перед ревью крупного диффа — поднять локальный stable до версии CI (`dtolnay/rust-toolchain@stable`):
rustup update stable    # 11.09.2026 локально был 1.94, CI 1.98 → линт chunks_exact_to_as_chunks упал уже на main
```

🔴 **Обе стороны собираются под обе ОС, и правка платформенного кода должна проверяться обеими командами выше.** Host: `WindowsInjector` на Windows, `MockInjector` на macOS (реальный SendInput не зовётся — для dev-цикла нормально). Клиент: полноценные реализации на обеих платформах за фасадами `keyboard_tap` / `status_bar` / `monitor` / `clipboard_files`; `cargo check` на маке НЕ увидит поломку Windows-ветки.

## Run

Команды запуска и сценарии — `README.md` («Build», «Run»); полная версия со всеми режимами и отладкой — `docs/run.md`.

## Architecture

Слои, модули и потоки данных — `docs/architecture.md` (краткая версия — `README.md`, раздел «Architecture»).

## Известные ограничения (индекс)

Полные формулировки с причинами — `docs/known-limitations.md`.

- Канал не аутентифицирован и не шифрован ни на одном транспорте; для BLE это реальная дыра (GATT `Plain`,…
- Ctrl+Alt+Del через SendInput не сработает на Windows (защищено ядром, нужен SAS API в SYSTEM-сервисе или…
- macOS Secure Input — поля паролей в любом приложении на Mac отключают CGEventTap системно
- Accessibility permission требуется и привязана к binary
- Файлы — single-file, ≤20 MB. Multi-file selection silently skip'ается (Phase 2 follow-up:…
- Видео — никогда
- Save+Restart pattern: большинство changes в settings UI требуют перезапуск процесса
- Mac autostart — не реализован (только manual launch из дока / Spotlight)
- Автозапуск хоста на Windows работает только от администратора (UIPI режет `SendInput` в окна элевированных приложений), поэтому заводится задачей планировщика `ONLOGON` + `RL HIGHEST`, а не ключом `Run`; 🔴 exe при этом обязан лежать в каталоге, куда обычный пользователь писать не может, иначе автозапуск = локальный EoP
- Outbound text debounce — ~400ms окно для physical Cmd+V (accepted limitation): debounce задерживает…
- Outbound text debounce — mixed-format clipboard, image case (accepted limitation): если ОДИН clipboard-item…
- Тот же race для файлов — FIXED (`main` `bf47aae`, 2026-07-01): Finder-копия файла лениво (200ms–9s…
- Code signing / нотарификация .app — не делается
- Single-instance на Win'е: при втором запуске exe — открывается Settings существующего процесса (через named…
- App icon в .exe embed'ится только при сборке на Windows (rc.exe / windres needed)
- PTY-mode — только на Windows-host'е и только для интерактивного `wd`; `wd --exec` остаётся pipe-based (design choice)
- У хоста два shell-слота (exec + pty), и какой протокол в ходу — решает `HelloAck.version`; обновлять стороны в порядке «сначала Mac, потом host», иначе параллельной работы просто не будет
- 🔴 Провод один: **при открытой консоли `wd --exec` идёт вчетверо медленнее** (урезанный бюджет канала — 407 КБ за 61 с против 15 с соло, замер 12.09.2026); без открытой консоли скорость прежняя
- 🔴 Любой вызов буфера обмена ОС идёт через `locked_*`-хелперы (лок `wiredesk_core::clipboard_lock`): два одновременных обращения к `NSPasteboard` роняют процесс, и юнит-тесты системный буфер не трогают вовсе. Прямой вызов `arboard` ловит `clippy.toml`
- Fullscreen — borderless (не native): Spaces-переход терял окно в WindowServer. Меню-бар/таскбар перекрываются уровнем окна, а не скрытием Dock (оно было на все дисплеи сразу); уровень снимается при потере фокуса
- Windows-клиент: `wd`/`wd --exec` только с Mac; BLE недоступен (роль Peripheral занята хостом, принудительный откат на serial), RFCOMM работает; 🔴 нет аналога Secure Input — хук в capture видит и пароли
- RFCOMM (`transport = "rfcomm"`) — живой линк с pairing и шифрованием; канал задаётся вручную на обеих сторонах (SDP-запись хоста с Mac не видна), собранный .app не получает системный запрос на Bluetooth — запускать бинарь из терминала
- RFCOMM с `keepalive_ms = 0` — хост считает клиента мёртвым после 8 с молчания и отдаёт линк другому устройству (с дефолтным keepalive молчания не бывает)
- Длинная команда `wd --exec` (>4 КБ) кладётся хостом во временный .ps1 и dot-source-ится: PowerShell разбирает длинную строку за квадратичное время; **текст команды при этом лежит на диске хоста** — секреты в теле команды не передавать

## Hardware setup

```
Host HDMI → splitter → monitor + capture card → Mac (QuickTime/VLC for video)
Host USB-Serial ←→ null-modem (TX-RX crossed, GND-GND, VCC isolated) ←→ Mac USB-Serial
```

CH340 USB-to-TTL кабели: красный=VCC (изолировать), синий=GND, зелёный=TX, белый=RX. Полная инструкция: `docs/setup.md`.

## Channel speed upgrade

Разбор апгрейда канала (варианты транспорта, замеры, что выбрано) — `docs/bluetooth-transport.md`. Bluetooth: `transport = "rfcomm"` (Classic, ~120 KB/s) — основной; `"bluetooth"` (BLE, 4–5 KB/s) — fallback.

## Plan

`docs/plans/wiredesk-mvp.md` — full MVP plan with protocol spec, etapes, and risk analysis.

`docs/briefs/ft232h-upgrade.md` — бриф апгрейда канала (**SHIPPED 2026-05-28** @ 3 Mbaud verified live; см. шапку файла).

`docs/briefs/interactive-wd-via-gui-ipc.md` + `docs/plans/completed/20260703-interactive-wd-via-gui-ipc.md` — interactive `wd` через GUI IPC (**SHIPPED в main 2026-07-03, live-verified**; 730 тестов на момент приёмки; последний direct-serial-путь устранён). Live-приёмка на реальном Mac+Ghostty+Win11: `wd` при открытом GUI подключился через IPC, промпт PowerShell не потерялся, `wd --exec` при активном интерактиве → «shell busy» exit 125. Host не менялся (wire-совместим, переустанавливать не нужно). 3 Codex P2 из `/pg.review` пофикшено — все три про порядок операций в двунаправленном socket-релее.

`docs/briefs/daemon-multiplex.md` — SUPERSEDED roadmap-бриф: full `wiredesk-daemon`-extraction больше не нужен — embedded-IPC-мост покрыл и `wd --exec`, и interactive `wd`.

`docs/briefs/gui-shell-pty-emulator.md` — устаревший roadmap-бриф (vt100 egui TerminalView для shell-panel): сама GUI shell-panel удалена, interactive `wd` через IPC-релей закрыл потребность.
