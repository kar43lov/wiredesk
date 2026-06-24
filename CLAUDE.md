# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

WireDesk — утилита для удалённого управления мышью, клавиатурой и clipboard на Windows-машине через serial-соединение (без сети). Видео — отдельно через HDMI capture card.

Контекст: на Host (Windows 11) стоит «Континент-АП» (СКЗИ), который через WFP-фильтры на уровне ядра блокирует **всю IP-связь** мимо своего туннеля — включая локальный LAN (Wi-Fi, Ethernet) и любой USB CDC NCM / Plugable bridge cable / Thunderbolt Networking, потому что все они создают сетевой интерфейс. Подтверждено живым тестом 2026-05-02 (см. `docs/briefs/ft232h-upgrade.md`): Win и Mac в одной Wi-Fi 192.168.1.0/24, route table показывает default через Wi-Fi, но `ping 192.168.1.98` → `General failure`, `Test-NetConnection ... -Port 5001` → `TcpTestSucceeded: False`. Допустимы только non-network каналы: USB CDC ACM (текущий serial), WinUSB / libusb bulk, USB HID — Континент их не трогает.

**Статус:** MVP+ работает end-to-end. Все детали реализации (module maps, threading, протокол, clipboard sync, keyboard hijack, shell-over-serial, key design decisions) — в [`docs/architecture.md`](docs/architecture.md). Здесь — обзор и текущие фичи:

- **Transport options:** USB-Serial null-modem (default) или Bluetooth LE (`transport = "bluetooth"` в config.toml). Поддерживаемые null-modem кабели — WCH CH340 (VID 0x1A86, ~11 KB/s @ 115200) **или** FTDI FT232H (VID 0x0403, **до ~300 KB/s @ 3 Mbaud, verified live 2026-05-28** на двух CJMCU-FT232H). На FT232H clipboard 1MB летит ~3 сек вместо ~90 сек на CH340. Никаких изменений в коде кроме `baud` в `config.toml` (обе стороны синхронно). Win11 требует **FTDI CDM driver** с ftdichip.com (для CH340 был CH341SER от WCH). **BLE shipped 2026-05-06, live-measured ~4-5 KB/s — *slower* than serial** на Mac M4 + Win11 reference setup; Continent-АП verified не вмешивается в BLE pathway (AC0 ✓), но реальный throughput не дотянул до brief'овой оценки 30-100 KB/s (bottleneck в btleplug/WinRT GATT layer). **Default остаётся serial**; BLE — fallback "no-cable mode". Подробно: `docs/bluetooth-transport.md`.

- **Ввод:** мышь (вкл. X1/X2 side buttons → Back/Forward через `MOUSEEVENTF_XDOWN/XUP` + `mouseData = XBUTTON1/2`), клавиатура (включая кириллицу через scancodes), системные шорткаты через CGEventTap (Cmd+Space → Win+Space). Karabiner ⌥/⌘ swap toggle в Settings.
- **Clipboard:** двунаправленный текст + PNG до 1 MB encoded + **одиночные файлы до 20 MB** (file URLs на Mac / CF_HDROP на Win). Filename packed в первом chunk как `[u16 LE name_len][name UTF-8][content]`; receive-side sanitize отбрасывает path traversal + Windows reserved names. Files идут в `~/Library/Caches/WireDesk/` (Mac) / `%TEMP%\WireDesk\` (Win) и vacuum'ятся каждые 24h на startup. LRU text history + раздельные slots per-format (text/image/file) гасят Whispr-Flow echo loops. **Отправка файлов Mac→Host — opt-in (галка «Send files», default OFF)**; приём (Host→Mac) и текст/картинки — autosend. 6-checkbox Settings panel для runtime-toggle (send/receive × text/image + receive_files + send_files). Mac детектит Finder-копии через `NSURL.path` (резолвит reference-URL `file:///.file/id=…` в реальный путь). Cancel-кнопка на прогресс-баре. Adaptive heartbeat timeout 6s/30s.
- **Synthetic Cmd+V dispatcher** ждёт окончания Mac→Host clipboard sync (4s max + 400ms grace), Host paste'ит recognized text не prev. CLIP_POLL_INTERVAL 200ms.
- **Outbound text debounce** (`decide_text_send`): текст уходит на провод только когда его hash стабилен **2 поллинга подряд** (≥200ms). Гасит фрагменты copy-on-select терминалов (Ghostty `copy-on-select = clipboard`, iTerm), которые потоково переписывают pasteboard во время drag-выделения — без debounce каждый промежуточный кусок (`ping`, `-n`, …) улетал отдельной clipboard-синхронизацией и Host вставлял случайный обрывок. Synthetic Cmd+V (Whispr-Flow, kick) минует debounce — пишет clipboard однократно, лишний тик не нужен.
- **Fullscreen:** Cmd+Enter, per-monitor selection, auto-engage/release capture.
- **`wd --exec COMMAND`** (non-interactive mode для AI-агентов / scripted use) — single-shot с UUID-tagged sentinel, `--ssh ALIAS` chain, `--timeout N` (default 90s), `--compress` (gzip+base64 stdout, ×5–10 для текстовых выводов; обе path'и bash/--ssh + PS host-direct; exit 125 на decode failure). На timeout печатает last 256 байт wire-buffer'а в stderr (`format_timeout_diagnostic`). Подробно: `docs/wd-exec-usage.md`.
- **Параллельная работа `wd --exec` с активным GUI** (Mac-only, через Unix-socket IPC). GUI поднимает `~/Library/Application Support/WireDesk/wd-exec.sock` на старте; `wd --exec` сначала пробует connect к нему, на success — ходит через GUI'ёвский serial-link через embedded runner; на ENOENT/ECONNREFUSED/2s read-timeout — fallback на legacy direct-open. Backward-compatible: GUI закрыт → behaviour identical pre-implementation. Crate `wiredesk-exec-core` содержит shared sentinel-runner (streaming через `FnMut(&[u8])` callback) + `ExecTransport` trait с двумя impl'ами (`SerialExecTransport` в term, `IpcExecTransport` в client'е). RAII `ExecSlotGuard` гарантирует panic-safe cleanup экзит-event mpsc-slot'а в reader_thread'е.
- **ConPTY** для interactive `wd` (PR #10) — настоящий PTY на Win11 через `portable-pty`. vim/htop/ssh без `-tt`, PSReadLine стрелки + Tab autocomplete. `wd --exec` и GUI shell-panel остаются pipe-mode.
- **UI:** Mac chrome panel с group'ами (Connection / Display / System), capture banner red-tinted, NSStatusItem (W / ↑% / ↓%), ScrollArea Settings. Win tray-agent (nwg): Show Settings / Open Logs / Restart / Quit, balloon notification на oversize image, double-click tray → Settings.
- **MAX_PAYLOAD = 4096** (bumped 512→4096 в `feat/wd-exec-fixes`); matched `MAX_FRAME_SIZE = 8192` в `SerialTransport`. Bluetooth transport использует ту же payload-границу через chunked fragmentation.
- **Auto-recovery канала (frame-error storm).** При физическом сбое FT232H (clock уезжает → оба направления систематически корраптятся: COBS-ошибки, CRC mismatch, бит-флип в magic) канал лечится переоткрытием serial-порта. `StormCounter` в `wiredesk-core` детектит шторм — **10 подряд** recv-ошибок `Protocol` (timeout/idle не считаются и не сбрасывают; сброс — только на успешно декодированный пакет). Обе стороны переоткрывают порт сами, т.к. неизвестно чей чип сбился: переоткрытие на сбитой стороне чинит канал, вторая переподключается через обычный Hello/HelloAck. **Win host** (`session_thread.rs`): на storm выходит из tick-loop, разбирает Session (`into_injector`), sleep 500ms (Win serial close асинхронный), reopen с backoff 1s→2s→4s→8s→16s→30s cap. **Mac client** (`link.rs` LinkSupervisor): полный in-process reconnect — respawn reader/writer, reopen с тем же backoff; `outgoing_rx` переживает реконнект (клоны `outgoing_tx` остаются валидными); `link_up: Arc<AtomicBool>` гейтит IPC (`wd --exec` во время reconnect → `IpcResponse::TransportUnavailable` → term exit 125); UI-статус `Reconnecting… (attempt N)`, в capture-fullscreen жёлтый banner «HOST LINK LOST». Закрывает старый бриф `mac-auto-reconnect.md` (host quit / unplug / любой disconnect). Ручной перезапуск вслепую больше не нужен.
- **Тесты:** 253 client + 158 host + 84 exec-core + 88 protocol + 41 transport (BLE+factory+reconnect) + 23 term + 18 wiredesk-core = 665 passing (+5 ignored). Host-side flaky на parallel runner'е macOS — `cargo test --workspace -- --test-threads=1`.

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
- **Settings window** (через tray): port — выпадающий список обнаруженных портов с подписью чипа («COM7 — FT232H», «COM5 — CH340», «COM9 — USB serial 1234:ABCD», «COM1 — serial») + free-text manual override («or type:»), обновляется при каждом открытии окна и по `Detect`. Кнопка `Detect` (`ui::format::classify_ports` / `target_indices`) перечисляет все serial-порты, заполняет дропдаун и авто-выбирает WireDesk-адаптер — CH340 VID 0x1A86 **или** FTDI VID 0x0403 (FT232H/R/2232/4232); при двух адаптерах берёт первый и просит выбрать. Выбор в дропдауне копирует bare-COM в manual-поле (canonical value, читается при Save). Дальше: baud, width/height, чекбокс «Run on startup», кнопка Copy Mac launch command. Кнопки в нижнем button-bar: `Re&start` (сохраняет TOML и спавнит новый процесс через `Command::spawn` + `stop_thread_dispatch`; новый процесс получает mutex через 5×100ms retry-loop) / `&Save` (primary — пишет TOML без рестарта). Save+Restart pattern: большинство изменений требуют перезапуск для apply. **Исключение — чекбокс «Receive files»: применяется live по Save** (без Restart) через shared `Arc<AtomicBool>` (`main` → session_thread + Save-handler); порт/baud/размер/transport по-прежнему требуют Restart.
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
- **Per-monitor fullscreen** (`Cmd+Enter`): если в settings выбран `preferred_monitor` — `toggle_fullscreen` сначала шлёт `ViewportCommand::OuterPosition(monitor.frame.min)`, потом `Fullscreen(true)`; при exit — `Fullscreen(false)` + drained `pending_position_restore` (Pos2 + Instant) с задержкой ~600мс, чтобы Spaces-transition завершился до того как мы попытаемся вернуть окно на исходную позицию (иначе OuterPosition применяется к закрывающемуся Space и окно пропадает). Невалидный индекс (отключённый монитор) → fullscreen на текущем + status «Selected monitor unavailable».
- **Auto-engage/release capture при fullscreen.** `toggle_fullscreen` при входе делает `if !self.capturing { self.toggle_capture() }`, при выходе — обратное (до отправки `Fullscreen(false)` чтобы успели отпустить модификаторы). Идея: fullscreen ≡ «я хочу управлять Host'ом», без второго хоткея не должно быть промежуточного состояния «fullscreen без capture».
- **Dock-icon pinning** (`force_dock_icon_from_bundle` в `main.rs`): winit/eframe иногда оставляют Dock с generic exec-иконкой через ~2с после launch. Загружаем `AppIcon.icns` из bundle через NSBundle/NSImage и зовём `[NSApp setApplicationIconImage:]` + `[NSApp setActivationPolicy:Regular]` из creator-callback'а eframe. Дополнительно `reapply_dock_icon_if_needed` пере-применяет иконку 4× в течение 10с из `update()` — это перебивает любое позднее переписывание системой/winit'ом.
- **Иконка**: `assets/icon-source.png` (1024×1024) → `Contents/Resources/AppIcon.icns` через `sips` + `iconutil` в build-mac-app.sh
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

`wiredesk-client` и `wiredesk-term` взаимоисключающие — оба открывают serial-порт.

## Architecture

Rust workspace с 6 crate:

```
crates/
  wiredesk-core       — WireDeskError, типы (Resolution, MouseButton, Modifiers)
  wiredesk-protocol   — бинарный протокол: Packet, Message (20 типов), COBS framing, CRC-16
  wiredesk-transport  — trait Transport, SerialTransport, MockTransport
apps/
  wiredesk-host       — Windows tray agent: Session + InputInjector + ShellProcess + ClipboardSync + nwg UI (settings + tray + autostart)
  wiredesk-client     — macOS egui app: capture-окно + InputMapper + clipboard poll thread + settings panel
  wiredesk-term       — macOS CLI: raw-mode terminal bridge для Ghostty/iTerm (только shell)
```

Полный архитектурный разбор (module maps Host + Client, threading, data flow, protocol details, clipboard sync, keyboard hijack, shell-over-serial, key design decisions) — в [`docs/architecture.md`](docs/architecture.md). Ключевые точки ниже:

- **Threading клиента:** writer / reader / clipboard poll / keyboard tap (CFRunLoop) — serial-порт расщеплён через `Transport::try_clone()`. Латенси UI→провод ~µs.
- **Протокол:** binary, COBS-framed, CRC-16 packet-level. Header `[magic][type][flags][seq:u16][len:u16]`. **MAX_PAYLOAD = 4096** + matched `MAX_FRAME_SIZE = 8192` в SerialTransport.
- **Heartbeat:** 2 сек, idle timeout 6с / busy 30с. Adaptive когда `clipboard.transfer_in_flight()` **или** `self.shell.is_some()` (PR #20). Без расширения на shell-busy heavy-output cmd'ы (`Get-EventLog`, ES `_search`) saturate'или wire → Mac heartbeat-sender falls behind → host disconnect через 6s.
- **IPC post-run drain** (`apps/wiredesk-client/src/ipc.rs`, PR #21): после ShellClose handler ждёт пока wire не стихнет (2s idle deadline, ShellExit short-circuit, 30s hard cap) **прежде чем** освободить `single_inflight`. Без этого host продолжал шипить tail предыдущей cmd-output (~30s) и next request попадал на dirty wire → cascade timeouts. Trade-off: +2s overhead на каждый `wd --exec` (даже Ok).
- **Shell-over-serial:** per-session opcode-discriminator (`ShellOpen=0x40` pipe vs `ShellOpenPty=0x45` PTY + `PtyResize=0x46`). Pipe-mode для `wd --exec` и GUI shell-panel; PTY-mode для interactive `wd` (Win11 only через `portable-pty`).
- **Loop avoidance в clipboard:** `LastSeen` с раздельными slots per-format + LRU text history (4 entries) против Whispr-style inject pattern. Hash от RGBA bytes (PNG round-trip нестабилен на macOS).

## Известные ограничения

- **Ctrl+Alt+Del** через SendInput не сработает на Windows (защищено ядром, нужен SAS API в SYSTEM-сервисе или Group Policy `SoftwareSASGeneration`). Кнопка в UI есть, но ничего не делает реально. Альтернативы — Win+L (lock), Ctrl+Shift+Esc (Task Manager).
- **macOS Secure Input** — поля паролей в любом приложении на Mac отключают CGEventTap системно. Capture-mode перестаёт работать пока окно с паролем активно. Workaround — переключиться в другое окно перед стартом capture.
- **Accessibility permission** требуется и привязана к binary path. После перекомпиляции в новую папку — заново добавить в System Settings → Privacy & Security → Accessibility.
- **Файлы — single-file, ≤20 MB.** Multi-file selection silently skip'ается (Phase 2 follow-up: `docs/briefs/clipboard-files-multi.md`). Directories не поддерживаются (Phase 2: zip on-fly). Картинки PNG передаются ≤1 MB encoded (FullHD-скриншот ~3 сек на FT232H @ 3 Mbaud / ~50–100 сек на CH340 @ 115200).
- **Видео** — никогда. Ставь HDMI capture card отдельно.
- **Save+Restart pattern**: большинство changes в settings UI требуют перезапуск процесса. LinkSupervisor реконнектит на тот же порт/baud (storm/disconnect-recovery), но смену настроек (другой порт, baud, разрешение) live не подхватывает — это компромисс ради простоты, race conditions с открытым serial-портом избегаются. **Исключение — host-чекбокс «Receive files»: применяется live по Save** (shared `Arc<AtomicBool>`, session_thread читает на каждом clipboard-offer) — Restart для приёма файлов не нужен.
- **Mac autostart** — не реализован (только manual launch из дока / Spotlight). Login Items / launchctl plist — follow-up.
- **Outbound text debounce — ~400ms окно для physical Cmd+V** (accepted limitation): debounce задерживает свежескопированный текст до подтверждения стабильности; в это окно *физический* Cmd+V (форвардится на Host сразу, без wait-gate в отличие от synthetic) вставил бы предыдущий clipboard. На практике недостижимо: форвард физического Cmd+V на Host требует capture-mode, а создание нового Mac-clipboard (мышь-выделение в Ghostty / Cmd+C) требует быть ВНЕ capture — состояния взаимоисключающи, переход дольше окна. Полный gate физического paste (как synthetic) добавил бы латентность каждому Ctrl+V на Host — непропорционально. См. `decide_text_send` в `clipboard.rs`.
- **Outbound text debounce — mixed-format clipboard** (accepted limitation): если ОДИН clipboard-item несёт И текст И картинку/файл (rich-копия из браузера/Word), отложенный на тик text уходит ПОСЛЕ image/file; Host коммитит каждый формат через `EmptyClipboard` (single-format clipboard), поэтому поздний text перезаписывает картинку → Host получает текст вместо image (до debounce приходила картинка). Чистый текст (терминальный copy-on-select) и чистые скриншоты не затронуты — регрессирует только mixed-item, что редко в терминальном workflow. Полный фикс требует NSPasteboard type-probing — непропорционально. См. known limitation #2 в docstring `decide_text_send`.
- **Code signing / нотарификация .app** — не делается. Gatekeeper при первом запуске требует «правый-клик → Open» и подтверждение.
- **Single-instance** на Win'е: при втором запуске exe — открывается Settings существующего процесса (через named auto-reset event), второй процесс молча выходит.
- **App icon в .exe** embed'ится только при сборке **на Windows** (rc.exe / windres needed). При cross-compile с macOS — иконка отсутствует. Полное решение — собирать на Win-машине; build.rs sets cargo warning если иконку не удалось встроить.
- **PTY-mode только для interactive `wd`**, не для `wd --exec` и не для GUI shell-panel — они остаются pipe-based. `wd --exec` это design choice (sentinel-detection требует чистого stdout); GUI shell — egui без ANSI parser'а, escape-codes показывались бы как мусор. ConPTY emulator в egui — отдельный follow-up.
- **PTY-mode только на Windows host'е**. На Mac/Linux host (если кто-то соберёт) `ShellOpenPty` возвращает `Error("PTY-mode shell is only supported on Windows host")`. Mac dev'ит pipe-only — реальный PTY проверяется live на Win11.
- **Параллельный cargo test флакает на macOS** для host'-пакета (~50% SIGABRT) — это pre-existing baseline issue (воспроизводится на чистом master). Использовать `cargo test --workspace -- --test-threads=1` для надёжного запуска.
- **macOS menu bar reveal в native fullscreen** — в native (Spaces-style) fullscreen `NSApplicationPresentationHideMenuBar` системно игнорится, approach к top edge всегда показывает меню. Закрытый тупик через `setPresentationOptions`; решается только borderless-fullscreen (NSWindow без `NSWindowStyleMaskFullScreen`) — отдельный follow-up за пределами текущего eframe-API.

## Hardware setup

```
Host HDMI → splitter → monitor + capture card → Mac (QuickTime/VLC for video)
Host USB-Serial ←→ null-modem (TX-RX crossed, GND-GND, VCC isolated) ←→ Mac USB-Serial
```

CH340 USB-to-TTL кабели: красный=VCC (изолировать), синий=GND, зелёный=TX, белый=RX. Полная инструкция: `docs/setup.md`.

## Channel speed upgrade

**SHIPPED & VERIFIED LIVE 2026-05-28.** Замена CH340 → **FT232H** на обеих сторонах null-modem'а подняла стабильный baud `115200 → 3_000_000` (×26), clipboard 1MB ~90 сек → ~3 сек. Никаких изменений в коде — только `baud = 3000000` в обоих `config.toml`. Hardware: два CJMCU-FT232H breakout (genuine FTDI, VID 0x0403 PID 0x6014), null-modem `AD0(TX) ↔ AD1(RX)` cross + GND, VCC изолированы. Windows требует **FTDI CDM driver** (https://ftdichip.com/drivers/vcp-drivers/) — без него COM-port не появляется в Ports (COM & LPT); macOS VCP встроен. Полный разбор + закрытые тупики (TCP/UDP режутся WFP-фильтрами Континента; Thunderbolt без TB-header'а на B760M не работает) + lessons learned — в `docs/briefs/ft232h-upgrade.md`. Plan B (Pi Zero 2W WinUSB bridge) остаётся как резерв на будущее **видео** по тому же каналу (USB 2.0 bulk ~30-40 MB/s).

**Деградация платы FT232H (эпизод 2026-06-16).** Если канал «отпадывает» постоянными штормами — частая первопричина не софт, а **деградировавшая плата** (TX-тракт одной из двух CJMCU-FT232H). Диагностика по сигнатуре в `client.log`: `COBS`/`CRC`/`bad magic` = порча битов (сигнал/baud на грани, лечится понижением baud); `Broken pipe`+`No such file` = USB-отвал (питание/контакт; `No such file` для `cu.usbserial-NNN` локализует именно Mac-сторону); `host link lost` при идущих `clipboard.send DONE` = асимметрия, бьётся TX одной стороны/жила провода/GND. Решающий тест «плата vs провод/окружение» — **swap двух плат местами**: если глюк переехал на другую сторону, виновата плата (в эпизоде swap вылечил канал даже на 3 Mbaud). Триаж лога — `/pg.wd-log` (личный slash-command).

## Plan

`docs/plans/wiredesk-mvp.md` — full MVP plan with protocol spec, etapes, and risk analysis.

`docs/briefs/ft232h-upgrade.md` — бриф апгрейда канала (**SHIPPED 2026-05-28** @ 3 Mbaud verified live; см. шапку файла).

`docs/briefs/daemon-multiplex.md` — roadmap-бриф: extract `wiredesk-daemon` чтобы GUI и `wd --exec` могли работать одновременно через один serial-порт (~2-3 нед).

`docs/briefs/gui-shell-pty-emulator.md` — roadmap-бриф: vt100 crate + custom egui TerminalView для real-PTY shell-panel (~1-2 нед).
