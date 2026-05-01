# Бриф: фоновое использование WireDesk на обеих сторонах

**Цель:** Превратить обе стороны из консольных утилит в системно-интегрированные приложения. Windows host — трей-агент с autostart при загрузке. Mac client — .app bundle с иконкой в доке. Оба сохраняют настройки в TOML, добавляют settings UI без регрессий существующей функциональности.

**Выбранный подход:** «Save + manual restart» для apply-логики. Пользователь меняет настройки, файл сохраняется, toast напоминает перезапустить. Простой код, нет supervisor'а / race conditions, и при autostart на Windows перезапуск всё равно дешёвый. Live-reconnect — апгрейд на потом.

## Стек

**Windows (host):**
- `native-windows-gui = "1"` (`nwg`) — UI: settings window + tray-icon (встроенно через `nwg::TrayNotification`)
- `windows_subsystem = "windows"` атрибут — без консольного окна
- `auto-launch = "0.6"` — autostart через HKCU\Software\Microsoft\Windows\CurrentVersion\Run
- `single-instance = "0.3"` — named mutex, второй запуск фокусит первый
- `tracing` + `tracing-appender` — лог в `%APPDATA%\WireDesk\host.log` с ротацией

**Mac (client):**
- Bash-скрипт `scripts/build-mac-app.sh` — собирает `target/release/WireDesk.app` с Info.plist, .icns, бинарём
- `iconutil` (встроен в macOS) для конвертации W.png → .icns
- Settings panel — расширение существующего `WireDeskApp::update()`, добавляется в chrome-режиме (не в capture/fullscreen)

**Общее:**
- `serde` + `toml` уже в deps
- Config types в новом crate (или модуль): `WireDeskHostConfig`, `WireDeskClientConfig`

## Требования

### Функциональные — Windows host
- F1: `windows_subsystem = "windows"` атрибут — без консольного окна при запуске
- F2: Tray-иконка с буквой W. Цвет — статус: зелёный (Connected), жёлтый (Waiting), серый (Stopped)
- F3: Tray-меню: «Show Settings», «Open Logs», «Quit»
- F4: Settings-окно (~400×400px, как Caramba):
  - COM port (combo, автоопределение CH340 через `serialport::available_ports()`)
  - Baud rate (numeric, дефолт 115200)
  - Width / Height (numeric, дефолт 2560×1440)
  - Чекбокс «Run on startup»
  - Status row: «● Connected to wiredesk-client» / «○ Waiting»
  - Кнопка «Copy Mac launch command» — копирует в clipboard готовое `./target/release/wiredesk-client --port /dev/cu.usbserial-XXX --baud N` с актуальными значениями (на основе обратной мэппинга COM3 → /dev/cu.usbserial-XXX по последней успешной handshake-сессии — иначе дефолт)
  - Кнопка «Save» — сохраняет TOML, показывает toast «Restart WireDesk to apply»
- F5: Persistence в `%APPDATA%\WireDesk\config.toml`. Дефолты — текущие хардкоды (COM3, 115200, 2560×1440). Создаётся при первом save.
- F6: Auto-start через `auto-launch` crate — чекбокс пишет/удаляет registry key
- F7: Single-instance: при втором запуске показывает message box «Already running, check tray», сам выходит. Поднимать первое окно не пытаемся — это требует named pipe IPC (overkill для solo-MVP). Если фокус на первом окне нужен — пользователь сам кликнет иконку трея.
- F8: Логи через `tracing-appender` rolling appender в `%APPDATA%\WireDesk\host.log`, ротация по дням, последние 7 файлов

### Функциональные — Mac client
- F9: `WireDesk.app` bundle: `Contents/MacOS/wiredesk-client`, `Contents/Info.plist` (CFBundleIdentifier=`dev.kar43lov.wiredesk`, CFBundleIconFile=`AppIcon.icns`), `Contents/Resources/AppIcon.icns`. Команда сборки `./scripts/build-mac-app.sh`
- F10: Settings panel в chrome-UI (под уже существующим контентом): expandable collapsing «Settings»:
  - Port (combo, автоопределение `/dev/cu.usbserial-*`)
  - Baud (numeric, дефолт 115200)
  - Host screen W / H (numeric, дефолт 2560×1440 — но эти приходят и из HelloAck)
  - Кнопка «Save» — сохраняет TOML, показывает inline toast «Restart to apply»
- F11: Persistence в `~/Library/Application Support/WireDesk/config.toml`
- F12: При старте обоих бинарей: читать TOML → применять как defaults → CLI args ещё override TOML (порядок: defaults → TOML → CLI args)

### Нефункциональные
- Без регрессий: 106 тестов проходят, мышь/клава/clipboard/shell/capture/fullscreen работают идентично
- Один бинарь host (~5 MB после strip), один .app bundle Mac
- Без installer'а

## Acceptance criteria (live на железе)
1. Windows: первый запуск `wiredesk-host.exe` → нет консольного окна, иконка W в трее
2. Tray-меню работает: правый клик → Show Settings / Open Logs / Quit
3. Settings: меняем port, нажимаем Save → toast, перезапуск через Quit+автозапуск → host работает с новыми настройками
4. Чекбокс «Run on startup»: вкл → перезагрузка Windows → host автоматически в трее
5. Кнопка «Copy Mac command»: вставка в Mac terminal → клиент запускается успешно
6. Mac: `./scripts/build-mac-app.sh` → `target/release/WireDesk.app` существует. Кликаем → окно открывается, в доке буква W
7. Mac chrome-UI: блок settings с актуальными значениями. Меняем port → Save → toast → перезапуск → новые настройки применены
8. Capture/fullscreen UI без изменений (не показывает settings)
9. `cargo test --workspace` — все 106 тестов + новые проходят
10. Single-instance: второй запуск host → message box «Already running, check tray» → второй процесс выходит. Первое окно остаётся как было.

## Тестирование
- **Unit:** config TOML serialize/deserialize, auto-launch enable/disable (mock реестра если получится), settings struct → CLI override merge
- **Integration:** не нужно — UI и .app bundle — manual
- **Live тест:** AC1-AC10 руками
- 5 новых unit-тестов minimum

## Что НЕ входит в scope
- Code signing / notarization (.app будет unsigned, при первом запуске Gatekeeper попросит подтверждение)
- DMG / .pkg installer
- Auto-update механизм
- Live-reconnect on settings change (отложено)
- Mac autostart (только manual launch из дока)
- Linux support
- Tray icon на Mac (host-only фича)

## Риски
1. nwg learning curve — малая (~1 день на освоение паттерна `derive(NwgUi)`)
2. .app bundle path resolution — Info.plist должен быть точным; iconutil требует исходную PNG в правильных размерах (16/32/64/128/256/512×@1+@2). Скрипт-генератор + статический набор PNG из одного source.
3. nwg может не пересобраться чисто на macOS dev-машине — добавить `[target.'cfg(windows)'.dependencies] nwg = "1"`, на Mac не тянется
4. Toast/notification на Windows: nwg имеет `nwg::Notice`, но это balloon. Возможно стоит просто label в settings-окне с auto-clear через 3с
5. Single-instance focus: «show first window» нужен IPC. Для solo: named pipe / file lock. Самый простой путь — `single-instance` crate сам этого НЕ делает (только лок), нужен отдельный механизм. Альтернатива — просто вывести «Already running, check tray» и выйти

## Первые шаги
1. Создать `crates/wiredesk-config` (или модуль в `wiredesk-core`) с `HostConfig`/`ClientConfig` структурами + load/save TOML
2. Mac: расширить `WireDeskApp` settings-блоком в chrome-UI, читать из TOML на старте, save в TOML
3. Mac: написать `scripts/build-mac-app.sh` + источник иконки `assets/icon-W.png` → `WireDesk.app`
4. Windows: новый бинарь-процесс `wiredesk-host`, переписать на nwg-based GUI в трее, переиспользовать `Session` логику (вынести в session-thread)
5. Live-тест на железе
6. Документация: README, CLAUDE.md, docs/setup.md

## Сложность

**Medium-high.** Точечный объём:
- Windows host UI: ~400-500 строк nwg-кода (новое для проекта)
- Config persistence: ~150 строк
- Mac settings UI: ~100 строк (расширение существующего)
- Mac .app bundle script: ~50 строк bash + одна PNG

**Где живёт работа:** ветка `feat/launcher-ui` (создана). Master стабилен на `221a75f`. Мерж только после live-теста.
