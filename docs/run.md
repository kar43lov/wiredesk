# Запуск WireDesk (полная версия)

Вынесено из `CLAUDE.md`; пользовательский вариант — в `README.md`, раздел «Run».

---

## Вынесено из CLAUDE.md 11.08.2026 (рез раздутого контекста)

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

- **Tray-меню** (правый клик): Show Settings / Open Logs / Quit
- **Typography:** глобальный default font — Segoe UI 16px (`nwg::Font::set_global_default` сразу после `nwg::init()`, до построения окон). На Win11 со 100% scaling это нативный диалог-вид; контролы наследуют без явного присваивания.
- **Settings window** (через tray): port — выпадающий список обнаруженных портов с подписью чипа («COM7 — FT232H», «COM5 — CH340», «COM9 — USB serial 1234:ABCD», «COM1 — serial») + free-text manual override («or type:»), обновляется при каждом открытии окна и по `Detect`. Кнопка `Detect` (`ui::format::classify_ports` / `target_indices`) перечисляет все serial-порты, заполняет дропдаун и авто-выбирает WireDesk-адаптер — CH340 VID 0x1A86 **или** FTDI VID 0x0403 (FT232H/R/2232/4232); при двух адаптерах берёт первый и просит выбрать. Выбор в дропдауне копирует bare-COM в manual-поле (canonical value, читается при Save). Дальше: baud, width/height, чекбокс «Run on startup», кнопка Copy Mac launch command. Кнопки в нижнем button-bar: `Re&start` (сохраняет TOML и спавнит новый процесс через `Command::spawn` + `stop_thread_dispatch`; новый процесс получает mutex через 5×100ms retry-loop) / `&Save` (primary — пишет TOML без рестарта). Save+Restart pattern: большинство изменений требуют перезапуск для apply. **Исключение — чекбокс «Receive files»: применяется live по Save** (без Restart) через shared `Arc<AtomicBool>` (`main` → session_thread + Save-handler); порт/baud/размер/transport по-прежнему требуют Restart. Окно **resizable** (флаг `MAIN_WINDOW`, а не `WINDOW` — есть `WS_THICKFRAME` + minimize/maximize; nwg `GridLayout` reflow'ится по `WM_SIZE`, `center(true)` открывает окно по центру активного монитора). Раньше был `WINDOW` (fixed-border) — на мониторе с другим DPI/масштабом правая колонка раскладки обрезалась без возможности расширить окно.
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

- **Settings panel** в chrome-UI (сгруппирована в три `ui.group()` блока — Connection / Display / System): port (combo + free-text), baud, host screen W×H, monitor selection (ComboBox с кэшированным `monitor::list_monitors()` через NSScreen, refresh раз в секунду), client name. Save пишет `~/Library/Application Support/WireDesk/config.toml` и показывает inline toast 3 секунды. В capture/fullscreen settings panel скрыта (info-only screen без интерактивных элементов).
- **Capture-mode UI** (`render_capture_overlays` + `render_capture_info_text`): banner и info-text рендерятся как `egui::Area` overlays с `interactable(false)` поверх **пустой** CentralPanel. Banner — full-width red-tinted «● CAPTURING — Cmd+Esc to release» (RichText 20pt, white-on-red) на верху, info-text — anchor-center с активными хоткеями. CentralPanel пустой по дизайну: Frame внутри центральной панели ел бы layout space и `normalize_mouse` squash'ил бы Host top region (фикс PR #14).
- **Permission screen** (`render_permission_screen`): тексты вынесены в pure helper `permission_steps() -> &'static [&'static str]` (4 шага). Каждый шаг — `ui.group()` с цифрой в кружке слева. Кнопка `Open System Settings` живёт внутри шага 1 (action рядом с инструкцией).
- **Per-monitor fullscreen** (`Cmd+Enter`) — **borderless, не native** (с 2026-09-04): `enter_borderless_fullscreen` шлёт `Decorations(false)` + `OuterPosition(monitor.frame.min)` + `InnerSize(monitor.frame.size())` и прячет меню-бар с доком через `mac_window::set_presentation_hidden(true)`. Системный `Fullscreen(true)` больше не вызывается: он создаёт отдельный Space, и при выходе окно оставалось приписанным к схлопнувшемуся Space — WindowServer переставал его показывать (в `CGWindowListCopyWindowInfo` окна нет вовсе), а `NSWindow` при этом отвечал `visible=true, onActiveSpace=true, alpha=1` и отдавал ровно те координаты, которые мы ставили. Проверять успех изнутри процесса было нечем, а починить — тем более: фильтрация по Spaces идёт после сортировки по уровням, так что `makeKeyAndOrderFront:` / `orderFrontRegardless` / `MoveToActiveSpace` не помогают. Невалидный `preferred_monitor` (отключённый монитор) → накрываем дисплей, на котором окно сейчас, + status «Selected monitor unavailable».
- **Возврат окна после fullscreen** (`pre_fullscreen_geometry` → `PendingRestore` → `drain_pending_position_restore`): позиция + inner size снимаются **перед** входом, в обеих ветках — раньше снимок делался только когда `preferred_monitor` резолвился, и любой fallback-fullscreen выходил без восстановления вообще. При выходе (`exit_borderless_fullscreen`: вернуть меню-бар, `Decorations(true)`, страховочный `Fullscreen(false)` на случай системного ⌃⌘F) первая попытка ждёт `FULLSCREEN_SETTLE` (600 мс), дальше **проверяем фактическую позицию и повторяем** каждые 300 мс до 6 раз с допуском 24 pt (`restore_landed`). 🔴 Позиция читается у AppKit (`mac_window::real_outer_rect`), а НЕ из `viewport().outer_rect`: последний отражает нашу же отправленную команду, поэтому проверка по нему всегда «успешна» и retry завершался на первой итерации. Исчерпали попытки → `mac_window::set_outer_rect` напрямую через `setFrame:display:` — одиночный fire-and-forget терялся, если transition шёл дольше задержки, и окно оставалось на fullscreen-дисплее вместо того, с которого его открыли. Вместе с `OuterPosition` шлём `InnerSize` (fullscreen меняет размер) и `Focus` (после Spaces окно всплывает за чужими окнами). Исчерпали попытки → `move_onto_primary_if_offscreen`.
- **Внешний выход из fullscreen** (`sync_fullscreen_state`): зелёная кнопка, ⌃⌘F и Mission Control меняют состояние мимо `toggle_fullscreen`, и флаг `self.fullscreen` расходился с реальностью — exit-путь не выполнялся вовсе. Теперь каждый кадр сверяемся с `viewport().fullscreen`; расхождение в течение `FULLSCREEN_CONFIRM_TIMEOUT` (2 с) после нашей собственной команды игнорируется (winit подтверждает через кадр-два), устойчивое — принимается за пользовательское, с тем же release-capture + restore. Снимка при внешнем входе нет (outer rect уже во весь экран), поэтому fallback — геометрия из `config.toml`.
- **Auto-engage/release capture при fullscreen.** `toggle_fullscreen` при входе делает `if !self.capturing { self.toggle_capture() }`, при выходе — обратное (до отправки `Fullscreen(false)` чтобы успели отпустить модификаторы). Идея: fullscreen ≡ «я хочу управлять Host'ом», без второго хоткея не должно быть промежуточного состояния «fullscreen без capture».
- **Window geometry persistence**: outer position + inner size пишутся в `config.toml` (`window_x/y/w/h`, целые points) после того как окно 800 мс стоит на месте, плюс финальный flush в `eframe::App::on_exit` (move + сразу Cmd+Q). При старте `main.rs` отдаёт их в `ViewportBuilder::with_position`/`with_inner_size`. Без этого позиции нет вообще и AppKit кладёт окно на произвольный дисплей каждый запуск. Fullscreen не сэмплится (там outer rect = весь экран), как и промежуток пока `pending_position_restore` двигает окно после выхода из fullscreen. Запись идёт через `config::save_window_geometry`, который **перечитывает файл с диска** и правит только четыре поля — иначе сдвиг окна коммитил бы несохранённые правки из Settings-панели. Через 800 мс после старта — одноразовая проверка `rescue_offscreen_window`: если окно не пересекается ни с одним живым монитором хотя бы на 120×40 pt (закрыли на внешнем дисплее, вернулись без него), оно переезжает на primary.
- **Dock-icon pinning** (`force_dock_icon_from_bundle` в `main.rs`): winit/eframe иногда оставляют Dock с generic exec-иконкой через ~2с после launch. Загружаем `AppIcon.icns` из bundle через NSBundle/NSImage и зовём `[NSApp setApplicationIconImage:]` + `[NSApp setActivationPolicy:Regular]` из creator-callback'а eframe. Дополнительно `reapply_dock_icon_if_needed` пере-применяет иконку 4× в течение 10с из `update()` — это перебивает любое позднее переписывание системой/winit'ом.
- **Иконка**: `assets/icon-source.png` (1024×1024) → `Contents/Resources/AppIcon.icns` через `sips` + `iconutil` в build-mac-app.sh. Рисунок — контур монитора со стрелкой курсора на тёмно-зелёной подложке; прежняя белая «W» на синем читалась как Microsoft Word. Тот же исходник кормит Windows-.ico (`scripts/icogen`), а трей-глифы (`assets/tray-*.png`) рисуются отдельно (`scripts/generate-tray-icons.swift`) — на 16 px курсор и контур сливаются, поэтому там упрощённый силуэт.
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
