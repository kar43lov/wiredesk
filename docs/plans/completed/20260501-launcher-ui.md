# Launcher UI: Tray on Windows, Dock app bundle on Mac

## Overview

Превратить обе стороны WireDesk из консольных утилит в системно-интегрированные приложения. Windows host получает безоконный launcher с tray-иконкой и settings-окном (nwg). Mac client получает полноценный `.app` bundle с иконкой в доке и settings-панель в существующем chrome UI. Обе стороны хранят настройки в TOML и загружают их при старте.

**Проблема:** сейчас оба бинаря — консольные. Host поднимается из PowerShell с открытым окном, его легко свернуть и забыть. Mac client запускается из терминала через `./target/release/wiredesk-client` — нет dock-иконки для Spotlight/Launchpad.

**Решение:** «Save + manual restart» — settings-UI сохраняет TOML, toast напоминает перезапустить. Никакого live-reconnect supervisor'а (отложено как полировка).

**Где живёт работа:** ветка `feat/launcher-ui`. Master стабилен на `221a75f`. Мерж только после live-теста.

**Acceptance criteria (live-тест):**
1. Windows: первый запуск `wiredesk-host.exe` → нет консольного окна, иконка W в трее
2. Tray-меню работает: правый клик → Show Settings / Open Logs / Quit
3. Settings: меняем port, нажимаем Save → toast, перезапуск → host работает с новыми настройками
4. Чекбокс «Run on startup»: вкл → перезагрузка Windows → host автоматически в трее
5. Кнопка «Copy Mac command»: вставка в Mac terminal → клиент запускается успешно
6. Mac: `./scripts/build-mac-app.sh` → `target/release/WireDesk.app`. Кликаем → окно открывается, в доке буква W
7. Mac chrome-UI: блок Settings с актуальными значениями. Меняем port → Save → toast → перезапуск → новые настройки применены
8. Capture/fullscreen UI без изменений (settings panel не показан)
9. `cargo test --workspace` — все 106 + новые тесты проходят
10. Single-instance: второй запуск host → message box «Already running, check tray» → второй процесс выходит

## Context (from discovery)

- **Workspace:** `serde + toml` уже в deps (используются как trait-импорты в `host/main.rs`, но без runtime config). Точка входа для persistence.
- **Host:** `apps/wiredesk-host/src/main.rs` — console binary, blocking `Session::tick()` в main thread.
- **Client:** `apps/wiredesk-client/src/{main.rs,app.rs}` — eframe-app с 5 потоками, существующий chrome UI расширяется settings-панелью.
- **Бриф:** `docs/briefs/launcher-ui.md` — полная спецификация и trade-offs.
- **Стек уже в проекте:** eframe 0.31, serialport 4, arboard 3 (host+client), `cfg(target_os = "macos")` блок для CGEventTap deps.

## Development Approach

- **Тестирование:** Regular (код сначала, тесты в той же задаче)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
- **CRITICAL: all tests must pass before starting next task**
- **CRITICAL: update this plan file when scope changes**
- run tests after each change
- maintain backward compatibility (мышь, клава, clipboard, shell, capture, fullscreen — без регрессий)

## Testing Strategy

- **unit tests:** для config TOML serialize/deserialize, settings-merge с CLI args, helpers (autostart enable/disable если возможно замокать)
- **integration tests:** не пишем — UI nwg и .app bundle проверяются вручную
- **manual live test:** AC1-AC10 руками после реализации

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope

## Solution Overview

```
┌── apps/wiredesk-host (Windows binary) ──────┐
│                                              │
│  main.rs:  windows_subsystem = "windows"     │
│            single_instance lock              │
│            init_logging() → %APPDATA%/log    │
│            load HostConfig from TOML         │
│            spawn session_thread (blocking)   │
│            run nwg event loop                │
│                                              │
│  ┌─ session thread ──────────────────────┐  │
│  │  существующий Session::tick() loop    │  │
│  │  Serial + Injector + Shell + Clip     │  │
│  │  status_tx → tray + settings UI       │  │
│  └────────────────────────────────────────┘  │
│                                              │
│  ┌─ nwg main thread ────────────────────┐   │
│  │  TrayIcon (W letter, status color)   │   │
│  │  SettingsWindow (hidden by default)  │   │
│  │  read status_rx, update UI           │   │
│  │  Save → TOML + autostart toggle      │   │
│  └───────────────────────────────────────┘   │
└──────────────────────────────────────────────┘

┌── apps/wiredesk-client (Mac .app bundle) ───┐
│                                              │
│  WireDesk.app/                              │
│    Contents/                                 │
│      MacOS/wiredesk-client (binary)         │
│      Resources/AppIcon.icns                 │
│      Info.plist                             │
│                                              │
│  WireDeskApp::update():                     │
│    + render_settings_panel() in chrome      │
│    Load ClientConfig from TOML at startup    │
│    Save → TOML + toast                      │
└──────────────────────────────────────────────┘
```

## Technical Details

**HostConfig** (`wiredesk-core/src/config.rs`):
```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct HostConfig {
    pub port: String,        // e.g., "COM3"
    pub baud: u32,           // 115200
    pub width: u16,          // 2560
    pub height: u16,          // 1440
    pub host_name: String,    // "wiredesk-host"
    pub run_on_startup: bool, // false default
}
```

**ClientConfig** (parallel):
```rust
#[derive(Serialize, Deserialize, Clone)]
pub struct ClientConfig {
    pub port: String,        // "/dev/cu.usbserial-120"
    pub baud: u32,           // 115200
    pub width: u16,          // 2560 (host screen size, fallback for first connect)
    pub height: u16,
    pub client_name: String,
}
```

**Config paths:**
- Windows: `%APPDATA%\WireDesk\config.toml` (получаем через `dirs::config_dir()` или `std::env::var("APPDATA")`)
- Mac: `~/Library/Application Support/WireDesk/config.toml`

**Merge order (lowest to highest precedence):**
1. Hardcoded defaults (текущие в `Args::default_value`)
2. TOML config file (если есть)
3. CLI args (override)

**nwg layout** (settings window):
```
┌─ WireDesk Host Settings ─────────────────────┐
│                                               │
│  Status: ● Connected to wiredesk-client      │
│                                               │
│  Serial port: [COM3       ▼]                 │
│  Baud rate:   [115200       ]                │
│  Screen size: [2560] x [1440]                │
│                                               │
│  ☑ Run on startup                            │
│                                               │
│  [ Copy Mac launch command ] [ Save ] [Hide] │
└───────────────────────────────────────────────┘
```

**Tray menu:**
```
WireDesk
─────────────
Show settings...
Open log folder
─────────────
Quit
```

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): Rust code, bash-скрипт сборки .app, тесты, документация
- **Post-Completion** (без чекбоксов): live-тест на железе, мерж в master

## Implementation Steps

### Task 1: Config types + TOML I/O — per-binary

`wiredesk-core` остаётся минимальным (error types + protocol-shared types). `dirs` крейт нужен только бинарям, поэтому config-модули живут в каждом из двух бинарей напрямую — это ~30 строк дублирования за поля и пути, что приемлемо (плата за чистый core-крейт).

**Files:**
- Create: `apps/wiredesk-host/src/config.rs`
- Create: `apps/wiredesk-client/src/config.rs`
- Modify: `apps/wiredesk-host/Cargo.toml` (deps: dirs)
- Modify: `apps/wiredesk-client/Cargo.toml` (deps: dirs)

- [x] добавить `dirs = "5"` в Cargo.toml host'а и клиента
- [x] создать `apps/wiredesk-host/src/config.rs`: `HostConfig { port, baud, width, height, host_name, run_on_startup }` с derive(Serialize, Deserialize, Clone, Default).
  - `Default` соответствует текущим хардкодам: `COM3`, 115200, 2560×1440, "wiredesk-host", false
  - `pub fn config_path() -> PathBuf` — `dirs::config_dir().unwrap().join("WireDesk").join("config.toml")`
  - `pub fn load() -> Self` — если файла нет ИЛИ ошибка парсинга → `Default::default()` + log warning. Не возвращать Result.
  - `pub fn save(&self) -> std::io::Result<()>` — `create_dir_all(parent)`, `fs::write(path, toml::to_string_pretty(self)?)`
  - дополнительно: `load_from(path)` / `save_to(path)` для тестов с tempdir
- [x] создать `apps/wiredesk-client/src/config.rs`: `ClientConfig { port, baud, width, height, client_name }` с теми же методами. Defaults: `/dev/cu.usbserial-120`, 115200, 2560×1440, "wiredesk-client".
- [x] write tests (host): TOML roundtrip
- [x] write tests (host): `load()` несуществующего файла возвращает `Default`
- [x] write tests (host): `save()` создаёт parent dir (через tempfile + override path)
- [x] write tests (host): `Default` значения = хардкоды
- [x] write tests (host): `load_from` мусорного файла возвращает Default
- [x] write tests (host): partial TOML — отсутствующие поля заполняются дефолтами (`#[serde(default)]`)
- [x] write tests (client): аналогично (6 тестов)
- [x] cargo test --workspace — 118 passed, 0 failed
- [x] ➕ unscoped fix: `crates/wiredesk-transport/src/mock.rs` (vec! → array) и `apps/wiredesk-host/src/session.rs` (unused import `InjectorEvent`) — clippy с обновлённым rustc уже падал на master до моих правок. Тривиальные правки чтобы `cargo clippy --workspace -- -D warnings` снова чист.
- [x] методы `config_path/load/save` помечены `#[allow(dead_code)]` до их wire-up в Task 2/5

### Task 2: Mac client settings panel + TOML loading

**Files:**
- Modify: `apps/wiredesk-client/src/main.rs` (load TOML, merge с CLI args)
- Modify: `apps/wiredesk-client/src/app.rs` (settings panel в chrome UI)

- [x] в `main.rs`: `ClientConfig::load()` → `Args::command().get_matches()` → `config::merge_args(&matches, toml_cfg)`. Вместо sentinel-сравнений используется `clap::parser::ValueSource` для чёткой семантики «CLI/Env override TOML».
- [x] `Args` сделан `pub` чтобы тесты в `config.rs` могли его использовать через `crate::Args` + `CommandFactory::command()`.
- [x] В `WireDeskApp`: новые поля `pending_config`, `config_dirty`, `save_toast`, `available_ports` + `runtime_serial_port` (immutable, что реально открыто).
- [x] `WireDeskApp::new` теперь принимает `ClientConfig` вместо отдельного `serial_port`.
- [x] `render_settings_panel(ui)` в chrome-ветке после shell-collapsing: collapsing("Settings") с полями port (combo + free-text), baud, width/height, client_name.
- [x] Combo обновляет `available_ports` on-click через `serialport::available_ports()` с фильтром `/dev/cu.`.
- [x] Кнопки Save (enabled только если dirty) и "Reset to defaults". Toast показывается 3 секунды через `save_toast: Option<(String, Instant)>`.
- [x] В info-only screen settings panel НЕ показывается (всё внутри `if !show_chrome { render_capture_info; return; }` гарда).
- [x] write tests (3): merge без CLI = TOML, --port override, --port + --baud + --name override
- [x] cargo test --workspace — 121 passed (56 client + 14 host + 47 protocol + 4 transport)
- [x] cargo clippy --workspace --all-targets -- -D warnings — clean
- [x] cargo build --release -p wiredesk-client — успешно

### Task 3: Mac .app bundle build script + iconography

**Files:**
- Create: `scripts/build-mac-app.sh`
- Create: `assets/icon-source.png` (512×512 W letter — могу сгенерить через `convert` или сразу взять простую заглушку)
- Create: `apps/wiredesk-client/Info.plist` (template)

- [x] сгенерировать `assets/icon-source.png` 1024×1024. ImageMagick на машине нет → написал `scripts/generate-icon.swift` (Swift+AppKit), генерит белую W на градиентном тёмно-синем rounded-square фоне. Результат закоммичен как `assets/icon-source.png` (116 KB).
- [x] создать `apps/wiredesk-client/Info.plist`: `CFBundleIdentifier=dev.kar43lov.wiredesk`, `CFBundleName=WireDesk`, `CFBundleExecutable=wiredesk-client`, `CFBundleIconFile=AppIcon`, `LSMinimumSystemVersion=11.0`, `LSUIElement=false`, `NSHighResolutionCapable=true`, `NSAppleEventsUsageDescription` (для будущих Accessibility-prompt'ов).
- [x] `scripts/build-mac-app.sh`:
  - cargo build --release -p wiredesk-client
  - rm -rf + mkdir -p target/release/WireDesk.app/Contents/{MacOS,Resources}
  - копирует binary, Info.plist
  - sips -z генерит 16/32/64/128/256/512/1024 + @2x варианты в tmp iconset
  - iconutil --convert icns → Contents/Resources/AppIcon.icns
  - chmod +x
- [x] оба скрипта `chmod +x`
- [x] manual run: `./scripts/build-mac-app.sh` успешно создаёт `target/release/WireDesk.app` со всеми тремя файлами (Info.plist, MacOS/wiredesk-client, Resources/AppIcon.icns 289 KB, тип "ic12")
- [x] cargo test --workspace — без регрессий (121 тест проходят)

### Task 4: Windows host — file logging через tracing + log bridge + panic hook

**Files:**
- Modify: `apps/wiredesk-host/Cargo.toml` (deps: tracing, tracing-subscriber, tracing-appender, tracing-log)
- Modify: `apps/wiredesk-host/src/main.rs` (init_logging)

ВАЖНО: эта задача идёт **до** Task 5 (которая отключает консоль через `windows_subsystem = "windows"`). Иначе паники между задачами невидимы.

- [x] добавить deps: `tracing = "0.1"`, `tracing-subscriber = "0.3"` (features: fmt, env-filter), `tracing-appender = "0.2"`, `tracing-log = "0.2"` в host Cargo.toml. `env_logger` удалён из host'а (полностью замещён tracing).
- [x] `apps/wiredesk-host/src/logging.rs`:
  - `pub fn log_dir() -> PathBuf` — `dirs::config_dir().join("WireDesk")` с fallback на `.`
  - `pub fn init_logging() -> io::Result<WorkerGuard>` (default path) и `init_logging_at(dir)` для тестов
  - `fs::create_dir_all(dir)?` — создаёт каталог рекурсивно если его нет
  - `tracing_appender::rolling::daily(dir, "host.log")` + `tracing_appender::non_blocking` для фоновой записи
  - `tracing_subscriber::fmt().with_writer(non_blocking).with_ansi(false).with_target(false).try_init()` — try_init безопаснее в multi-test сценарии
  - `tracing_log::LogTracer::init()` — bridge log → tracing (sessoin/clipboard/injector/shell используют log::*)
  - `install_panic_hook()` — `std::panic::set_hook` с `tracing::error!(target: "panic", ...)`
  - `pub fn format_panic(info: &PanicHookInfo) -> String` — pure formatter с location и payload (поддерживает &str, String, fallback)
  - `WorkerGuard` возвращён — `main()` держит его как `_log_guard`
- [x] log:: вызовы по всему коду не тронуты (LogTracer их подхватит). main.rs: env_logger::init заменён на init_logging() с fallback на eprintln warning.
- [x] write tests (4):
  - `format_panic_includes_location_and_message` — через catch_unwind + custom hook + PanicHookGuard для восстановления
  - `rolling_appender_writes_to_log_file` — пишет через NonBlocking writer в tempdir, проверяет что файл `test.log*` создаётся после flush+drop guard
  - `init_logging_at_creates_missing_dir` — `init_logging_at` создаёт parent рекурсивно
  - `log_dir_is_under_config_dir` — sanity check итоговый путь
  - `PanicHookGuard` (тестовый helper) сохраняет/восстанавливает panic hook чтобы тесты не лекали друг в друга
- [x] cargo test -p wiredesk-host — 18 passed (8 + 6 config + 4 logging)
- [x] cargo clippy --workspace --all-targets -- -D warnings — clean (была подсказка type_complexity на тестовый Box dyn — введён typedef `StoredHook`)

### Task 5: Windows host — рефактор session в отдельный поток + hide console

**Files:**
- Modify: `apps/wiredesk-host/src/main.rs`
- Create: `apps/wiredesk-host/src/session_thread.rs`

- [x] `#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]` в начале `main.rs` (с `windows` гардом — иначе атрибут на не-Windows цели вызывает unknown-attribute warning)
- [x] `apps/wiredesk-host/src/session_thread.rs`:
  - `pub enum SessionStatus { Disconnected(String), Waiting, Connected { client_name: String } }` + `is_connected()`, `label()`
  - `pub fn spawn(config: HostConfig, status_tx: mpsc::Sender<SessionStatus>) -> JoinHandle<()>` — два cfg-варианта (Windows + non-Windows), оба делегируют в generic `spawn_with_injector` с замыканием для построения InputInjector
  - Внутри потока: открывает SerialTransport, строит injector, создаёт Session, в loop вызывает tick + при изменении состояния шлёт SessionStatus наверх; `last_reported` дедупит чтобы не флудить канал
  - `pub fn derive_status(state: SessionState, client_name: Option<&str>)` — pure func, отделяемая от threading
- [x] `Session::current_state()` и `client_name()` (раньше state был `#[cfg(test)]`); поле `client_name: Option<String>` + reset на disconnect/timeout/rehandshake
- [x] `main.rs::main()`: init_logging → HostConfig::load → Args::command().get_matches → merge_args → session_thread::spawn → recv loop логирует SessionStatus changes (placeholder вместо nwg-цикла, заменяется в Task 6/7)
- [x] return type main() изменён на `()` — uncaught error в session_thread теперь не панацует main (раньше main возвращал Result<()> и `?` валил процесс при первой ошибке открытия порта; теперь session_thread шлёт Disconnected и завершается, main продолжает жить).
- [x] write tests (4 для SessionStatus + derive_status): label disconnected/waiting/connected, derive_status mapping
- [x] write tests (3 для merge_args): no CLI = TOML, --port override, --port + --baud + --name + --width + --height full override (run_on_startup сохраняется т.к. не выставляется через CLI)
- [x] cargo test -p wiredesk-host — 25 passed (8 + 6 config + 4 logging + 3 merge + 4 session_thread)
- [x] cargo build --release -p wiredesk-host — успешно (на macOS с MockInjector, без nwg — nwg добавляется в Task 6 под `cfg(windows)`)
- [x] cargo clippy --workspace --all-targets -- -D warnings — clean

### Task 6: Windows host — nwg settings window + pure helpers

Дизайн: вся проверяемая логика (валидация полей, форматирование mac-команды, маппинг status→цвет) живёт в чистых модулях вне nwg-handlers. nwg-handlers — тонкие обёртки.

**Files:**
- Modify: `apps/wiredesk-host/Cargo.toml` (deps: native-windows-gui, native-windows-derive)
- Create: `apps/wiredesk-host/src/ui/mod.rs`
- Create: `apps/wiredesk-host/src/ui/format.rs` (pure helpers, тестируется юнитами)
- Create: `apps/wiredesk-host/src/ui/settings_window.rs` (под `cfg(windows)`)

- [x] добавить deps: `native-windows-gui = "1"`, `native-windows-derive = "1"` в `[target.'cfg(windows)'.dependencies]`
- [x] `apps/wiredesk-host/src/ui/mod.rs` — оба подмодуля + `#[cfg_attr(not(windows), allow(dead_code))]` на `format` чтобы macOS-сборка не ругалась на pub helpers, которые на Windows используются из `settings_window`
- [x] `ui/format.rs` (cross-platform):
  - `format_mac_launch_command(&HostConfig) -> String` — выводит `./target/release/wiredesk-client --port /dev/cu.usbserial-120 --baud N`. COM→cu mapping не делаем (разные драйверы, кейсы) — Mac-порт оставляем дефолтным, baud берём из конфига
  - `validate_baud(&str) -> Result<u32, String>` — parse + min 9600
  - `validate_port(&str) -> Result<&str, String>` — trim + non-empty
  - `validate_dimension(&str) -> Result<u16, String>` — parse u16 (отбраковывает 65536+), min 320
  - `status_color(&SessionStatus) -> StatusColor` — Green/Yellow/Gray
- [x] `ui/settings_window.rs` (`#[cfg(windows)]`): `SettingsWindow` struct с владением nwg-controls. Используется builder API (`nwg::Window::builder()`, `nwg::Label::builder()`, etc) вместо derive macro — explicitly more легко audit'ится. Window 420×340, GridLayout 3 cols × 9 rows. Поля: status_label, port/baud/width/height (label + TextInput), autostart_check, copy_mac_btn, save_btn, hide_btn, message_label.
- [x] Методы:
  - `build(&HostConfig) -> Result<Rc<RefCell<Self>>, NwgError>` — собирает controls + layout, окно hidden by default
  - `show()` / `hide()` — set_visible
  - `read_form() -> Result<HostConfig, String>` — читает values + валидирует через format::*; возвращает первую ошибку для surfacing в message_label
  - `set_status(&SessionStatus)` / `set_message(&str)` — обновляют labels (вызывается из status_bridge handler в Task 7)
- [x] write tests (13 в `ui::format::tests`): status_color × 3 варианта, format_mac_command default+custom baud, validate_baud accepts standard/rejects too_low/rejects garbage, validate_port accepts nonempty/rejects empty, validate_dimension accepts realistic/rejects too_small/rejects overflow_and_garbage
- [x] **cross-compile check:** `cargo build -p wiredesk-host` на macOS — успешно. nwg не тянется (под `cfg(windows)`)
- [x] cargo test -p wiredesk-host — 38 passed (25 + 13 format)
- [x] cargo clippy --workspace --all-targets -- -D warnings — clean
- ➕ Note: nwg-handlers (Save/Copy/Hide buttons → callbacks) отложены на Task 7, где они будут привязываться через `nwg::full_bind_event_handler` к `Rc<RefCell<SettingsWindow>>`. Это в плане за рамками Task 6 — settings_window.rs владеет controls, event wiring — Task 7.

### Task 7: Windows host — tray icon, autostart, single-instance, status-bridge

Status-bridge дизайн (явно):
- session_thread шлёт `SessionStatus` через `mpsc::Sender<SessionStatus>` → bridge thread.
- bridge thread держит `Arc<Mutex<SessionStatus>>` (последнее значение) и `nwg::NoticeSender`.
- На каждый event bridge заменяет содержимое мьютекса и зовёт `notice.notice()` — это безопасно cross-thread.
- nwg main thread ловит notice через handler, читает `Mutex<SessionStatus>`, обновляет TrayUi (icon + tooltip) и SettingsWindow (status label).

**Files:**
- Modify: `apps/wiredesk-host/Cargo.toml` (deps: auto-launch, single-instance)
- Create: `apps/wiredesk-host/src/ui/tray.rs` (`#[cfg(windows)]`)
- Create: `apps/wiredesk-host/src/ui/autostart.rs`
- Create: `apps/wiredesk-host/src/ui/status_bridge.rs`
- Create: `assets/tray-green.png`, `assets/tray-yellow.png`, `assets/tray-gray.png` (16×16 W в трёх цветах)
- Modify: `apps/wiredesk-host/src/main.rs` (wire it all together)

- [x] ➕ deps скорректированы: вместо `auto-launch` и `single-instance` написал собственные wrapper'ы поверх `windows` crate (уже в deps). Преимущества: 0 новых зависимостей, явное API. Добавлены features: `Win32_System_Registry`, `Win32_System_Threading`, `Win32_Security`.
- [x] 3 PNG 16×16 (W зелёный/жёлтый/серый) сгенерированы через `scripts/generate-tray-icons.swift` и закоммичены в `assets/tray-{green,yellow,gray}.png`.
- [x] `ui/tray.rs` (`#[cfg(windows)]`): `TrayUi` через builder API (без NwgUi derive). Три PNG embedded через `include_bytes!`. Окно — `nwg::MessageWindow` (невидимое, нужно для message routing). Меню popup: Show Settings / Open Logs / Quit (separator перед Quit). Поле `log_dir: PathBuf` принимается в `TrayUi::build(log_dir)`.
- [x] `update_status(&SessionStatus)` использует `format::status_color`, перестраивает Icon из embedded bytes, вызывает `tray.set_icon(&icon)` и `tray.set_tip(...)`.
- [x] `show_popup()` для `OnContextMenu` event handler через `nwg::GlobalCursor::position()`.
- [x] `open_logs()` запускает `explorer.exe <log_dir>`.
- [x] `ui/autostart.rs`: 3 функции (`enable / disable / is_enabled`) над HKCU\Software\Microsoft\Windows\CurrentVersion\Run через `windows::Win32::System::Registry`. Имя value — "WireDesk Host", data — `std::env::current_exe()` в кавычках. Non-Windows стабы (no-op).
- [x] `ui/single_instance.rs`: `SingleInstanceGuard::acquire("WireDeskHostSingleton")` через `CreateMutexW` + проверку `GetLastError() == ERROR_ALREADY_EXISTS`. Variants `Acquired(guard)` / `AlreadyRunning` / `Error`. Drop closes handle. Non-Windows: всегда Acquired.
- [x] `ui/status_bridge.rs`: `spawn(rx, Arc<Mutex<SessionStatus>>, NoticeSender) -> JoinHandle` — loop с recv, lock+store, notice.notice(). Также `spawn_no_notice(rx, last)` для dev-loop на не-Windows.
- [x] `main.rs` cross-platform — split на `run_windows` (`#[cfg(windows)]`) и `run_dev_loop` (`#[cfg(not(windows))]`):
  - SingleInstanceGuard перед всем (фоллбек handler через into_guard_or_panic в случае Error)
  - `run_windows`: nwg::init, TrayUi::build, SettingsWindow::build, nwg::Notice → status_bridge::spawn → `nwg::full_bind_event_handler` для tray (OnNotice → update icon, OnContextMenu → show_popup, OnMenuItemSelected → Show/Open/Quit dispatch) и settings (OnButtonClick → Save/Copy/Hide handlers, OnWindowClose → hide)
  - Save handler: read_form → save TOML → toggle autostart → set_message; невалидное значение даёт inline ошибку
  - Copy Mac command handler: `format_mac_launch_command` → `nwg::Clipboard::set_data_text`
  - `nwg::dispatch_thread_events()` → main loop
- [x] cross-compile check `cargo check --target x86_64-pc-windows-gnu -p wiredesk-host` — успешно (т.е. nwg/tray/settings_window/autostart/single_instance синтаксически валидны для Windows)
- [x] Тесты: `acquire_returns_a_variant` (single_instance), `non_windows_stubs_dont_panic` (autostart), `windows_enable_then_disable_round_trip` (autostart, `#[ignore]` — runs только на Windows), `no_notice_bridge_stores_latest_status` + `no_notice_bridge_exits_on_sender_drop` (status_bridge)
- [x] cargo test -p wiredesk-host — 42 passed, 1 ignored (Windows-only registry round-trip)
- [x] cargo clippy --workspace --all-targets -- -D warnings — clean
- [x] ➕ Architectural fix: `init_logging` больше не устанавливает global panic hook (вынесено в `install_panic_hook()` который main вызывает явно). Это убрало race condition в параллельных тестах: install_panic_hook → tracing::error! → broken non_blocking writer (после drop WorkerGuard в конце предыдущего теста) → recursive panic → abort.
- [x] `Session::client_name` reset на disconnect / heartbeat timeout / rehandshake (иначе stale name попадал в `SessionStatus::Connected` через `derive_status`)

### Task 8: Live test на железе

- [ ] Windows: build release (`cargo build --release -p wiredesk-host`)
- [ ] AC1: запуск `wiredesk-host.exe` — нет консольного окна, иконка в трее
- [ ] AC2: правый клик трей → меню Show Settings / Open Logs / Quit
- [ ] AC3: settings: меняем port на COM4, Save, видим toast → quit, relaunch — работает с COM4
- [ ] AC4: вкл "Run on startup" → reboot → host автоматически в трее
- [ ] AC5: кнопка "Copy Mac command" → вставка в Mac terminal → клиент запускается
- [ ] Mac: `./scripts/build-mac-app.sh` → существует `target/release/WireDesk.app`
- [ ] AC6: двойной клик WireDesk.app — окно открывается, иконка W в доке
- [ ] AC7: в окне chrome UI — блок Settings со значениями, change port, Save, toast → relaunch — новые значения применены
- [ ] AC8: capture (Cmd+Esc) и fullscreen (Cmd+Enter) UI без settings panel — info-only
- [ ] AC9: cargo test --workspace — все 106+ тестов проходят
- [ ] AC10: вторая попытка `wiredesk-host.exe` — message box "Already running", второй процесс выходит
- [ ] live: clipboard sync, Cmd+Space, Cmd+C/V — без регрессий

### Task 9: Документация и финализация

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md`
- Modify: `docs/setup.md`

- [x] CLAUDE.md: новая секция «Configuration» с TOML paths и merge-order
- [x] CLAUDE.md: обновить Run-секцию — Host (Windows) tray agent, Client (macOS) `.app` bundle, прямой запуск бинарей для dev
- [x] CLAUDE.md: «Host module map» — детальная разбивка `apps/wiredesk-host/src/` (config, logging, session_thread, ui/* подмодули)
- [x] CLAUDE.md: дополнения в «Известные ограничения» — Save+Restart, mac autostart not implemented, code signing not done, single-instance focus
- [x] README.md: обновить Run-секцию — Host tray agent, Mac `.app` bundle, Configuration paths и merge order, прямой запуск как dev опция
- [x] README.md: статус — упоминание launcher UI features
- [x] docs/setup.md (Шаг 4): инструкция про `./scripts/build-mac-app.sh` + Gatekeeper warning + ссылка на `generate-icon.swift`
- [x] docs/setup.md (Шаг 6): tray UI на Windows, открытие logs/settings через tray, autostart toggle, ref на `%APPDATA%\WireDesk\config.toml`
- [x] move plan to `docs/plans/completed/20260501-launcher-ui.md`
- Финальный коммит на ветке, PR в master, merge after live test → пользовательский шаг (Task 8)

## Post-Completion

*Items requiring manual intervention:*

**Live-тест на железе:**
- AC1-AC10 проверяются ВРУЧНУЮ на реальной паре машин (Windows + Mac)
- Особое внимание: первый запуск .app на Mac — Gatekeeper warning, нужно правый-клик → Open
- Особое внимание: Windows autostart — после reboot убедиться что host реально стартует и в трее видна иконка
- Особое внимание: clipboard и keyboard hijack продолжают работать (не сломали)

**Мерж в master:**
- После успешного live-теста: `gh pr create` + `gh pr merge --merge --delete-branch`
- Master HEAD движется на merge commit, ветка удаляется

**Возможные follow-ups (вне scope):**
- Live-reconnect supervisor (вместо Save+restart)
- Code signing / нотарификация для распространения
- DMG installer для Mac
- Auto-update mechanism
- Mac autostart через Login Items / launchctl plist
- Custom-painted W tray icon (сейчас bundled PNG, можно потом нарисовать SVG-based)
