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

- [ ] добавить `dirs = "5"` в Cargo.toml host'а и клиента
- [ ] создать `apps/wiredesk-host/src/config.rs`: `HostConfig { port, baud, width, height, host_name, run_on_startup }` с derive(Serialize, Deserialize, Clone, Default).
  - `Default` соответствует текущим хардкодам: `COM3`, 115200, 2560×1440, "wiredesk-host", false
  - `pub fn config_path() -> PathBuf` — `dirs::config_dir().unwrap().join("WireDesk").join("config.toml")`
  - `pub fn load() -> Self` — если файла нет ИЛИ ошибка парсинга → `Default::default()` + log warning. Не возвращать Result.
  - `pub fn save(&self) -> std::io::Result<()>` — `create_dir_all(parent)`, `fs::write(path, toml::to_string_pretty(self)?)`
- [ ] создать `apps/wiredesk-client/src/config.rs`: `ClientConfig { port, baud, width, height, client_name }` с теми же методами. Defaults: `/dev/cu.usbserial-120`, 115200, 2560×1440, "wiredesk-client".
- [ ] write tests (host): TOML roundtrip
- [ ] write tests (host): `load()` несуществующего файла возвращает `Default`
- [ ] write tests (host): `save()` создаёт parent dir (через tempfile + override path)
- [ ] write tests (host): `Default` значения = хардкоды
- [ ] write tests (client): аналогично
- [ ] cargo test --workspace — must pass before next task

### Task 2: Mac client settings panel + TOML loading

**Files:**
- Modify: `apps/wiredesk-client/src/main.rs` (load TOML, merge с CLI args)
- Modify: `apps/wiredesk-client/src/app.rs` (settings panel в chrome UI)

- [ ] в `main.rs` перед `Args::parse()` загрузить `ClientConfig::load()`. Использовать как `default_value` для clap polishing — возможно через прямой construct `Args` если defaults dynamic (или сразу применить TOML после parse'а если CLI не override)
- [ ] подход: после `Args::parse()` перезаписать поля только если они равны хардкод-дефолтам, и заменить TOML-значениями. Минимально инвазивно.
- [ ] в `WireDeskApp` добавить поля `pending_config: ClientConfig`, `config_dirty: bool`, `save_toast: Option<(String, Instant)>`
- [ ] в `update()` (chrome ветка, после shell-collapsing): добавить `ui.collapsing("Settings", |ui| { ... })` с полями:
  - Port: combo-box заполняется через `serialport::available_ports()` фильтрацией на `/dev/cu.usbserial-`/`/dev/cu.wch-`
  - Baud: TextEdit numeric (parse to u32)
  - Width / Height: два TextEdit numeric inline
  - Client name: TextEdit string
- [ ] кнопка "Save" — если `config_dirty=true`: вызвать `pending_config.save()`, выставить `save_toast` на 3 секунды
- [ ] показывать toast (label) если `save_toast.is_some()` и `now < toast_time + 3s`
- [ ] в info-only screen (capture/fullscreen) — НЕ показывать settings panel (уже есть `should_show_chrome()` гард)
- [ ] write tests: load + merge with CLI args; CLI args override TOML
- [ ] cargo test --workspace — must pass before next task

### Task 3: Mac .app bundle build script + iconography

**Files:**
- Create: `scripts/build-mac-app.sh`
- Create: `assets/icon-source.png` (512×512 W letter — могу сгенерить через `convert` или сразу взять простую заглушку)
- Create: `apps/wiredesk-client/Info.plist` (template)

- [ ] сгенерировать `assets/icon-source.png` 1024×1024 — простая буква "W" на градиентном фоне через ImageMagick (`convert -size 1024x1024 ...`) или текстовый placeholder
- [ ] создать `Info.plist` template со значениями: `CFBundleIdentifier=dev.kar43lov.wiredesk`, `CFBundleName=WireDesk`, `CFBundleExecutable=wiredesk-client`, `CFBundleIconFile=AppIcon.icns`, `LSMinimumSystemVersion=11.0`, `LSUIElement=false` (показывать в доке)
- [ ] написать `scripts/build-mac-app.sh`:
  - cargo build --release -p wiredesk-client
  - mkdir -p target/release/WireDesk.app/Contents/{MacOS,Resources}
  - копировать binary в Contents/MacOS/
  - копировать Info.plist в Contents/
  - сгенерировать AppIcon.icns через iconutil из icon-source.png (создать iconset с разными размерами)
  - вывести путь до WireDesk.app
- [ ] make script executable: `chmod +x scripts/build-mac-app.sh`
- [ ] write tests: для скрипта — нет (это shell-скрипт, проверка manual)
- [ ] manual: запустить скрипт, убедиться что WireDesk.app создаётся и открывается двойным кликом
- [ ] cargo test --workspace — без изменений с прошлой задачи, must pass

### Task 4: Windows host — file logging через tracing + log bridge + panic hook

**Files:**
- Modify: `apps/wiredesk-host/Cargo.toml` (deps: tracing, tracing-subscriber, tracing-appender, tracing-log)
- Modify: `apps/wiredesk-host/src/main.rs` (init_logging)

ВАЖНО: эта задача идёт **до** Task 5 (которая отключает консоль через `windows_subsystem = "windows"`). Иначе паники между задачами невидимы.

- [ ] добавить deps: `tracing = "0.1"`, `tracing-subscriber = "0.3"`, `tracing-appender = "0.2"`, `tracing-log = "0.2"` в host Cargo.toml
- [ ] функция `init_logging() -> Result<WorkerGuard, std::io::Error>`:
  - получить config dir через `dirs::config_dir()` → `%APPDATA%\WireDesk\`
  - `std::fs::create_dir_all(&log_dir)?` — рекурсивно создаёт каталог если его нет
  - `tracing_appender::rolling::daily(&log_dir, "host.log")` — по дню, keep-7 неявно (нужно `Builder` с `max_log_files(7)` если такой API есть, иначе ручная очистка)
  - `tracing_subscriber::fmt().with_writer(non_blocking).init()`
  - `tracing_log::LogTracer::init()` — **обязательно**: переадресует все вызовы из `log::*` фасада (используются в `session.rs`, `clipboard.rs`, `injector.rs`, `shell.rs`) в tracing. Без этого половина логов молча теряется после Task 5.
  - `std::panic::set_hook(Box::new(|info| tracing::error!("PANIC: {info}")))` — паники тоже идут в файл
  - вернуть `WorkerGuard` (чтобы main держал его до конца)
- [ ] оставить вызовы `log::info!` / `log::error!` в существующем коде нетронутыми (LogTracer их подхватит) — изменения только в host/main.rs
- [ ] write tests: init_logging с tempdir родительский каталог не существует — должен создаться, файл должен открыться, `tracing::info!` должен записать строку
- [ ] write tests: panic hook пишет в лог (через `std::panic::catch_unwind` + проверка содержимого файла)
- [ ] cargo test -p wiredesk-host — must pass before next task

### Task 5: Windows host — рефактор session в отдельный поток + hide console

**Files:**
- Modify: `apps/wiredesk-host/src/main.rs`
- Create: `apps/wiredesk-host/src/session_thread.rs`

- [ ] добавить `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` в начало `main.rs`. На debug-сборке остаётся console для разработки; на release консоль скрыта.
- [ ] вынести существующий Session-loop в `session_thread.rs::run(config: HostConfig, status_tx: mpsc::Sender<SessionStatus>) -> JoinHandle`
- [ ] определить enum `SessionStatus { Disconnected, Waiting, Connected { client_name: String } }` для UI
- [ ] в `main.rs::main()`: вызвать `init_logging()` (из Task 4) **первой строкой**, загрузить `HostConfig::load()`, спавнить session_thread, оставить main для placeholder-loop с `tracing::info!` — это placeholder, в Task 6+ заменим на nwg
- [ ] загружать TOML, мержить с CLI args через `clap::ArgMatches::value_source(name)` для определения override (избегать sentinel-сравнений с дефолтами — корректно по семантике).
- [ ] write tests: SessionStatus enum (создание, Display)
- [ ] write tests: config merge — TOML значения применяются если CLI не передан, иначе CLI override (через `ArgMatches::value_source`)
- [ ] cargo test -p wiredesk-host — must pass before next task
- [ ] cargo build --release -p wiredesk-host — должно собраться (на macOS — с MockInjector, без nwg)

### Task 6: Windows host — nwg settings window + pure helpers

Дизайн: вся проверяемая логика (валидация полей, форматирование mac-команды, маппинг status→цвет) живёт в чистых модулях вне nwg-handlers. nwg-handlers — тонкие обёртки.

**Files:**
- Modify: `apps/wiredesk-host/Cargo.toml` (deps: native-windows-gui, native-windows-derive)
- Create: `apps/wiredesk-host/src/ui/mod.rs`
- Create: `apps/wiredesk-host/src/ui/format.rs` (pure helpers, тестируется юнитами)
- Create: `apps/wiredesk-host/src/ui/settings_window.rs` (под `cfg(windows)`)

- [ ] добавить deps: `native-windows-gui = "1"`, `native-windows-derive = "1"` в `[target.'cfg(windows)'.dependencies]`
- [ ] `ui/format.rs` (cross-platform pure logic):
  - `pub fn format_mac_launch_command(config: &HostConfig) -> String` — `./target/release/wiredesk-client --port /dev/cu.usbserial-120 --baud 115200` (port substitution через mapping table COM-X → cu.usbserial-X — пока fallback дефолт)
  - `pub fn validate_baud(s: &str) -> Result<u32, String>` — parse + range check (>=9600)
  - `pub fn validate_port(s: &str) -> Result<&str, String>` — non-empty
  - `pub fn validate_dimension(s: &str) -> Result<u16, String>` — parse u16 + sanity (>=320)
  - `pub fn status_color(status: &SessionStatus) -> StatusColor` — enum {Green, Yellow, Gray}
- [ ] `ui/settings_window.rs` (`#[cfg(windows)]`): `SettingsWindow` struct через `#[derive(Default, NwgUi)]`. Простой vbox без избыточных GroupBox'ов:
  - Window 420×340, hidden by default
  - Label «Status: …» обновляется из status-bridge
  - 4 поля: port, baud, width, height
  - CheckBox "Run on startup" 
  - Buttons: "Copy Mac launch command" (использует `format::format_mac_launch_command`), "Save", "Hide"
- [ ] обработчик "Save": использует `validate_*` функции, при успехе пишет `HostConfig::save()`, апдейтит autostart (Task 7), показывает inline label «Saved. Restart to apply»
- [ ] обработчик "Copy Mac launch command": кладёт в clipboard через `nwg::Clipboard::set_data_text()`
- [ ] обработчик "Hide" / окно close-button: `set_visible(false)`, не quit
- [ ] на не-Windows целях `ui/settings_window.rs` исключён через cfg, `format.rs` остаётся (cross-platform)
- [ ] write tests (format.rs): `format_mac_launch_command` — табличные кейсы для разных config'ов
- [ ] write tests (format.rs): `validate_baud` — успех на 9600/115200/921600, ошибка на 100/abc/empty
- [ ] write tests (format.rs): `validate_port` — non-empty ok, empty fail
- [ ] write tests (format.rs): `validate_dimension` — 320+/u16-max ok, 0/abc/65536 fail
- [ ] write tests (format.rs): `status_color` — табличные кейсы по 3 SessionStatus вариантам
- [ ] **cross-compile check:** `cargo build -p wiredesk-host` на macOS успешен (nwg deps под `cfg(windows)`, не тянутся)
- [ ] cargo test -p wiredesk-host — must pass before next task

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

- [ ] добавить deps: `auto-launch = "0.6"`, `single-instance = "0.3"` в `[target.'cfg(windows)'.dependencies]`
- [ ] закоммитить три PNG-иконки 16×16 (W зелёный/жёлтый/серый) в `assets/`. Можно сгенерить через ImageMagick локально и положить готовые в репо.
- [ ] `ui/tray.rs`: `TrayUi` struct через `#[derive(NwgUi)]` с `nwg::TrayNotification`. Три иконки embedded через `include_bytes!`. Меню: Show Settings / Open Logs / Quit. Поле `log_dir: PathBuf` хранит путь к директории логов (резолвится из `dirs::config_dir()` в main и передаётся в `TrayUi::new(log_dir)`).
- [ ] `ui/tray.rs`: метод `update_status(status: SessionStatus)` использует `format::status_color(status)` и через `tray.set_icon(&self.icon_for_color(c))` меняет иконку, а также `set_tooltip()` обновляет текст
- [ ] `ui/tray.rs`: handler "Show Settings" — emits через nwg notice который settings_window ловит и показывает
- [ ] `ui/tray.rs`: handler "Open Logs" — `Command::new("explorer").arg(&self.log_dir).spawn()`
- [ ] `ui/autostart.rs`: тонкая обёртка над `auto-launch`. `pub fn enable() -> Result<()>`, `pub fn disable()`, `pub fn is_enabled() -> bool`. Имя приложения = "WireDesk Host". Путь = `std::env::current_exe()`.
- [ ] `ui/status_bridge.rs`: `pub fn spawn(status_rx: mpsc::Receiver<SessionStatus>, last: Arc<Mutex<SessionStatus>>, notice: nwg::NoticeSender) -> JoinHandle`. Простой loop с `recv()`, lock, store, `notice.notice()`.
- [ ] в `main.rs`: после `init_logging()` и `HostConfig::load()`:
  - `single_instance::SingleInstance::new("wiredesk-host-singleton-mutex")?` — если `is_single() == false`: `nwg::simple_message("WireDesk", "Already running — check tray icon")`, `std::process::exit(0)`. **Не пытаемся** raise existing window (отложено как follow-up, требует named pipe IPC).
  - спавнить session_thread с `status_tx`
  - создать `Arc<Mutex<SessionStatus>>`, спавнить status_bridge
  - инициализировать TrayUi, SettingsWindow (с notice handles)
  - `nwg::dispatch_thread_events()`
- [ ] write tests: `autostart::enable` / `disable` / `is_enabled` — реальный registry change. **Помечены `#[ignore]`** — запускаются только локально на Windows-машине вручную, не на CI/Mac dev.
- [ ] cargo test -p wiredesk-host — must pass before next task

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

- [ ] CLAUDE.md: новая секция «Configuration» с TOML paths и merge-order
- [ ] CLAUDE.md: обновить Run-секцию — упомянуть .app bundle и tray-агента
- [ ] CLAUDE.md: новая секция «Tray on Windows» — UI, autostart, single-instance, logs
- [ ] README.md: обновить Run-секцию — двойной клик WireDesk.app на Mac, tray-launch на Windows
- [ ] docs/setup.md: добавить шаг про `./scripts/build-mac-app.sh` для Mac пользователей
- [ ] docs/setup.md: добавить шаг про autostart toggle на Windows
- [ ] move plan to `docs/plans/completed/20260501-launcher-ui.md`
- [ ] финальный коммит на ветке, PR в master, merge after live test

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
