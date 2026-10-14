# Запуск WireDesk (полная версия)

Все режимы запуска и отладки. Короткая версия — в [`README.md`](../README.md), раздел «Run».

---


## Run

Хардкод-дефолты (низший приоритет резолвинга) — COM3 на Windows, `/dev/cu.usbserial-120` на Mac, baud 115200, разрешение 2560×1440 — исторические заглушки под голый CH340-кабель "из коробки". Реальный solo-сетап (FT232H, `/dev/cu.usbserial-140` @ 3_000_000, 2560×1440) живёт в `config.toml` обеих сторон, а не в этих дефолтах. `wd` (`wiredesk-term`) вдобавок auto-detect'ит адаптер по VID при старте — см. ниже.

### Configuration

Обе стороны грузят настройки из TOML на старте:

| Платформа | Путь                                                         |
|-----------|--------------------------------------------------------------|
| Windows   | `%APPDATA%\WireDesk\config.toml`                             |
| macOS     | `~/Library/Application Support/WireDesk/config.toml`         |

Порядок резолвинга (низший → высший приоритет): хардкод-дефолты → `config.toml` → CLI args. Override через `clap::ArgMatches::value_source()` — если значение пришло из CLI/Env, оно побеждает; иначе — TOML.

### Host (Windows) — tray agent

Release-сборка работает фоновым tray-приложением. `windows_subsystem = "windows"` атрибут (только в release) скрывает консоль. Debug-build держит консоль для разработки.

```powershell
.\target\release\wiredesk-host.exe
```

#### Ручное обновление и запуск из репозитория

Для текущего пользовательского варианта exe запускается непосредственно из
`C:\Download\prjcts\wiredesk\target\release\`; перенос в Program Files не нужен.
Перед обновлением закрыть хост через tray → Quit. Вставить весь блок в PowerShell:

```powershell
& {
    $ErrorActionPreference = 'Stop'
    Set-Location 'C:\Download\prjcts\wiredesk'
    git pull --ff-only
    if ($LASTEXITCODE -ne 0) { throw 'git pull failed' }
    cargo build --release --locked -p wiredesk-host
    if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
    Start-Process '.\target\release\wiredesk-host.exe' -Verb RunAs
}
```

Для запуска уже собранного exe достаточно `Start-Process` из последней строки.
Это ручной запуск с запросом UAC; требования к каталогу для автоматической
задачи планировщика описаны отдельно в `docs/known-limitations.md`.

- **Tray-меню** (правый клик): Show Settings / Open Logs / Quit
- **Typography:** глобальный default font — Segoe UI 16px (`nwg::Font::set_global_default` сразу после `nwg::init()`, до построения окон). На Win11 со 100% scaling это нативный диалог-вид; контролы наследуют без явного присваивания.
- **Settings window** (через tray): port — выпадающий список обнаруженных портов с подписью чипа («COM7 — FT232H», «COM5 — CH340», «COM9 — USB serial 1234:ABCD», «COM1 — serial») + free-text manual override («or type:»), обновляется при каждом открытии окна и по `Detect`. Кнопка `Detect` (`ui::format::classify_ports` / `target_indices`) перечисляет все serial-порты, заполняет дропдаун и авто-выбирает WireDesk-адаптер — CH340 VID 0x1A86 **или** FTDI VID 0x0403 (FT232H/R/2232/4232); при двух адаптерах берёт первый и просит выбрать. Выбор в дропдауне копирует bare-COM в manual-поле (canonical value, читается при Save). Дальше: baud, width/height, чекбокс «Run on startup (as admin)» (заводит задачу планировщика с `RunLevel HIGHEST`; из неэлевированного host'а откатывается на ключ `Run` с warning'ом в логе), кнопка Copy Mac launch command. Кнопки в нижнем button-bar: `Re&start` (сохраняет TOML и спавнит новый процесс через `Command::spawn` + `stop_thread_dispatch`; новый процесс получает mutex через 5×100ms retry-loop) / `&Save` (primary — пишет TOML без рестарта). Save+Restart pattern: большинство изменений требуют перезапуск для apply. **Исключение — чекбокс «Receive files»: применяется live по Save** (без Restart) через shared `Arc<AtomicBool>` (`main` → session_thread + Save-handler); порт/baud/размер/transport по-прежнему требуют Restart. Окно **resizable** (флаг `MAIN_WINDOW`, а не `WINDOW` — есть `WS_THICKFRAME` + minimize/maximize; nwg `GridLayout` reflow'ится по `WM_SIZE`, `center(true)` открывает окно по центру активного монитора). Раньше был `WINDOW` (fixed-border) — на мониторе с другим DPI/масштабом правая колонка раскладки обрезалась без возможности расширить окно.
- **Single-instance lock**: named mutex `WireDeskHostSingleton`. Второй запуск показывает «Already running — check tray icon» и выходит.
- **Logs**: `%APPDATA%\WireDesk\host.log.YYYY-MM-DD` через `tracing-appender::rolling::daily`. `tracing-log::LogTracer` мостит legacy `log::*` в tracing, panics через `install_panic_hook()`.

### Client (macOS) — `.app` bundle

```bash
./scripts/build-mac-app.sh
# → target/release/WireDesk.app

open target/release/WireDesk.app
```

Dev shortcut for kill+rebuild+launch with logs:

```bash
./scripts/run-mac.sh                          # default RUST_LOG=info,btleplug=info
RUST_LOG=debug ./scripts/run-mac.sh           # more verbose
./scripts/run-mac.sh --no-build               # quick relaunch, skip rebuild
```

Logs tee to `/tmp/wiredesk-mac.log` for retrospective analysis.

**Подпись и Accessibility.** `build-mac-app.sh` подписывает бандл ad-hoc, и macOS считает каждую пересборку новым приложением — грант Accessibility слетает, экран с инструкцией показывается снова. Один раз выполнить `./scripts/make-dev-signing-cert.sh` — он кладёт в login keychain self-signed identity «WireDesk Dev» (два запроса пароля: доверие сертификату и первый `codesign`), после чего скрипт сборки подхватывает её сам (или задать любую другую через `WIREDESK_SIGN_IDENTITY`), designated requirement остаётся прежним и грант переживает пересборки. Сам грант, выданный уже работающему процессу, подхватывается без перезапуска — тап стартует, экран пропадает через ≤2 с.

- **Settings panel** в chrome-UI (сгруппирована в три `ui.group()` блока — Connection / Display / System): port (combo + free-text), baud, host screen W×H, monitor selection (ComboBox с кэшированным `monitor::list_monitors()` через NSScreen, refresh раз в секунду), client name. Save пишет `~/Library/Application Support/WireDesk/config.toml` и показывает inline toast 3 секунды. В capture/fullscreen settings panel скрыта (info-only screen без интерактивных элементов).
- **Capture-mode UI**: в обычном Capture Input центральная панель пустая, а подсказки и прогресс — overlays; это сохраняет координаты мыши. В режиме «Экран Windows» виден только кадр с чёрными полями по пропорциям: баннеры, подсказки и progress-overlay скрыты. Ctrl+Cmd+Enter выходит из режима, Cmd+Esc освобождает ввод. Прогресс и кнопка отмены передачи доступны после выхода из desktop-режима.
- **Permission screen** (`render_permission_screen`): тексты вынесены в pure helper `permission_steps() -> &'static [&'static str]` (4 шага). Каждый шаг — `ui.group()` с цифрой в кружке слева. Кнопка `Open System Settings` живёт внутри шага 1 (action рядом с инструкцией).
- **Per-monitor fullscreen** (`Cmd+Enter`) — borderless: окно занимает выбранный дисплей без отдельного Space. `Decorations(false)`, `OuterPosition` и `InnerSize` задают границы; `mac_window::set_above_menu_bar` поднимает только окно WireDesk над меню-баром и Dock, пока оно в фокусе. При потере фокуса уровень возвращается к обычному. Dock на остальных дисплеях не скрывается. Если выбранный дисплей недоступен, используется текущий с предупреждением. Причина отказа от native fullscreen и механизм восстановления — `docs/known-limitations.md`.
- **Экран Windows** — синяя кнопка рядом с Capture Input или `Ctrl+Cmd+Enter`: тот же выбранный дисплей, изображение текущего Windows-стола, автоматический захват после первого кадра. Повторное сочетание закрывает режим и освобождает ввод. `Cmd+Esc` отпускает или снова включает ввод; после ошибки кадра/разрыва связи включать его явно. При выходе из ACTIVE в PASSIVE удержанные модификаторы отпускаются на Windows. Требуется host generation 5; настройка панели и ограничения — `docs/browser.md`.
- **Возврат окна после fullscreen** (`pre_fullscreen_geometry` → `PendingRestore` → `drain_pending_position_restore`): позиция + inner size снимаются **перед** входом, в обеих ветках — раньше снимок делался только когда `preferred_monitor` резолвился, и любой fallback-fullscreen выходил без восстановления вообще. При выходе (`exit_borderless_fullscreen`: вернуть меню-бар, `Decorations(true)`, страховочный `Fullscreen(false)` на случай системного ⌃⌘F) первая попытка ждёт `FULLSCREEN_SETTLE` (600 мс), дальше **проверяем фактическую позицию и повторяем** каждые 300 мс до 6 раз с допуском 24 pt (`restore_landed`). 🔴 Позиция читается у AppKit (`mac_window::real_outer_rect`), а НЕ из `viewport().outer_rect`: последний отражает нашу же отправленную команду, поэтому проверка по нему всегда «успешна» и retry завершался на первой итерации. Исчерпали попытки → `mac_window::set_outer_rect` напрямую через `setFrame:display:` — одиночный fire-and-forget терялся, если transition шёл дольше задержки, и окно оставалось на fullscreen-дисплее вместо того, с которого его открыли. Вместе с `OuterPosition` шлём `InnerSize` (fullscreen меняет размер) и `Focus` (после Spaces окно всплывает за чужими окнами). Исчерпали попытки → `move_onto_primary_if_offscreen`.
- **Внешний выход из fullscreen** (`sync_fullscreen_state`): зелёная кнопка, ⌃⌘F и Mission Control меняют состояние мимо `toggle_fullscreen`, и флаг `self.fullscreen` расходился с реальностью — exit-путь не выполнялся вовсе. Теперь каждый кадр сверяемся с `viewport().fullscreen`; расхождение в течение `FULLSCREEN_CONFIRM_TIMEOUT` (2 с) после нашей собственной команды игнорируется (winit подтверждает через кадр-два), устойчивое — принимается за пользовательское, с тем же release-capture + restore. Снимка при внешнем входе нет (outer rect уже во весь экран), поэтому fallback — геометрия из `config.toml`.
- **Auto-engage/release capture при fullscreen.** `toggle_fullscreen` при входе делает `if !self.capturing { self.toggle_capture() }`, при выходе — обратное (до отправки `Fullscreen(false)` чтобы успели отпустить модификаторы). Идея: fullscreen ≡ «я хочу управлять Host'ом», без второго хоткея не должно быть промежуточного состояния «fullscreen без capture».
- **Window geometry persistence**: outer position + inner size пишутся в `config.toml` (`window_x/y/w/h`, целые points) после того как окно 800 мс стоит на месте, плюс финальный flush в `eframe::App::on_exit` (move + сразу Cmd+Q). При старте `main.rs` отдаёт их в `ViewportBuilder::with_position`/`with_inner_size`. Без этого позиции нет вообще и AppKit кладёт окно на произвольный дисплей каждый запуск. Fullscreen не сэмплится (там outer rect = весь экран), как и промежуток пока `pending_position_restore` двигает окно после выхода из fullscreen. Запись идёт через `config::save_window_geometry`, который **перечитывает файл с диска** и правит только четыре поля — иначе сдвиг окна коммитил бы несохранённые правки из Settings-панели. Через 800 мс после старта — одноразовая проверка `rescue_offscreen_window`: если окно не пересекается ни с одним живым монитором хотя бы на 120×40 pt (закрыли на внешнем дисплее, вернулись без него), оно переезжает на primary.
- **Dock-icon pinning** (`force_dock_icon_from_bundle` в `main.rs`): winit/eframe иногда оставляют Dock с generic exec-иконкой через ~2с после launch. Загружаем `AppIcon.icns` из bundle через NSBundle/NSImage и зовём `[NSApp setApplicationIconImage:]` + `[NSApp setActivationPolicy:Regular]` из creator-callback'а eframe. Дополнительно `reapply_dock_icon_if_needed` пере-применяет иконку 4× в течение 10с из `update()` — это перебивает любое позднее переписывание системой/winit'ом.
- **Иконка**: `assets/icon-source.png` (1024×1024) → `Contents/Resources/AppIcon.icns` через `sips` + `iconutil` в build-mac-app.sh. Рисунок — контур монитора со стрелкой курсора на тёмно-зелёной подложке; прежняя белая «W» на синем читалась как Microsoft Word. Тот же исходник кормит Windows-.ico (`scripts/icogen`), а трей-глифы (`assets/tray-*.png`) рисуются отдельно (`scripts/generate-tray-icons.swift`) — на 16 px курсор и контур сливаются, поэтому там упрощённый силуэт.
- **Windows-клиент**: `.\scripts\build-win-client.ps1` собирает `target\release\wiredesk-client.exe`. Собирать лучше на самой Windows — тогда build.rs вшивает иконку в .exe (нужен rc.exe или windres). С мака проверяется так: `cargo clippy -p wiredesk-client --target x86_64-pc-windows-gnu --all-targets -- -D warnings` для типов и `cargo build -p wiredesk-client --target x86_64-pc-windows-gnu` для реальной линковки (второе требует `brew install mingw-w64`). Настройки и логи — `%APPDATA%\WireDesk\`, первый запуск берёт `COM3`, порт меняется в Settings → Save & Restart. Хоткеи: `Ctrl+Esc` — capture, `Ctrl+Enter` — fullscreen.
- **Значок в macOS menu bar**: `assets/menubar-icon.png` (36 px = 18 pt @2x), вшит в клиент через `include_bytes!` и ставится как **template**-изображение — AppKit сам красит его чёрным на светлой полосе и белым на тёмной. Рисуется `swift scripts/generate-menubar-icon.swift` из той же геометрии, что и app-иконка, но без подложки и с чуть уменьшенным курсором: в одноцветном силуэте он иначе слипается с рамкой монитора. До 2026-09-04 на этом месте была буква «W».
- **Info.plist**: `dev.kar43lov.wiredesk`, `LSUIElement=false`, `NSHighResolutionCapable=true`. Gatekeeper при первом запуске — правый-клик → Open
- Source-иконка можно перерисовать через `swift scripts/generate-icon.swift` (Swift+AppKit, без ImageMagick)
- **Logs**: `~/Library/Application Support/WireDesk/client.log.YYYY-MM-DD` через `tracing-appender::rolling::daily` (зеркалит host pattern). Dual-sink: file + stderr. `RUST_LOG=debug` фильтр работает (env-filter wired up). Panics + legacy `log::*` macros через `tracing-log::LogTracer`.

### Прямой запуск бинарей (dev)

```bash
# Host без tray (debug):
cargo run -p wiredesk-host

# Client GUI:
./target/release/wiredesk-client
# или через .app
open target/release/WireDesk.app

# Terminal-only клиент (raw-mode для Ghostty/iTerm), Ctrl+] для выхода
./target/release/wiredesk-term
```

Все флаги переопределяемы (`--port`, `--baud`, `--width`, `--height`, `--name`, `--shell`).

`wiredesk-client` и `wiredesk-term` больше **не** взаимоисключающие на Mac: при запущенном GUI `wd` (и interactive, и `--exec`) идёт через IPC-релей поверх `wd-exec.sock`, а не открывает serial-порт напрямую. Direct-serial (эксклюзивное открытие порта) остаётся только когда GUI закрыт.

Друг другу они тоже больше не мешают: с 2026-09-12 у хоста два shell-слота, и `wd --exec` выполняется, пока в соседнем окне открыт интерактивный `wd`. Условие — пересобранный хост (он объявляет поколение протокола в `HelloAck`); против старого хоста поведение прежнее, второй из двух получает `shell busy`.
