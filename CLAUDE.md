# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

WireDesk — утилита для удалённого управления мышью, клавиатурой и clipboard на Windows-машине через serial-соединение (без сети). Видео — отдельно через HDMI capture card.

Контекст: на Host (Windows 11) стоит «Континент-АП» (СКЗИ), который через WFP-фильтры на уровне ядра блокирует **всю IP-связь** мимо своего туннеля — включая локальный LAN (Wi-Fi, Ethernet) и любой USB CDC NCM / Plugable bridge cable / Thunderbolt Networking, потому что все они создают сетевой интерфейс. Подтверждено живым тестом 2026-05-02 (см. `docs/briefs/ft232h-upgrade.md`): Win и Mac в одной Wi-Fi 192.168.1.0/24, route table показывает default через Wi-Fi, но `ping 192.168.1.98` → `General failure`, `Test-NetConnection ... -Port 5001` → `TcpTestSucceeded: False`. Допустимы только non-network каналы: USB CDC ACM (текущий serial), WinUSB / libusb bulk, USB HID — Континент их не трогает.

**Статус:** MVP+ работает end-to-end. Соединение, мышь, клавиатура (включая кириллицу), переключение языка через Cmd+Space, двунаправленный буфер обмена через Cmd+C/Cmd+V (текст + PNG-картинки до 1 MB encoded; LRU text history + раздельные slots per-format гасят Whispr-Flow / clipboard-manager echo loops; системные шорткаты перехватываются на macOS-уровне через CGEventTap, FlagsChanged events pass-through так что Ctrl+Option-style modifier-only hotkeys работают и в capture mode). **Karabiner-Elements `left_command ↔ left_option` compensation** через Settings toggle — physical events swap'аются перед forward'ом, hotkey detection accept'ает либо Cmd, либо Option flag. **Synthetic Cmd+V dispatcher** (Whispr Flow): synthetic events детектятся через `EVENT_SOURCE_STATE_ID`, очередятся в `synth_tx`, dispatcher ждёт окончания Mac→Host clipboard sync (4s max + 400ms grace) перед emit'ом — Host paste'ит actual recognized text, не previous. Tap kicks poll thread на synthetic для немедленного pickup'а нового clipboard'а; CLIP_POLL_INTERVAL 200ms. Fullscreen по Cmd+Enter с per-monitor selection и auto-engage/release capture. Mac UI: progress bars с **Cancel-кнопкой** в окне и в capture banner, NSStatusItem в menu bar (W / ↑% / ↓%), Settings → System (`Swap ⌥/⌘`, `Save & Restart`) и Clipboard 4-checkbox toggle, ScrollArea для длинного Settings. Win host: tray-agent (nwg) с **Restart** entry, Save & Restart, balloon notification на oversize image, double-click tray открывает Settings. **Adaptive heartbeat timeout** 6s/30s — продлевается во время clipboard transfer'а чтобы heartbeat не тонул в chunk traffic'е. 147 client + 93 host тестов проходят.

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
- Polling раз в 500мс. **Probing text + image в одном tick'е** — OS clipboard может содержать оба формата (typical Cmd+C на rich content). Каждый dedup'ится в свой slot независимо.
- **Два формата:** `FORMAT_TEXT_UTF8 = 0` (UTF-8 строка, лимит 256 KB), `FORMAT_PNG_IMAGE = 1` (PNG-encoded RGBA, лимит `MAX_IMAGE_BYTES = 1 MB` после encode). Константы — в `wiredesk-protocol::message`.
- ClipOffer { format, total_len } + N×ClipChunk { index, data ≤ 256B }. Сборка через `BTreeMap<u16, Vec<u8>>` — устойчиво к out-of-order.
- **Loop avoidance — `LastSeen` struct** с раздельными слотами per-type: `text_history: VecDeque<u64>` (LRU, последние 4 hash'а), `image: Option<u64>`, `oversize_image: Option<u64>`. Mac `Arc<Mutex<LastSeen>>`, Host plain field. Single-slot enum (старый `LastKind`) ломался: (a) text-write erased image hash → loop, (b) Whispr-style inject pattern (save→write→paste→restore) re-shлёт `prev` после каждого цикла. LRU text-history покрывает Whispr `prev → new → prev → newer → prev` без resend'ов. Хэш для image считается **от RGBA bytes**, не от encoded PNG: round-trip arboard PNG↔RGBA нестабилен (NSPasteboard TIFF round-trip меняет байты).
- **Pre-stamp on startup.** При создании poll thread (Mac) и `ClipboardSync::with_counters` (Host) текущий clipboard читается, hash стампится в `LastSeen` БЕЗ отправки. Без этого каждый restart re-uploads то что юзер оставил в clipboard от прошлой сессии.
- **Image encode/decode:** `image 0.25` (`default-features=false, features=["png"]`), helpers `encode_rgba_to_png` / `decode_png_to_rgba` дублируются на обеих сторонах. Encode в poll thread (~50–150 ms). Decode имеет `Limits::max_alloc = 64 MB` + post-decode проверка `(w*h*4) ≤ 64 MB` (PNG-bomb защита: палеточный 8K×8K decode'ит в 256 MB RGBA).
- **Settings → Clipboard panel** (Mac UI): 4 независимых runtime-toggle через `Arc<AtomicBool>` — `send_images`, `receive_images`, `send_text`, `receive_text`. Без рестарта. Полезно для apps вроде Whispr Flow / Maccy которые часто пишут в clipboard.
- **Status UI на Mac:** (1) `format_progress("Sending clipboard", cur, total)` рендерится как `egui::ProgressBar` с inline текстом — в chrome panel И в capture banner (для fullscreen где menu bar скрыт macOS). (2) `NSStatusItem` справа от часов через `objc2-app-kit::NSStatusBar::systemStatusBar` + `dispatch_async_f` на main queue. Idle: «W», active: «↑43%» / «↓67%». Click handler — TODO (custom NSObject subclass через `objc2::declare_class!` нужен).
- **Tray balloon notification (Win)** при oversize: `SessionStatus::Notification(String)` slot в `StatusState` — отдельно от persistent `Connected/Waiting/Disconnected`, не overwrites tray icon color и settings status row. Surface через `nwg::TrayNotification::show(msg, title, WARNING_ICON|LARGE_ICON, None)`.
- **Tray double-click → Settings (Win):** nwg 1.0.13 не имеет нативного double-click event для tray. Workaround: `OnMousePress(MousePressLeftUp)` + `Cell<Option<Instant>>` tracking previous up — два up в окне 500ms = double-click.
- **Pass-through modifier-only events в capture mode.** `CGEventType::FlagsChanged` callback теперь возвращает `Keep` (не `Drop`) после forward'а scancodes на Host. Это позволяет Whispr Flow / push-to-talk dictation apps trigger'нуться на Ctrl+Option, при этом letter keys всё ещё intercept'ятся через `KeyDown` → Drop.
- **Edge case: interleaved offers.** Новый ClipOffer пришёл во время незавершённой reassembly → `log::warn!("incoming offer aborted previous reassembly")` + `received.clear()` + reset counters.
- **Edge case: peer disconnect.** При `TransportEvent::Disconnected` (Mac) или потере связи (Host) — `IncomingClipboard::reset()` обнуляет expected_len / format / received / counters. Sender'ская `last_kind` сохраняется (после reconnect не нужно повторно слать тот же контент).
- **Edge case: oversized peer offer.** `on_offer` отвергает `total_len > MAX_*` ДО reassembly arming — без этого peer мог запросить 4GB allocation в `Vec::with_capacity` через корраптный/враждебный offer.
- **Edge case: non-contiguous chunks / length mismatch.** `commit()` проверяет (a) chunk indices contiguous 0..N, (b) reassembled `buf.len() == expected_len`. Иначе log warn + reset. Защита от silent corruption.
- **TransferOverlay (Win) отключён** в main.rs. Topmost popup-window даже invisible забирал z-order у других окон (Total Commander не активировался кликом). Прогресс на host'е — только в логе + balloon notification на oversize.
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
- Client → `ShellOpen { shell }` — Host спавнит подпроцесс с `Stdio::piped()` (powershell/cmd на Windows, bash/zsh на Unix). На Win добавлен `CREATE_NO_WINDOW` чтобы child не показывал console window.
- `ShellInput { data }` — байты в stdin подпроцесса.
- `ShellOutput { data }` — байты из stdout/stderr, чанки по 480 байт.
- `ShellClose` — закрывает stdin (EOF); `ShellExit { code }` — на выходе процесса.
- Line-based MVP **без PTY**. PowerShell `-NoExit` игнорирует EOF на stdin → ShellClose теперь делает `sh.close()` **и** force `kill()` чтобы host's slot гарантированно освобождался. На re-Hello host тоже kill'ает leftover shell. Term шлёт `Disconnect` после `ShellClose` чтобы host не ждал heartbeat timeout.
- Без PTY: vim/htop/sudo с паролем работают только частично; ssh нужен с `-tt` или `RequestTTY force` в `~/.ssh/config` иначе remote bash идёт non-interactive (нет `.bashrc`/PS1/alias'ов). Полный TTY ждёт ConPTY-рефактора через `portable-pty`.

**Два frontend'а к этому каналу:**
- **GUI shell-panel** в `wiredesk-client` (Settings collapsing → Terminal). Командная строка с `id_salt("shell_input")`. После Enter автоматический `request_focus()` чтобы поле не теряло фокус (без этого — `lost_focus()`-pattern, и пользователь должен кликать перед каждой следующей командой). При первом open `shell_just_opened` flag запрашивает focus на следующем frame'е.
- **`wiredesk-term`** CLI (отдельный бинарь). Raw-mode bridge для Ghostty/iTerm. Serial-port расщеплён через `Transport::try_clone()` на independent reader/writer handles — reader thread больше не держит mutex на blocking recv'е, stdin keystrokes доходят без latency. **Три потока**: reader (owns reader handle), main (stdin → cooked-mode buffer → writer.lock), heartbeat (`Heartbeat` каждые 2s через writer.lock). Все три держат общий `stop: AtomicBool`.
  - **Cooked-mode line discipline на client'е** (стандарт для serial-bridge без PTY на той стороне): `line_buf: Vec<u8>` копит байты, `line_cells: usize` (display-cell count, считается по lead-byte'ам UTF-8). Printable byte → append + local echo. `0x7F` (Backspace) → `pop_utf8_char` (UTF-8 aware! Russian "д" = 2 bytes erased atomically) + `\b \b` локально, **не** forward на host. `\r` → erase locally-echoed (`\b \b` × line_cells) чтобы host's echo не дублировал, отправить `line_buf + b'\n'`. `0x03/0x04` (Ctrl+C/D) — bypass буфера, immediate forward. Без этого PS in pipe-mode echo'ит только `\x20` на BS → trailing space, видимо ломая всю последующую typing.
  - **Output translation `\n → \r\n`** на reader thread'е через `translate_output_for_terminal(input, last_was_cr)` — Linux/SSH'd output использует bare `\n`, raw-mode terminal без line discipline даёт лестницу вправо. State `last_was_cr` carries across chunks (CRLF spanning packet boundary не expand'ится в CRCRLF).
  - Banner после handshake — `format_connected_banner(host_name, w, h)` (pure helper, тесты).
  - Hotkey cheatsheet печатается после banner'а: `Ctrl+]` exit (telnet/nc convention — позволяет forward'ить Ctrl+C/Ctrl+D на host'а).
- GUI и CLI **взаимоисключающие** — оба открывают serial-порт. Multiplex-daemon вне scope MVP.

### Key design decisions

- **Scancodes, not VK codes** — ввод как hardware scancodes, работает независимо от раскладки Host (включая кириллицу).
- **Extended scancodes** (0xE0xx) — в SendInput требуют `KEYEVENTF_EXTENDEDKEY` flag.
- **Cmd → Ctrl** mapping в `egui_modifiers_to_u8` и `cg_flag_change_to_scancodes`. Win-key combos (Win+Space, Win+L) — через CGEventTap они теперь работают напрямую (Cmd+Space на Mac → Win+Space на Host), но кнопки в UI оставлены как fallback для случая когда permission ещё не granted.
- **Synthetic vs physical events.** Tap callback читает `EventField::EVENT_SOURCE_STATE_ID` (1=HIDSystemState→physical, 0=CombinedSessionState→synthetic). Karabiner-Elements remap'ит на HID-уровне → physical events приходят post-Karabiner. Synthetic CGEventPost (Whispr Flow, TextExpander) bypass'ает Karabiner и несёт литеральный modifier intent. Swap toggle применяется только к physical; synthetic forward'ится со standard mapping'ом + ad-hoc modifier wrap (synthetic'е приходят без preceding FlagsChanged, иначе Host видит orphan letter scancodes).
- **Karabiner ⌥/⌘ swap toggle.** `swap_option_command: bool` config поднимает `cg_flag_change_to_scancodes_swapped` для FlagsChanged forward'а и `disable()` cleanup'а — Cmd flag → Alt scancode, Option flag → Ctrl scancode. Hotkey detection (`is_cmd_enter`/`is_release_capture`) при swap=true принимает либо `CG_FLAG_COMMAND`, либо `CG_FLAG_ALT` (но не оба) — Cmd+Esc/Cmd+Enter работают на той же физической кнопке независимо от того remap'нута ли клавиатура.
- **Synthetic Cmd+V dispatcher** (`apps/wiredesk-client/src/main.rs`). Whispr Flow's Cmd+V опережает Mac→Host clipboard sync — Host paste'ит prev. Решение: tap не emit'ит synthetic Cmd+V напрямую — упаковывает в `SyntheticCombo` (`Vec<Packet>` из modifier-press + key-press) и push'ит в `synth_tx`. Dispatcher thread ждёт пока poll thread сбросит `outgoing_text_in_flight=false` (max 4s), плюс grace 400ms (для Host commit), потом emit'ит. Дополнительно tap kicks poll через `poll_kick_tx` mpsc channel — poll wakes immediately и читает clipboard, не дожидаясь sleep'а. CLIP_POLL_INTERVAL 200ms.
- **Adaptive heartbeat timeout (host)**. `Session::heartbeat_timeout()` возвращает 30s когда `clipboard.transfer_in_flight()` (есть active reassembly или непустой `pending_outbox`), иначе 6s. CH340 bidirectional saturation топит heartbeats peer'а во время image transfer'а и строгий 6s давал false-positive disconnect'ы.
- **115200 baud, не 921600** — на дешёвых CH340 с Dupont-проводами 921600 даёт single-bit corruption (видели "bad magic" с XOR 0x80). 115200 надёжно. Bandwidth budget ~11 KB/s, реально ~1 KB/s для ввода + редкие всплески под clipboard/shell.
- **Leading 0x00 + drain on open** — серьёзный фикс для startup transient: при открытии порта CH340 выпускает мусорный байт, который иначе склеивается с первым кадром. Решено в `SerialTransport::send` (ведущий 0x00) + `SerialTransport::open` (drain OS буфера).
- **MockTransport** — mpsc, протокол-тесты без железа. `try_clone` для Mock возвращает ошибку (не нужно в тестах).
- **Aspect ratio correction** в `InputMapper::normalize_mouse` — letterbox/pillarbox для разной геометрии окна и Host.
- **Cancel-кнопки на progress bars** (UI helper `render_progress_row`, Mac). `outgoing_cancel`/`incoming_cancel: Arc<AtomicBool>` shared с writer/reader threads. Writer drop'ает queued ClipOffer/ClipChunk без записи на провод и self-arms flag после non-clip packet или timeout (queue empty). Reader на первом ClipChunk при cancel=true делает `incoming_clip.reset()` + drop, flag clear'ится на следующем ClipOffer. Лог компактный — один summary INFO в start, один в end с counter'ом (per-chunk даёт 700+ строк за 180 KB image cancel). Без protocol message: Host видит partial offer, self-correct'ится на следующем ClipOffer'е.
- **Self-relaunch helper** `apps/wiredesk-client/src/restart.rs`. На macOS spawn'ит `open -n WireDesk.app` если бинарь внутри bundle, иначе spawn'ит `current_exe`. Затем `std::process::exit(0)`. Используется Save & Restart кнопкой в Settings (Mac). Аналогичный pattern на Win — Restart entry в tray menu делает `Command::new(current_exe).spawn() + nwg::stop_thread_dispatch()`.
- **Second-instance показывает Settings** существующего host'а (вместо MessageBox + exit). Win32 named auto-reset event `WireDeskHostShowSettings` (см. `ui::single_instance::create_show_settings_event` / `signal_show_settings`). Первый процесс CreateEvent + spawn wait-thread, второй — OpenEvent + SetEvent + exit. Wait-thread поднимает `show_settings_pending: AtomicBool` и **piggybacks** на existing status-bridge `nwg::Notice` через `notice.sender().notice()` — nwg 1.0.13 panic'ит на втором Notice anywhere в дереве (см. `feedback_nwg_gotchas.md` #4). OnNotice handler в начале arm'а делает `swap → false` на pending flag и поднимает Settings.
- **`Message::ClipDecline { format }` (proto type 0x23)** — receiver просит sender'а abandon transfer. `IncomingClipboard::on_offer` возвращает `Some(Message::ClipDecline)` когда rejectит из-за settings toggle (`receive_text=false` / `receive_images=false`); reader thread форвардит decline через outgoing_tx. Sender (host: `ClipboardSync::cancel_outgoing()` дренит `pending_outbox`; client: set `outgoing_cancel: AtomicBool` который writer thread наблюдает) — drop'ает все pending chunks. Без этого toggle-off вызывал ~75 sec wire-saturation на 1 MB image (host всё равно шлёт chunks), что starved'ил TX (mouse / heartbeats) → false-positive heartbeat timeout disconnect.
- **CREATE_NO_WINDOW для shell child** в `apps/wiredesk-host/src/shell.rs`. Win-host с `windows_subsystem = "windows"` — без console; default child создаёт свою console window. Flag (0x0800_0000) на `Command::creation_flags` подавляет это так что ShellOpen не показывает PowerShell window на host'ской HDMI-capture.
- **Embed app icon в .exe** через `winresource` build-dependency. `apps/wiredesk-host/build.rs` gating через `HOST` env triple (only Windows host has `rc.exe`/`windres`) — Mac dev cross-checks compile clean но без icon resource section. Производственная сборка на Win показывает WireDesk-иконку в taskbar/Alt+Tab/Explorer.

### Известные ограничения

- **Ctrl+Alt+Del** через SendInput не сработает на Windows (защищено ядром, нужен SAS API в SYSTEM-сервисе или Group Policy `SoftwareSASGeneration`). Кнопка в UI есть, но ничего не делает реально. Альтернативы — Win+L (lock), Ctrl+Shift+Esc (Task Manager).
- **macOS Secure Input** — поля паролей в любом приложении на Mac отключают CGEventTap системно. Capture-mode перестаёт работать пока окно с паролем активно. Workaround — переключиться в другое окно перед стартом capture.
- **Accessibility permission** требуется и привязана к binary path. После перекомпиляции в новую папку — заново добавить в System Settings → Privacy & Security → Accessibility.
- **Файлы (file URLs / CF_HDROP)** — не передаются. Картинки PNG передаются (≤1 MB encoded; FullHD-скриншот ~50–100 сек на 11 KB/s wire).
- **Видео** — никогда. Ставь HDMI capture card отдельно.
- **Save+Restart pattern**: changes в settings UI требуют перезапуск процесса (нет live-reconnect supervisor'а). Это компромисс ради простоты — race conditions с открытым serial-портом и работающей session избегаются.
- **Mac autostart** — не реализован (только manual launch из дока / Spotlight). Login Items / launchctl plist — follow-up.
- **Code signing / нотарификация .app** — не делается. Gatekeeper при первом запуске требует «правый-клик → Open» и подтверждение.
- **Single-instance** на Win'е: при втором запуске exe — открывается Settings существующего процесса (через named auto-reset event), второй процесс молча выходит.
- **App icon в .exe** embed'ится только при сборке **на Windows** (rc.exe / windres needed). При cross-compile с macOS — иконка отсутствует. Полное решение — собирать на Win-машине; build.rs sets cargo warning если иконку не удалось встроить.
- **Shell без PTY**: vim, htop, sudo с паролем, git interactive editor, ssh без `-tt` (или `RequestTTY force` в `~/.ssh/config`) — broken или partial. Полный TTY ждёт ConPTY-рефактора через `portable-pty` (см. `~/.claude/projects/.../memory/project_conpty_followup.md`).

## Hardware setup

```
Host HDMI → splitter → monitor + capture card → Mac (QuickTime/VLC for video)
Host USB-Serial ←→ null-modem (TX-RX crossed, GND-GND, VCC isolated) ←→ Mac USB-Serial
```

CH340 USB-to-TTL кабели: красный=VCC (изолировать), синий=GND, зелёный=TX, белый=RX. Полная инструкция: `docs/setup.md`.

## Channel speed upgrade — pre-decided plan

Брейншторм 2026-05-02 (session "improve-FT232H") зафиксировал: текущий канал CH340 @ 115200 baud (~11 KB/s) — узкое место для clipboard'а (1 МБ картинка едет ~90 сек). Возможные пути ускорения проанализированы и ранжированы по effort/impact, см. `docs/briefs/ft232h-upgrade.md`.

| План | Что | Effort | Impact | Confidence | Статус |
|---|---|---|---|---|---|
| **A** (выбран) | CH340 → FT232H breakout, baud 115200 → 3 000 000 (до 12 Mbps на FT4232H) | ~1 день, ~$30 железа | ×100, clipboard 1MB <2 сек | **high** | ждёт покупки железа |
| **B** (Plan B) | WinUSB через Pi Zero 2W в gadget mode как мост, custom USB device class | 2–3 недели, ~$25 | ~30 MB/s, потенциально видео | medium | активируется если A флакает |
| **C** (отклонён) | Thunderbolt AIC + TB DMA peer-to-peer вне TCP/IP стека | 1–2 месяца, дорого | 20+ Gbps, экзотика | low | отклонён: нет TB-header'а на B760M, undocumented API |
| **D** (отклонён) | Не делать ничего | 0 | 0 | high | отклонён: clipboard-боль ощутима |

**Закрытые тупики** (зачем-то проверены, чтобы не возвращались в будущих сессиях):
- TCP/UDP по Wi-Fi/Ethernet/Thunderbolt Networking/USB CDC NCM/Plugable bridge cable — **все режутся WFP-фильтрами Континента** на уровне ядра, route-table обманчива. Нет смысла пробовать ни одно из них как канал WireDesk.
- Thunderbolt в принципе — Host-материнка MAXSUN MS-Challenger B760M не имеет TB-header'а, AIC без него работать не будет. Mac mini M4 имеет 3×TB4 40 Gbps, но это бесполезно при отсутствии TB на Win.

**Что делать когда железо приедет:** см. секцию "Первые шаги" в брифе. Никаких архитектурных изменений в коде — только правка `baud` в `config.toml` обеих сторон.

## Plan

`docs/plans/wiredesk-mvp.md` — full MVP plan with protocol spec, etapes, and risk analysis.

`docs/briefs/ft232h-upgrade.md` — бриф апгрейда канала (готов к /planning:make когда железо будет).
