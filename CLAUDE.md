# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

WireDesk — утилита для удалённого управления мышью, клавиатурой и clipboard на Windows-машине через serial-соединение (без сети). Видео — отдельно через HDMI capture card.

Контекст: на Host (Windows 11) стоит ПО "Континент", которое блокирует все сетевые интерфейсы. Serial (COM-порт) не блокируется.

**Статус:** MVP работает end-to-end. Соединение, мышь, клавиатура (включая кириллицу), переключение языка через Cmd+Space, двунаправленный буфер обмена через Cmd+C/Cmd+V (текст + PNG-картинки до 1 MB encoded; системные шорткаты перехватываются на macOS-уровне через CGEventTap), fullscreen по Cmd+Enter с per-monitor selection и auto-engage/release capture — проверено живьём. Launcher UI: tray-агент на Windows (nwg) с auto-detect CH340 + Save & Restart, `.app` bundle на macOS, TOML config на обеих сторонах, file logging + autostart + single-instance на Windows. 211 тестов проходят.

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo build --release --workspace
```

Host компилируется и на macOS (с MockInjector), и на Windows (`WindowsInjector` за `cfg(target_os = "windows")` через crate `windows`). На macOS реальный SendInput не вызывается — для dev-цикла без Windows это нормально.

## Run

Дефолты подобраны под solo-сетап (single user): COM3 на Windows, `/dev/cu.usbserial-120` на Mac, baud 115200, разрешение 2560×1440.

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
- **Settings window** (через tray): port (TextEdit) + кнопка `Detect` (auto-detect CH340 по VID 0x1A86), baud, width/height, чекбокс «Run on startup», кнопка Copy Mac launch command. Кнопки в нижнем button-bar: `Re&start` (сохраняет TOML и спавнит новый процесс через `Command::spawn` + `stop_thread_dispatch`; новый процесс получает mutex через 5×100ms retry-loop) / `&Save` (primary — пишет TOML без рестарта). Save+Restart pattern: изменения требуют перезапуск процесса для apply.
- **Single-instance lock**: named mutex `WireDeskHostSingleton`. Второй запуск показывает «Already running — check tray icon» и выходит.
- **Logs**: `%APPDATA%\WireDesk\host.log.YYYY-MM-DD` через `tracing-appender::rolling::daily`. `tracing-log::LogTracer` мостит legacy `log::*` в tracing, panics через `install_panic_hook()`.

### Client (macOS) — `.app` bundle

```bash
./scripts/build-mac-app.sh
# → target/release/WireDesk.app

open target/release/WireDesk.app
```

- **Settings panel** в chrome-UI (сгруппирована в три `ui.group()` блока — Connection / Display / System): port (combo + free-text), baud, host screen W×H, monitor selection (ComboBox с кэшированным `monitor::list_monitors()` через NSScreen, refresh раз в секунду), client name. Save пишет `~/Library/Application Support/WireDesk/config.toml` и показывает inline toast 3 секунды. В capture/fullscreen settings panel скрыта (info-only screen без интерактивных элементов).
- **Capture-mode UI** (`render_capture_info`): full-width red-tinted banner «● CAPTURING — Cmd+Esc to release» (RichText 20pt, white-on-red) сверху + info-блоки с активными хоткеями. Banner существует чтобы пользователь, смотрящий на HDMI-monitor (Host), сразу понимал что текущие нажатия идут в Windows.
- **Permission screen** (`render_permission_screen`): тексты вынесены в pure helper `permission_steps() -> &'static [&'static str]` (4 шага). Каждый шаг — `ui.group()` с цифрой в кружке слева. Кнопка `Open System Settings` живёт внутри шага 1 (action рядом с инструкцией).
- **Per-monitor fullscreen** (`Cmd+Enter`): если в settings выбран `preferred_monitor` — `toggle_fullscreen` сначала шлёт `ViewportCommand::OuterPosition(monitor.frame.min)`, потом `Fullscreen(true)`; при exit — `Fullscreen(false)` + drained `pending_position_restore` (Pos2 + Instant) с задержкой ~600мс, чтобы Spaces-transition завершился до того как мы попытаемся вернуть окно на исходную позицию (иначе OuterPosition применяется к закрывающемуся Space и окно пропадает). Невалидный индекс (отключённый монитор) → fullscreen на текущем + status «Selected monitor unavailable».
- **Auto-engage/release capture при fullscreen.** `toggle_fullscreen` при входе делает `if !self.capturing { self.toggle_capture() }`, при выходе — обратное (до отправки `Fullscreen(false)` чтобы успели отпустить модификаторы). Идея: fullscreen ≡ «я хочу управлять Host'ом», без второго хоткея не должно быть промежуточного состояния «fullscreen без capture».
- **Dock-icon pinning** (`force_dock_icon_from_bundle` в `main.rs`): winit/eframe иногда оставляют Dock с generic exec-иконкой через ~2с после launch. Загружаем `AppIcon.icns` из bundle через NSBundle/NSImage и зовём `[NSApp setApplicationIconImage:]` + `[NSApp setActivationPolicy:Regular]` из creator-callback'а eframe. Дополнительно `reapply_dock_icon_if_needed` пере-применяет иконку 4× в течение 10с из `update()` — это перебивает любое позднее переписывание системой/winit'ом.
- **Иконка**: `assets/icon-source.png` (1024×1024) → `Contents/Resources/AppIcon.icns` через `sips` + `iconutil` в build-mac-app.sh
- **Info.plist**: `dev.kar43lov.wiredesk`, `LSUIElement=false`, `NSHighResolutionCapable=true`. Gatekeeper при первом запуске — правый-клик → Open
- Source-иконка можно перерисовать через `swift scripts/generate-icon.swift` (Swift+AppKit, без ImageMagick)

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

`wiredesk-client` и `wiredesk-term` взаимоисключающие — оба открывают serial-порт.

## Architecture

Rust workspace с 6 crate:

```
crates/
  wiredesk-core       — WireDeskError, типы (Resolution, MouseButton, Modifiers)
  wiredesk-protocol   — бинарный протокол: Packet, Message (18 типов), COBS framing, CRC-16
  wiredesk-transport  — trait Transport, SerialTransport, MockTransport
apps/
  wiredesk-host       — Windows tray agent: Session + InputInjector + ShellProcess + ClipboardSync + nwg UI (settings + tray + autostart)
  wiredesk-client     — macOS egui app: capture-окно + InputMapper + clipboard poll thread + settings panel
  wiredesk-term       — macOS CLI: raw-mode terminal bridge для Ghostty/iTerm (только shell)
```

### Host module map (`apps/wiredesk-host/src/`)

```
main.rs                — entry, single-instance, init_logging, config merge,
                          run_windows() / run_dev_loop() split
config.rs              — HostConfig, load/save TOML, merge_args via ArgMatches
logging.rs             — tracing-appender rolling daily + LogTracer bridge,
                          install_panic_hook() (separate from init_logging
                          to avoid leaking global hook in tests)
session.rs             — Session<T,I> state machine, current_state(), client_name()
session_thread.rs      — spawn() generic over injector cfg, SessionStatus enum,
                          derive_status() pure helper
ui/
  mod.rs               — module routing, dead_code allows for non-Windows
  format.rs            — pure validators (validate_baud/port/dimension),
                          format_mac_launch_command, status_color,
                          detect_ch340_port + DetectResult enum (VID 0x1A86)
  icons.rs             — shared embedded PNG bytes (ICON_GREEN/YELLOW/GRAY_BYTES)
                          + app-icon.ico bytes — used by tray + settings status
  settings_window.rs   — #[cfg(windows)] nwg builder UI grouped into Frame
                          blocks (Connection / Display / System), bottom
                          button-bar (Save & Restart + Save primary), Detect
                          button, status ImageFrame, runtime icon load via
                          nwg::Icon::builder().source_bin (no PE-resource
                          path due to mingw fallback)
  tray.rs              — #[cfg(windows)] TrayUi using icons.rs constants, popup menu
  autostart.rs         — HKCU\...\Run via windows::Win32::System::Registry
                          (own implementation, no auto-launch crate)
  single_instance.rs   — CreateMutexW("WireDeskHostSingleton") + drop=close,
                          try_acquire_with_retry (5×100ms) для Save & Restart race
  status_bridge.rs     — session_thread → nwg::Notice via Arc<Mutex<SessionStatus>>
```

### Client module map (`apps/wiredesk-client/src/`)

Дополнительно к keyboard_tap.rs / keymap.rs / clipboard.rs:

```
monitor.rs             — NSScreen FFI wrapper (objc2-app-kit) под cfg(macos),
                          MonitorInfo { index, name, frame, size },
                          list_monitors(), resolve_target_monitor(preferred, &monitors)
                          (pure helper для fullscreen orchestration)
```

### Threading (client)

Клиент делит serial-порт на два независимых хэндла через `Transport::try_clone()`:

- **writer_thread** — единственный отправитель. Блокируется на `outgoing_rx.recv_timeout(2s)`. Пакет → отправляет немедленно. Таймаут → шлёт Heartbeat. UI кладёт пакеты в канал и не ждёт.
- **reader_thread** — единственный получатель. recv() в цикле, диспатчит на `events_tx` для UI. Также держит `IncomingClipboard` для сборки входящих ClipChunks.
- **clipboard poll thread** — раз в 500мс читает Mac clipboard, при изменении отправляет ClipOffer + ClipChunks через тот же `outgoing_tx`.
- **keyboard tap thread** (только macOS) — отдельный CFRunLoop, владеет CGEventTap. Подробнее в секции «Keyboard hijack».

Латенси UI→провод ~µs (только время записи в UART, ~100µs).

### Data flow

```
Client (macOS)                          Host (Windows)
  egui captures input                     Session::tick() loop
  → InputMapper.send_*(outgoing_tx)         → recv Packet via serial
  → outgoing_tx (mpsc channel)              → handle_packet
  → writer_thread.send()                    → InputInjector::key_down/mouse_move/...
  → SerialTransport::send()                 → Win32 SendInput API
```

### Protocol (wiredesk-protocol)

Packet: `[magic "WD"][type][flags][seq:u16][len:u16][payload][crc16]`, COBS-framed over serial.

18 message types: HELLO/HELLO_ACK (handshake with screen resolution), 5 input types (mouse move/button/scroll, key down/up), 3 clipboard types (offer/chunk/ack), heartbeat/error/disconnect, 5 shell types (open/input/output/close/exit).

Ввод — fire-and-forget. Clipboard — fire-and-forget chunks (256 байт), reassembly по `index`. ACK-сообщения определены в протоколе, но в текущей реализации не используются (CRC на пакетном уровне даёт достаточную защиту для MVP). Heartbeat: каждые 2 сек, timeout 6 сек (3 пропущенных).

### Clipboard sync

Симметрично на обеих сторонах:
- Polling раз в 500мс. Сначала `get_text()`, при неудаче — `get_image()`.
- **Два формата:** `FORMAT_TEXT_UTF8 = 0` (UTF-8 строка, лимит 256 KB), `FORMAT_PNG_IMAGE = 1` (PNG-encoded RGBA, лимит `MAX_IMAGE_BYTES = 1 MB` после encode). Константы — в `wiredesk-protocol::message`.
- ClipOffer { format, total_len } + N×ClipChunk { index, data ≤ 256B }. Сборка через `BTreeMap<u16, Vec<u8>>` — устойчиво к out-of-order.
- **Loop avoidance** через `enum LastKind { Text(u64), Image(u64), None }` (Mac `Arc<Mutex<>>`, Host plain field — single-threaded tick-loop). Хэш для image считается **от RGBA bytes**, не от encoded PNG: round-trip arboard через PNG нестабилен (compression options дают разные encoded байты, RGBA — стабилен).
- **Image encode/decode:** `image 0.25` (`default-features=false, features=["png"]`), helpers `encode_rgba_to_png` / `decode_png_to_rgba` дублируются на обеих сторонах (CLAUDE.md разрешает duplication для clipboard.rs). Encode выполняется в poll thread (~50–150 ms терпимо при 500 ms cadence). Лимит проверяется **после** encode'а через pure helper `check_image_size(png_len, limit)` — compression ratio из dimensions не предсказать.
- **Status-line counter (Mac).** Четыре `Arc<AtomicU64>` (outgoing/incoming × progress/total) обновляются writer-thread (после каждого ClipChunk send) и `IncomingClipboard::on_chunk`. UI рендерит `format_progress("Sending image", cur, total)` → `"Sending image — 340/780 KB (43%)"` отдельной строкой над status-row. После последнего chunk counter обнуляется sender'ом.
- **Toast при oversize (Mac).** `TransportEvent::Toast(String)` шлётся poll-thread'ом через существующий `events_tx` при `check_image_size == Err`; UI показывает 3 секунды через `transient_toast: Option<(String, Instant)>`. Host пишет `log::info!` для start/finish image-transfer, без toast (нет интерактивного UI).
- **Edge case: interleaved offers.** Новый ClipOffer пришёл во время незавершённой reassembly предыдущего → `log::warn!("incoming offer aborted previous reassembly")` + `received.clear()` + reset counters перед сохранением нового offer'а.
- **Edge case: peer disconnect.** При `TransportEvent::Disconnected` (Mac) или потере связи (Host) — `IncomingClipboard::reset()` обнуляет expected_len / expected_format / received / counters. Sender'ская `last_kind` сохраняется (после reconnect не нужно повторно слать тот же контент).
- Mac side: `apps/wiredesk-client/src/clipboard.rs`. Host side: `apps/wiredesk-host/src/clipboard.rs`. Не вынесено в общий crate — duplication приемлема.

### Keyboard hijack (macOS)

Чтобы перехватывать системные шорткаты типа `Cmd+Space` (которые macOS интерпретирует раньше уровня приложения) — используется `CGEventTap` на сессионном уровне. Без этого `egui` видит только клавиши, которые macOS не успел обработать.

**Permission gate.** Tap требует Accessibility permission (System Settings → Privacy & Security → Accessibility). Без неё tap создаётся, но молча не срабатывает. На старте `keyboard_tap::start()` вызывает `AXIsProcessTrustedWithOptions({prompt: false})` и, если permission нет, возвращает no-op handle. UI показывает экран с инструкцией. После grant'а **обязательно перезапустить процесс** — tap-поток создаётся один раз при `start()`.

**Threading.**
- Отдельный thread `wiredesk-keyboard-tap` с CFRunLoop.
- `CGEventTapCreate` с маской `KeyDown | KeyUp | FlagsChanged | TapDisabledByTimeout | TapDisabledByUserInput`.
- Callback быстрый: проверяет `enabled: Arc<AtomicBool>` + `passive: Arc<AtomicBool>`. Три состояния: ACTIVE (intercept всё, Drop на Host) / PASSIVE (только Cmd+Esc и Cmd+Enter — отправляем `TapEvent::EngageCapture`/`ToggleFullscreen` в UI и Drop, остальное Keep) / IDLE (`enabled=false`, всё Keep — macOS видит ввод как обычно).
- `CGEventFlags` приходят как полный bitmask текущего состояния модификаторов (не diff). `prev_flags: Arc<AtomicU64>` хранит прошлый bitmask, `cg_flag_change_to_scancodes(cur, prev)` выдаёт список (scancode, pressed) для отправки.
- При `kCGEventTapDisabled*` — re-enable через сохранённый `CFMachPortRef` (как `usize` через `Arc<AtomicUsize>`, потому что raw pointer нужно протащить в callback closure).
- Shutdown: `CFRunLoop` сохранён в `Arc<Mutex<Option<CFRunLoop>>>`. `Drop for TapHandle` вызывает `rl.stop()`, join потока с timeout 1с.

**Focus-gating + passive mode.** `WireDeskApp::sync_tap_to_focus()` бежит в каждом `update()`, читает `ctx.input(|i| i.viewport().focused)` и синхронизирует через 3-стейт machine:
- `(focused=true, capturing=true)` → `enable()` (ACTIVE) — всё на Host;
- `(focused=true, capturing=false)` → `enable_passive()` (PASSIVE) — ловим только Cmd+Esc для engage и Cmd+Enter для fullscreen, остальное Mac обрабатывает сам. Без passive AppKit перехватывает Cmd+Esc как Cancel-button accelerator до того как egui увидит keypress, и engage capture хоткеем не работает.
- `(focused=false, _)` → `disable()` (IDLE) — sticky-cleanup, отправляются KeyUp для нажатых модификаторов, чтобы Host не остался с залипшим Ctrl.

**Cmd → Ctrl mapping.** Cmd OR Ctrl на Mac → Ctrl на Windows. Bit 20 (Command) или bit 18 (Control) в CGEventFlags — оба маппятся на единственный Win scancode 0x1D. Если оба нажаты одновременно — не дублируем press/release (см. тесты в `keymap::cg_flag_change_to_scancodes`).

**Локальные хоткеи.**
| Combo | Mac VK | Действие |
|-------|--------|----------|
| `Cmd+Esc` | `0x35` | Toggle capture (вкл/выкл) |
| `Cmd+Enter` | `0x24` | Toggle fullscreen |

Оба перехватываются tap-callback'ом (через `is_release_capture` и `is_cmd_enter`) и не форвардятся на Host. Также детектятся в egui-input для случая когда tap не запущен (out-of-capture, пользователь нажал Cmd+Enter ещё до Capture).

**Файлы.** `apps/wiredesk-client/src/keyboard_tap.rs` (~440 строк), `keymap.rs::cgkeycode_to_scancode`, `keymap.rs::cg_flag_change_to_scancodes`. Deps: `core-graphics 0.25`, `core-foundation 0.10`, `accessibility-sys 0.1` под `cfg(target_os = "macos")`.

**Известные ограничения.**
- macOS Secure Input (поля паролей в любом приложении): tap отключается системой. Workaround — переключиться в другое окно перед capture.
- Permission attaches к binary path. Если перекомпилировал в другую папку — нужно заново добавить в Accessibility list.

### Shell-over-serial

Опциональный канал терминала на том же serial:
- Client → `ShellOpen { shell }` — Host спавнит подпроцесс (powershell/cmd на Windows, bash/zsh на Unix).
- `ShellInput { data }` — байты в stdin подпроцесса.
- `ShellOutput { data }` — байты из stdout/stderr, чанки по 480 байт.
- `ShellClose` — закрывает stdin (EOF); `ShellExit { code }` — на выходе процесса.
- Line-based MVP без PTY: vim, sudo с паролем не работают. SSH с key-based auth работает.

### Key design decisions

- **Scancodes, not VK codes** — ввод как hardware scancodes, работает независимо от раскладки Host (включая кириллицу).
- **Extended scancodes** (0xE0xx) — в SendInput требуют `KEYEVENTF_EXTENDEDKEY` flag.
- **Cmd → Ctrl** mapping в `egui_modifiers_to_u8` и `cg_flag_change_to_scancodes`. Win-key combos (Win+Space, Win+L) — через CGEventTap они теперь работают напрямую (Cmd+Space на Mac → Win+Space на Host), но кнопки в UI оставлены как fallback для случая когда permission ещё не granted.
- **115200 baud, не 921600** — на дешёвых CH340 с Dupont-проводами 921600 даёт single-bit corruption (видели "bad magic" с XOR 0x80). 115200 надёжно. Bandwidth budget ~11 KB/s, реально ~1 KB/s для ввода + редкие всплески под clipboard/shell.
- **Leading 0x00 + drain on open** — серьёзный фикс для startup transient: при открытии порта CH340 выпускает мусорный байт, который иначе склеивается с первым кадром. Решено в `SerialTransport::send` (ведущий 0x00) + `SerialTransport::open` (drain OS буфера).
- **MockTransport** — mpsc, протокол-тесты без железа. `try_clone` для Mock возвращает ошибку (не нужно в тестах).
- **Aspect ratio correction** в `InputMapper::normalize_mouse` — letterbox/pillarbox для разной геометрии окна и Host.

### Известные ограничения

- **Ctrl+Alt+Del** через SendInput не сработает на Windows (защищено ядром, нужен SAS API в SYSTEM-сервисе или Group Policy `SoftwareSASGeneration`). Кнопка в UI есть, но ничего не делает реально. Альтернативы — Win+L (lock), Ctrl+Shift+Esc (Task Manager).
- **macOS Secure Input** — поля паролей в любом приложении на Mac отключают CGEventTap системно. Capture-mode перестаёт работать пока окно с паролем активно. Workaround — переключиться в другое окно перед стартом capture.
- **Accessibility permission** требуется и привязана к binary path. После перекомпиляции в новую папку — заново добавить в System Settings → Privacy & Security → Accessibility.
- **Картинки/файлы в clipboard** — не передаются, только текст.
- **Видео** — никогда. Ставь HDMI capture card отдельно.
- **Save+Restart pattern**: changes в settings UI требуют перезапуск процесса (нет live-reconnect supervisor'а). Это компромисс ради простоты — race conditions с открытым serial-портом и работающей session избегаются.
- **Mac autostart** — не реализован (только manual launch из дока / Spotlight). Login Items / launchctl plist — follow-up.
- **Code signing / нотарификация .app** — не делается. Gatekeeper при первом запуске требует «правый-клик → Open» и подтверждение.
- **Single-instance focus**: при втором запуске host'а на Windows показывается message box и выход. «Поднять» существующее окно tray-приложения требует named pipe IPC — не реализовано (overkill для solo-MVP).
- **App icon в taskbar / Alt+Tab на Windows** — при сборке с macOS dev-машины (без `x86_64-w64-mingw32-windres`) `embed-resource` не используется; иконка прокидывается только через `nwg::Window::builder().icon(...)` runtime-load из `app-icon.ico`. Это даёт иконку в title-bar Settings-окна, но **не** в taskbar / Alt+Tab — там остаётся generic Rust binary иконка. Полное решение — пересобрать на Windows-машине (или установить `mingw-w64`) с включённым `embed-resource` путём в `build.rs`.

## Hardware setup

```
Host HDMI → splitter → monitor + capture card → Mac (QuickTime/VLC for video)
Host USB-Serial ←→ null-modem (TX-RX crossed, GND-GND, VCC isolated) ←→ Mac USB-Serial
```

CH340 USB-to-TTL кабели: красный=VCC (изолировать), синий=GND, зелёный=TX, белый=RX. Полная инструкция: `docs/setup.md`.

## Plan

`docs/plans/wiredesk-mvp.md` — full MVP plan with protocol spec, etapes, and risk analysis.
