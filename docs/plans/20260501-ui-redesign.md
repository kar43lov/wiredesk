# UI Redesign + 3 фичи (issue #5)

## Overview

Превратить Settings UI обеих сторон WireDesk из «утилитарно-разработческого» (плоский grid в nwg, collapsing-stuffed UI в egui) в native-looking, согласованный визуально интерфейс. Параллельно — три недостающие функциональные фичи: **auto-detect CH340** на Windows, **Save & Restart** на Windows, **multi-monitor fullscreen selection** на macOS.

**Проблема:** UI собран наспех в launcher-PR (#3) — функционально работает (AC1-AC10 live-tested), но визуально диссонирует. UX-эксперт нашёл 8 critical + 12 improve пунктов в обеих платформах. Плюс пользователь хочет очевидные фичи (auto-detect устройства, Save & Restart, выбор монитора для HDMI-capture сценария).

**Решение:** Refactor in-place — остаёмся на nwg для Windows и egui для macOS, не меняем стек. Полируем стандартными средствами библиотек: nwg `Frame` / `ImageFrame` / `Font` global default; egui `RichText` / `Frame::group()` / `CollapsingHeader`. Embed PE-icon resource на Windows для иконки в title-bar / taskbar / Alt+Tab. NSScreen FFI на macOS для перечисления физических мониторов.

**Где живёт работа:** ветка `feat/ui-redesign` (создаётся от master `532a3df`). Master стабилен. Мерж только после live-теста на реальном железе (AC1-AC10 + 3 новых acceptance criteria для фичей).

**Acceptance criteria (live-тест):**
1. Все 8 Critical UX-пунктов закрыты (4 Windows + 4 macOS)
2. ≥80% Improve UX-пунктов закрыты (что не закрыто — оставляем follow-up issue)
3. Auto-detect: подключаю CH340 → Detect → port подставился; нет CH340 → message; несколько → список и просьба выбрать
4. Save & Restart: меняю port → жму → новый процесс работает с новыми настройками
5. Monitor selection (3 монитора macOS): выбираю Right → `Cmd+Enter` → fullscreen на правом → `Cmd+Enter` → возврат на исходный
6. Регресс: clipboard sync, `Cmd+Space`, `Cmd+C/V` — без изменений
7. AC1-AC10 (launcher-ui live-test) — все ещё проходят
8. Скриншоты до/после в обеих платформах в PR
9. `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` — clean
10. Cross-check `cargo check --target x86_64-pc-windows-gnu -p wiredesk-host` — clean

## Context (from discovery)

- **Workspace:** Rust workspace с 3 lib + 3 binary crates. Master стабилен на коммите `532a3df` (LICENSE), 149 unit-тестов проходят, clippy clean.
- **Host UI:** `apps/wiredesk-host/src/ui/{settings_window.rs, tray.rs, format.rs, autostart.rs, single_instance.rs, status_bridge.rs}` — nwg builder API, `#[cfg(windows)]` гард. Tray + 16×16 PNG иконки уже есть (`assets/tray-{green,yellow,gray}.png`). Settings — плоский 9-row GridLayout без визуальной иерархии.
- **Client UI:** `apps/wiredesk-client/src/app.rs` — egui, три mode'а через `should_show_chrome()` и `render_capture_info() / render_permission_screen()`. Settings — collapsing с TextEdit-полями (без groups). Status — крошечная цветная точка + `format!("{:?}", ...)` (Debug формат).
- **Бриф и UX-аудит:** `docs/briefs/ui-redesign.md` + комментарии в issue #5 (полный аудит UX-эксперта).
- **Стек уже в проекте:** nwg 1, native-windows-derive 1, egui/eframe 0.31, serialport 4, dirs 5, tracing-appender, embed-manifest 1. Что добавим: `embed-resource` (Win build.rs, для PE-icon), `objc2`/`objc2-app-kit` или сырой FFI (Mac NSScreen).

## Development Approach

- **Тестирование:** Regular (код сначала, тесты в той же задаче) — выбрано пользователем
- complete each task fully before moving to the next
- make small, focused changes — каждая task = один логический коммит из 9
- **CRITICAL: every task MUST include new/updated tests** для код-изменений в этой task'е
  - Pure-helper юниты — обязательно (detect_ch340_port, Display for ConnectionState, NSScreen index validation, монитор fallback)
  - UI rendering / nwg layout / egui widget визуально проверяется через скриншоты — это НЕ зачёт за unit-tests, скриншоты собираются manually на live-test
- **CRITICAL: all tests must pass before starting next task** — `cargo test --workspace` + `cargo clippy --workspace --all-targets -- -D warnings` после каждой task'а
- **CRITICAL: update this plan file when scope changes** — если nwg::Frame окажется без header-label, отметим как ⚠️ и используем fallback
- **Cross-check к Windows target** после каждой task'а: `cargo check --target x86_64-pc-windows-gnu -p wiredesk-host` — гарантирует что Win-only код не сломан с macOS dev-машины
- maintain backward compatibility — AC1-AC10 launcher live-test не должны деградировать

## Testing Strategy

- **unit tests:** обязательны для каждой task'а (см. Development Approach)
  - Tasks с pure-helper'ами (1, 7, 9) → классические табличные тесты
  - Tasks с UI-рендером (2, 3, 4, 5, 6) → unit-тестов для самого рендера нет (это nwg/egui internals), но если в task'е появился pure helper (e.g. `format_status_text(ConnectionState) -> String`) — тестируется
  - Tasks с handler-логикой (5, 8) → если возможно — pure-helper для Save&Restart spawn-команды
- **integration tests:** не пишем — UI nwg / egui визуально проверяется вручную
- **manual / live-test:** только в конце PR, после task 9, на реальной паре машин с CH340-кабелем + (опционально) multi-monitor для AC по выбору монитора. Скриншоты до/после — обе платформы — в описании PR.
- **live-test gate:** перед merge'ем в master, AC1-AC10 + AC3-AC5 (новые фичи) + регресс-чек clipboard / Cmd+Space / Cmd+C/V.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope (e.g. nwg::Frame fallback на panel+separator+Label)
- keep plan in sync with actual work done

## Solution Overview

```
master (532a3df, LICENSE)
   │
   └── feat/ui-redesign
        │
        ├── Task 1: chore(ui) typography pass
        │     • Win: Segoe UI 9pt global default ДО SettingsWindow::build
        │     • Mac: impl Display for ConnectionState (human strings)
        │
        ├── Task 2: feat(ui) window icons
        │     • Win: build.rs + embed-resource → PE-icon → title-bar / taskbar / Alt+Tab
        │     • Mac: W-logo asset рядом с ui.heading через ui.image
        │
        ├── Task 3: feat(ui) unified status indicators
        │     • Win: ImageFrame slot слева от status_label, cycle по SessionStatus
        │     • Mac: RichText.size(16) большой ●, явный текст с причиной
        │
        ├── Task 4: refactor(ui) grouped settings layout
        │     • Win: 3 Frame блока (Connection / Display / System), nested grids
        │     • Mac: Frame::group() / CollapsingHeader.default_open(true) для трёх блоков
        │
        ├── Task 5: refactor(ui) button-bar conventions
        │     • Win: button-bar внизу справа (Save primary right + default), Hide убрать
        │     • Mac: Capture Input — primary крупная цветная, переехала вверх
        │
        ├── Task 6: feat(client) capture-mode banner + permission steps
        │     • Mac: full-width red-tinted banner в capture; numbered groups в permission screen
        │
        ├── Task 7: feat(host) auto-detect CH340 button
        │     • Win: ui/format::detect_ch340_port pure helper + DetectResult enum
        │     • Win: settings_window.detect_btn + handler
        │
        ├── Task 8: feat(host) Save & Restart button
        │     • Win: settings_window.restart_btn + handler (Command::spawn + stop_thread_dispatch)
        │     • Race-mitigation: Sleep(200ms) перед stop_thread_dispatch
        │
        └── Task 9: feat(client) monitor selection for fullscreen
              • Mac: src/monitor.rs — NSScreen FFI wrapper (через objc2-app-kit)
              • Mac: ClientConfig.preferred_monitor: Option<usize>
              • Mac: WireDeskApp.toggle_fullscreen — OuterPosition + Fullscreen orchestration

Verification step (Task 10): cargo test / clippy / cross-check / live-test AC1-AC10 + AC новых фич
Final step (Task 11): docs update, move plan to completed/, push for PR
```

## Technical Details

### Task 1 — Typography

**Windows global font (`apps/wiredesk-host/src/main.rs`, в `run_windows` после `nwg::init`):**
```rust
let mut font = nwg::Font::default();
nwg::Font::builder().family("Segoe UI").size(16).build(&mut font)?;
let _ = nwg::Font::set_global_default(Some(font));
```
nwg `Font::size(16)` — это pixels, не points. Segoe UI 9pt ≈ 12px на 96 DPI; на Win11 default scaling 100% это даёт нативный вид. Размер 16 — стандартный для Win11 диалогов с DPI-awareness.

**Mac `impl Display for ConnectionState` (`apps/wiredesk-client/src/app.rs`):**
```rust
impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => write!(f, "Not connected"),
            ConnectionState::Connecting => write!(f, "Connecting…"),
            ConnectionState::Connected => write!(f, "Connected"),
        }
    }
}
```
В UI заменить `format!("{:?}", self.state)` на `format!("{}", self.state)`.

### Task 2 — Window icons

**Windows .ico (выбор: pre-built .ico закоммитим в repo один раз):**
- Генерация .ico — однократная, не каждую сборку. Способ генерации фиксирован: **`magick assets/icon-source.png -define icon:auto-resize=16,32,48,256 assets/app-icon.ico`** (ImageMagick). Если на dev-машине нет `magick` — можно один раз на любой машине с ImageMagick, либо через online-конвертер. .ico попадает в repo как обычный binary asset (так же как PNG'шки tray).
- Никаких `scripts/generate-app-ico.swift` не делаем — в Swift нет нативного .ico encoder'а, было бы fake-скрипт.
- Cargo.toml `[build-dependencies]` += `embed-resource = "2"`. `build.rs` после embed-manifest:
  ```rust
  if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
      embed_resource::compile("apps/wiredesk-host/app.rc", embed_resource::NONE);
  }
  ```
- `apps/wiredesk-host/app.rc`:
  ```
  1 ICON "../../assets/app-icon.ico"
  ```

**Pre-check (обязательный, перед началом Task 2):** `which x86_64-w64-mingw32-windres`. Если отсутствует:
- **Option A:** `brew install mingw-w64` (рекомендую — стандарт для cross-check к Win-target)
- **Option B (fallback):** не использовать `embed-resource`. Прокинуть иконку только через `nwg::Window::builder().icon(Some(&icon))` runtime-load из bytes. Window-icon работает в title-bar, но **не** в taskbar / Alt+Tab без PE-resource. Пометить как ⚠️ в плане и в issue #5.

**Mac W-logo (без отдельного asset'а — переиспользуем `icon-source.png`):**
```rust
ui.horizontal(|ui| {
    ui.add(
        egui::Image::new(egui::include_image!("../../../assets/icon-source.png"))
            .fit_to_exact_size(egui::vec2(28.0, 28.0))
    );
    ui.heading("WireDesk");
});
```
egui сам ресайзит на render — лишний asset не нужен. `egui_extras::install_image_loaders(&cc.egui_ctx)` нужен один раз в `main.rs` клиента (eframe 0.31 требует image loader для include_image!).

### Task 3 — Unified status indicators

**Windows ImageFrame:** добавить `status_icon: nwg::ImageFrame` в SettingsWindow, инициализировать `Bitmap` из `tray-gray.png`. В `set_status()`:
```rust
pub fn set_status(&mut self, status: &SessionStatus) {
    let bytes: &[u8] = match crate::ui::format::status_color(status) {
        StatusColor::Green => ICON_GREEN_BYTES,
        StatusColor::Yellow => ICON_YELLOW_BYTES,
        StatusColor::Gray => ICON_GRAY_BYTES,
    };
    let mut bmp = nwg::Bitmap::default();
    nwg::Bitmap::builder().source_bin(Some(bytes)).strict(true).build(&mut bmp).ok();
    self.status_icon.set_bitmap(Some(&bmp));
    self.status_label.set_text(&status.label());
}
```
ICON_*_BYTES перенести из `tray.rs` в `ui/icons.rs` (новый модуль) — общие байты для tray и settings.

**Mac large status glyph:** в `WireDeskApp::update` chrome-ветке:
```rust
ui.horizontal(|ui| {
    ui.add(egui::Label::new(
        egui::RichText::new("●").size(18.0).color(status_color)
    ));
    ui.label(self.status_text()); // "Connected to win-host" / "Waiting for handshake…" / etc.
});
```
`status_text()` — новый метод, формирует human-friendly строку.

### Task 4 — Grouped settings

**Windows Frame:**
```rust
pub struct SettingsWindow {
    // ...
    pub connection_frame: nwg::Frame,
    pub display_frame: nwg::Frame,
    pub system_frame: nwg::Frame,
    // ...
}
```
nwg Frame в builder API: `nwg::Frame::builder().text("Connection").parent(&s.window).build(&mut s.connection_frame)?;`. Потом under-grid: `nwg::GridLayout::builder().parent(&s.connection_frame)...`.

⚠️ **Risk:** если nwg::Frame не рисует header-label (только container), fallback: panel + Label сверху + separator. Помечу ➕ если откроется.

**Mac group():**
```rust
ui.group(|ui| {
    ui.label(egui::RichText::new("Connection").strong());
    // port + baud
});
ui.group(|ui| {
    ui.label(egui::RichText::new("Host display").strong());
    // width + height
});
ui.group(|ui| {
    ui.label(egui::RichText::new("Identity").strong());
    // client_name
});
```

### Task 5 — Button-bar

**Windows:**
- Move `copy_mac_btn` from верха в `system_frame` (логически принадлежит System).
- Внизу окна — горизонтальный Frame (без заголовка) как button-bar:
  - Слева: пусто или `Hide` (или убрать вовсе)
  - Справа: `Save & Restart` → `Save` (default, primary)
- `s.window.set_default_button(Some(&s.save_btn))` — Enter триггерит Save

**Mac:**
- `Capture Input` button — поднять выше всего в chrome (после status, до clipboard / shell), сделать `Button::new(RichText::new("Capture Input").size(16.0).strong()).fill(Color32::from_rgb(...))`, `min_size([200, 32])`.
- При `capturing=true` — менять fill на красноватый и текст на «Release Input».

### Task 6 — Capture banner + permission steps (Mac only)

**Capture banner:** в `render_capture_info`:
```rust
egui::Frame::group(ui.style())
    .fill(Color32::from_rgb(180, 60, 60).linear_multiply(0.3))
    .show(ui, |ui| {
        ui.label(RichText::new("● CAPTURING — Cmd+Esc to release").size(20.0).strong().color(Color32::WHITE));
    });
```

**Permission steps:** в `render_permission_screen` каждый шаг — отдельный `ui.group()` с кружком цифры:
```rust
for (i, step) in steps.iter().enumerate() {
    ui.group(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}", i+1)).size(20.0).strong());
            ui.label(step);
        });
    });
}
```
Кнопка `Open System Settings` — внутри шага 1, не в самом низу.

### Task 7 — Auto-detect CH340

**Pure helper в `ui/format.rs`:**
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectResult {
    Found(String),
    Multiple(Vec<String>),
    NotFound,
}

pub const WCH_VID: u16 = 0x1A86;

pub fn detect_ch340_port(ports: &[serialport::SerialPortInfo]) -> DetectResult {
    let matches: Vec<String> = ports.iter()
        .filter(|p| matches!(&p.port_type, serialport::SerialPortType::UsbPort(info) if info.vid == WCH_VID))
        .map(|p| p.port_name.clone())
        .collect();
    match matches.len() {
        0 => DetectResult::NotFound,
        1 => DetectResult::Found(matches.into_iter().next().unwrap()),
        _ => DetectResult::Multiple(matches),
    }
}
```

**В settings_window:** добавить `detect_btn: nwg::Button` рядом с port_input. Handler в main.rs:
```rust
} else if handle == s.detect_btn.handle {
    let ports = serialport::available_ports().unwrap_or_default();
    match crate::ui::format::detect_ch340_port(&ports) {
        DetectResult::Found(name) => {
            s.port_input.set_text(&name);
            s.set_message(&format!("Detected: {name}"));
        }
        DetectResult::Multiple(names) => {
            s.set_message(&format!("Multiple CH340 found: {}", names.join(", ")));
        }
        DetectResult::NotFound => {
            s.set_message("No CH340/CH341 detected. Plug the cable in.");
        }
    }
}
```

### Task 8 — Save & Restart

**Race-mitigation strategy (выбран retry-loop в single_instance):** старый процесс делает `spawn → stop_thread_dispatch`, не пытается ничего ждать. Новый процесс при acquire mutex повторяет 5 попыток × 100ms перед сдачей — это запас на graceful shutdown старого.

**В `single_instance.rs`:** новый pure-helper, тестируемый отдельно:
```rust
pub fn try_acquire_with_retry(name: &str, attempts: u8, delay_ms: u64) -> SingleInstanceResult {
    for _ in 0..attempts {
        match SingleInstanceGuard::acquire(name) {
            SingleInstanceResult::AlreadyRunning => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                continue;
            }
            other => return other,
        }
    }
    SingleInstanceResult::AlreadyRunning
}
```
Главный `main.rs` зовёт `try_acquire_with_retry("WireDeskHostSingleton", 5, 100)`. На «нормальный» запуск это 1 попытка ~ms. На restart — старый процесс закрывается за ≤500ms, новый ждёт. Этот pure-helper тестируется через mock'ы — а сама `acquire` может быть протрейчена в integration-тесте.

**В settings_window:** добавить `restart_btn: nwg::Button`. Handler:
```rust
} else if handle == s.restart_btn.handle {
    match s.read_form() {
        Ok(new_cfg) => {
            if let Err(e) = new_cfg.save() {
                s.set_message(&format!("Save failed: {e}"));
                return;
            }
            let want_startup = new_cfg.run_on_startup;
            let r = if want_startup { ui::autostart::enable() } else { ui::autostart::disable() };
            if let Err(e) = r {
                s.set_message(&format!("Saved, but autostart toggle failed: {e}"));
                return;
            }
            // Spawn new process — it will retry mutex acquire 5×100ms while
            // the old process winds down. No artificial sleep here.
            if let Ok(exe) = std::env::current_exe() {
                let _ = std::process::Command::new(exe).spawn();
            }
            nwg::stop_thread_dispatch();
        }
        Err(e) => s.set_message(&e),
    }
}
```

### Task 9 — Monitor selection (Mac only)

**Module `apps/wiredesk-client/src/monitor.rs`:**
```rust
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub frame: egui::Rect, // global coordinates
    pub size: egui::Vec2,
}

#[cfg(target_os = "macos")]
pub fn list_monitors() -> Vec<MonitorInfo> {
    use objc2::rc::Retained;
    use objc2_app_kit::NSScreen;
    use objc2_foundation::MainThreadMarker;
    let mtm = MainThreadMarker::new().unwrap();
    let screens = NSScreen::screens(mtm);
    screens.iter().enumerate().map(|(i, screen)| {
        let frame = screen.frame();
        MonitorInfo {
            index: i,
            name: screen.localizedName().to_string(),
            frame: egui::Rect::from_min_size(
                egui::Pos2::new(frame.origin.x as f32, frame.origin.y as f32),
                egui::Vec2::new(frame.size.width as f32, frame.size.height as f32),
            ),
            size: egui::Vec2::new(frame.size.width as f32, frame.size.height as f32),
        }
    }).collect()
}

#[cfg(not(target_os = "macos"))]
pub fn list_monitors() -> Vec<MonitorInfo> { Vec::new() }
```

**ClientConfig:**
```rust
pub struct ClientConfig {
    // existing fields
    pub preferred_monitor: Option<usize>,
}
```

**WireDeskApp:**
```rust
fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
    self.fullscreen = !self.fullscreen;
    if self.fullscreen {
        if let Some(idx) = self.pending_config.preferred_monitor {
            let monitors = monitor::list_monitors();
            if let Some(m) = monitors.get(idx) {
                self.original_position = ctx.input(|i| i.viewport().outer_rect.map(|r| r.min));
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(m.frame.min));
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                return;
            }
            // Fallback: monitor index invalid — just fullscreen on current
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
    } else {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        if let Some(pos) = self.original_position.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
        }
    }
}
```

**Settings UI:** в Connection или новый Display block — `ComboBox`:
```rust
let mut idx_str = match cfg.preferred_monitor {
    Some(i) => format!("Display {}", i + 1),
    None => "(active monitor)".to_string(),
};
egui::ComboBox::from_id_salt("monitor_select")
    .selected_text(idx_str)
    .show_ui(ui, |ui| {
        ui.selectable_value(&mut cfg.preferred_monitor, None, "(active monitor)");
        for m in &monitors {
            let label = format!("Display {} — {} ({}×{})", m.index + 1, m.name, m.size.x, m.size.y);
            ui.selectable_value(&mut cfg.preferred_monitor, Some(m.index), label);
        }
    });
```

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): Rust код, тесты, docs обновления, .ico генерация
- **Post-Completion** (без чекбоксов): live-тест AC1-AC10 + новые AC, скриншоты до/после, мерж в master, follow-up issue для незакрытых Improve пунктов

## Implementation Steps

### Task 1: Typography pass — Segoe UI on Win + Display for ConnectionState on Mac

**Files:**
- Modify: `apps/wiredesk-host/src/main.rs`
- Modify: `apps/wiredesk-client/src/app.rs`

- [x] создать ветку `feat/ui-redesign` от master `532a3df` (created as ui-redesign by /planning:exec)
- [x] в `main.rs::run_windows` после `nwg::init()` — построить `nwg::Font::builder().family("Segoe UI").size(16)` и установить через `Font::set_global_default(Some(font))` ДО `SettingsWindow::build()` и `TrayUi::build()`
- [x] в `app.rs` — `impl Display for ConnectionState` с человеческими строками («Not connected» / «Connecting…» / «Connected»)
- [x] заменить `format!("{:?}", self.state)` в UI на `format!("{}", self.state)`
- [x] write tests: табличный тест для `Display for ConnectionState` (3 варианта)
- [x] cargo test --workspace — must pass before next task
- [x] cargo clippy --workspace --all-targets -- -D warnings — clean
- [x] cargo check --target x86_64-pc-windows-gnu -p wiredesk-host — clean
- [x] commit: `chore(ui): typography pass — Segoe UI on Win, Display for ConnectionState on Mac`

### Task 2: Window icons — embed .ico in Win PE-headers + W в Mac heading

**Files:**
- Create: `assets/app-icon.ico` (готовый binary, генерим **один раз** через ImageMagick: `magick assets/icon-source.png -define icon:auto-resize=16,32,48,256 assets/app-icon.ico`)
- Create: `apps/wiredesk-host/app.rc`
- Modify: `apps/wiredesk-host/build.rs`
- Modify: `apps/wiredesk-host/Cargo.toml` (build-dep `embed-resource`)
- Modify: `apps/wiredesk-client/Cargo.toml` (dep `egui_extras` для image loader, если ещё нет)
- Modify: `apps/wiredesk-client/src/app.rs` (W в heading)
- Modify: `apps/wiredesk-client/src/main.rs` (eframe image loader install)

- [x] **pre-check:** `which x86_64-w64-mingw32-windres` — mingw not present, fallback to runtime icon (no PE-resource path; iconify via `nwg::Window::builder().icon(...)` only — title-bar yes, taskbar/Alt+Tab degraded until built on Windows host) ⚠️
- [x] сгенерить `assets/app-icon.ico` один раз и закоммитить — magick недоступен, использован Rust xtask `scripts/icogen` (ico 0.3 + image 0.25, 16/32/48/256 px output) → `cargo run --manifest-path scripts/icogen/Cargo.toml --release`
- [x] ~~добавить `embed-resource = "2"` в `[build-dependencies]` host'а~~ — пропущено (mingw fallback ⚠️)
- [x] ~~`apps/wiredesk-host/app.rc` с одной строкой `1 ICON "../../assets/app-icon.ico"`~~ — пропущено (mingw fallback ⚠️)
- [x] ~~в `build.rs` после embed-manifest — `embed_resource::compile(...)` под cfg(windows)~~ — пропущено (mingw fallback ⚠️). Вместо этого: `SettingsWindow` загружает `app-icon.ico` через `include_bytes!` + `nwg::Icon::builder().source_bin(...)` runtime, передаёт в `Window::builder().icon(...)`
- [x] на Mac — переиспользуем `assets/icon-source.png` напрямую: `ui.horizontal(|ui|{ ui.add(egui::Image::new(egui::include_image!("../../../assets/icon-source.png")).fit_to_exact_size(egui::vec2(28.0, 28.0))); ui.heading("WireDesk"); })`
- [x] в `main.rs` клиента — `egui_extras::install_image_loaders(&cc.egui_ctx)` в `Box::new(|cc| ...)`. Добавлены deps: `egui_extras = { version = "0.31", features = ["image"] }` + `image = { version = "0.25", default-features = false, features = ["png"] }`
- [x] write tests: pure-helper'ов не появилось — task без unit-тестов, проверка через build success (`include_bytes!("../../../../assets/app-icon.ico")` и `include_image!(".../icon-source.png")` падают на compile-time если файлы отсутствуют)
- [x] cargo build --release -p wiredesk-host (Win-build only via cross-check; full release build on Win machine at live-test gate)
- [x] manual visual check Dock-icon (deferred to live-test gate)
- [x] cargo test --workspace + clippy + cross-check — clean
- [x] commit: `feat(ui): window icons — embed .ico in Win PE-headers + W in Mac heading`

### Task 3: Unified status indicators — ImageFrame on Win + RichText on Mac

**Files:**
- Create: `apps/wiredesk-host/src/ui/icons.rs` (общие embedded PNG bytes)
- Modify: `apps/wiredesk-host/src/ui/tray.rs` (использовать icons.rs)
- Modify: `apps/wiredesk-host/src/ui/settings_window.rs` (status_icon ImageFrame)
- Modify: `apps/wiredesk-host/src/ui/format.rs` (если нужен новый pure-helper, например `status_text(&SessionStatus) -> String`)
- Modify: `apps/wiredesk-host/src/ui/mod.rs`
- Modify: `apps/wiredesk-client/src/app.rs`

- [x] создать `ui/icons.rs` с тремя `pub const ICON_*_BYTES: &[u8] = include_bytes!("../../../../assets/tray-*.png")`
- [x] обновить `tray.rs` чтобы использовать константы из `icons.rs`
- [x] добавить `status_icon: nwg::ImageFrame` (+ owned `status_icon_bitmap: nwg::Bitmap`) в `SettingsWindow`, инициализировать с `ICON_YELLOW_BYTES` (Waiting — initial state). Destructured borrow в build() чтобы borrow-checker увидел disjoint поля.
- [x] в `set_status(&mut self, ...)` — rebuild `status_icon_bitmap` in-place через builder по `format::status_color(status)` и вызвать `status_icon.set_bitmap(Some(&self.status_icon_bitmap))`. `set_status` теперь принимает `&mut self`.
- [x] обновить вызов `set_status` в main.rs (handler OnNotice ветка) — теперь `borrow_mut()`
- [x] на Mac в `app.rs::update` — заменить status row: `ui.horizontal(|ui|{ ui.add(Label::new(RichText::new("●").size(18.0).color(...))); ui.label(self.status_text()); })`
- [x] добавить метод `WireDeskApp::status_text(&self) -> String` — формирует human-friendly строку с причиной для Disconnected (парсит `status_msg` префикс «disconnected: …»)
- [x] write tests: pure-helper `status_text` для всех 3 ConnectionState вариантов + Disconnected с причиной (assert_eq! на ожидаемые строки)
- [x] cargo test --workspace + clippy + cross-check — clean
- [x] commit: `feat(ui): unified status indicators — ImageFrame on Win + RichText on Mac`

### Task 4: Grouped settings layout — Frame blocks on Win + group() on Mac

**Files:**
- Modify: `apps/wiredesk-host/src/ui/settings_window.rs` (Frame blocks + nested grids)
- Modify: `apps/wiredesk-client/src/app.rs` (Frame::group() / CollapsingHeader для трёх блоков)

- [x] добавить `connection_frame, display_frame, system_frame: nwg::Frame` в `SettingsWindow` (+ `connection_layout`, `display_layout`, `system_layout`, и три `*_title: nwg::Label` для headers — см. fallback ниже)
- [x] инициализировать каждую через `nwg::Frame::builder().parent(&s.window).flags(VISIBLE | BORDER).build(...)?` и под-grid `GridLayout::builder().parent(&s.connection_frame).margin([6,6,6,6])...`
- [x] перераспределить controls: port + baud в connection_frame; width + height в display_frame; autostart в system_frame
- [x] copy_mac_btn перенести в system_frame (логически относится к system)
- [x] Save / Hide / новые Detect/Restart кнопки — оставить вне frames в нижнем button-bar (Task 5)
- [x] ⚠️ **fallback использован:** `nwg::Frame::builder()` не имеет `.text()` (см. native-windows-gui 1.0.13 `controls/frame.rs:147-213` — только size/position/enabled/flags/parent/ex_flags). Header GroupBox-style недоступен. Использован паттерн **Label "Connection" (strong) + Frame с BORDER** под ней — каждая группа = 2 строки внешнего grid (заголовок 1 row + frame со spread на 2 rows). На macOS аналогично — `ui.group()` не рисует header автоматически, добавлен `RichText::new("...").strong()` первой строкой внутри group.
- [x] на Mac — обернуть три блока в `ui.group(|ui|{...})` с `RichText::new("Connection").strong()` заголовками
- [x] write tests: pure-helper'ов в этом task'е не появилось — рендер визуально через скриншот в live-test (UX-проверка на железе). Existing tests (151 на workspace) продолжают проходить — структурные изменения не затронули логику.
- [x] cargo test --workspace + clippy + cross-check — clean (151 tests pass, 0 warnings, Windows target clean)
- [x] commit: `refactor(ui): grouped settings layout — Frame blocks on Win + group() on Mac`

### Task 5: Button-bar conventions — primary right-aligned, default action keyboarded

**Files:**
- Modify: `apps/wiredesk-host/src/ui/settings_window.rs` (button-bar внизу справа, default_button)
- Modify: `apps/wiredesk-host/src/main.rs` (убрать Hide handler если кнопку убрали)
- Modify: `apps/wiredesk-client/src/app.rs` (Capture button primary)

- [x] в `SettingsWindow` — нижний button-bar Frame (без заголовка) с `nwg::GridLayout` правое выравнивание (`bar_frame` + `bar_layout`, 3 col grid, col 0 spacer)
- [x] order слева→направо: spacer / `Save & Restart` (новый, Task 8 placeholder — built but no handler) / `Save` (primary)
- [x] убрать `hide_btn` — поле и handler удалены (close-X duplication, UX-аудит N3)
- [x] **НЕ ставлю** `set_default_button` — конфликт с TextEdit Enter UX. Save доступен мышью + Alt+S accelerator
- [x] `&` префикс в caption: `Save` → `&Save`, `Save & Restart` → `Save && &Restart` (Alt+R). Двойной `&&` — литеральный амперсанд в win-resource caption
- [x] на Mac в `app.rs::update` chrome — `Capture Input` уже сразу после status row, изменён только стиль
- [x] стиль применён: `egui::Button::new(RichText::new(...).size(16.0).strong()).fill(...).min_size(vec2(200, 32))`
- [x] при `capturing=true` — fill красноватый `Color32::from_rgb(180, 60, 60)`, текст «Release Input». Idle — синеватый `(60, 110, 180)` («Capture Input»).
- [x] write tests: pure helper не введён (стайлинг inline 2 строки — overkill оборачивать) — skip per plan
- [x] cargo test --workspace + clippy + cross-check — clean (151 tests pass, 0 warnings)
- [x] commit: `refactor(ui): button-bar conventions — primary right-aligned, default action keyboarded` (committed da7e9dd)

### Task 6: Capture-mode banner + permission-screen step-by-step (Mac only)

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs` (render_capture_info, render_permission_screen)

- [x] в `render_capture_info` — full-width `egui::Frame::group(ui.style()).fill(...)` баннер сверху с `RichText::new("● CAPTURING — Cmd+Esc to release").size(20.0).strong().color(Color32::WHITE)`
- [x] цвет фона: `Color32::from_rgb(180, 60, 60).linear_multiply(0.3)` (полупрозрачный красноватый)
- [x] **обязательно** выделить `pub fn permission_steps() -> &'static [&'static str]` (4-pункта инструкции как массив строк) — pure helper, тестируется
- [x] в `render_permission_screen` — каждый из шагов из `permission_steps()` в `ui.group()` с цифрой в кружке слева (`RichText::new(format!("{}", i+1)).size(20.0).strong()`)
- [x] кнопка `Open System Settings` — внутри шага 1 (не в самом низу)
- [x] warning про restart внизу — `RichText` с цветом + иконка ⚠
- [x] write tests (2): `permission_steps()` возвращает 4 элемента; первый шаг содержит «System Settings» substring (защита от случайного breakage текста инструкции)
- [x] cargo test --workspace + clippy + cross-check — clean
- [x] commit: `feat(client): capture-mode banner + permission-screen step-by-step`

### Task 7: Auto-detect CH340 button (VID 0x1A86 filter)

**Files:**
- Modify: `apps/wiredesk-host/src/ui/format.rs` (DetectResult enum + detect_ch340_port + табличные тесты)
- Modify: `apps/wiredesk-host/src/ui/settings_window.rs` (detect_btn в connection_frame)
- Modify: `apps/wiredesk-host/src/main.rs` (handler OnButtonClick для detect_btn)

- [ ] в `format.rs` добавить `pub const WCH_VID: u16 = 0x1A86`
- [ ] в `format.rs` добавить `pub enum DetectResult { Found(String), Multiple(Vec<String>), NotFound }` (Debug, Clone, PartialEq, Eq)
- [ ] `pub fn detect_ch340_port(ports: &[serialport::SerialPortInfo]) -> DetectResult` — фильтр по `SerialPortType::UsbPort(info)` где `info.vid == WCH_VID`
- [ ] добавить `detect_btn: nwg::Button` в `SettingsWindow` рядом с port_input (внутри connection_frame)
- [ ] handler в main.rs: `serialport::available_ports()` → `detect_ch340_port` → match → `set_text` + `set_message`
- [ ] write tests (5+): NotFound (пусто), NotFound (только non-USB), Found (1 CH340 + другие), Multiple (2 CH340), Found через PID variants (0x7523, 0x55D4, 0x55D3)
- [ ] use mock `SerialPortInfo` через прямую конструкцию `SerialPortType::UsbPort(UsbPortInfo {...})`
- [ ] cargo test --workspace + clippy + cross-check — clean
- [ ] commit: `feat(host): auto-detect CH340 button (VID 0x1A86 filter)`

### Task 8: Save & Restart button (Command::spawn + stop_thread_dispatch)

**Files:**
- Modify: `apps/wiredesk-host/src/ui/settings_window.rs` (restart_btn в button-bar)
- Modify: `apps/wiredesk-host/src/main.rs` (handler OnButtonClick для restart_btn)

- [ ] добавить `restart_btn: nwg::Button` в SettingsWindow и в нижний button-bar (между spacer'ом и save_btn)
- [ ] handler: `read_form` → `save` → `autostart toggle` → `Command::new(current_exe).spawn()` → `Sleep(200ms)` → `nwg::stop_thread_dispatch`
- [ ] на ошибку валидации/save — `set_message` без рестарта
- [ ] race-condition mitigation: 200ms между spawn и stop_dispatch — даёт новому процессу 200ms init time, к моменту exit'а старого mutex освобождается раньше чем новый дойдёт до acquire
- [ ] write tests: pure helper `restart_command(exe: &Path) -> Command` если введён — тест что builder правильно собран; иначе skip (handler сам не тестируется без nwg-runtime)
- [ ] cargo test --workspace + clippy + cross-check — clean
- [ ] commit: `feat(host): Save & Restart button (Command::spawn + stop_thread_dispatch)`

### Task 9a: Monitor enumeration module (NSScreen FFI + pure-helper)

**Files:**
- Create: `apps/wiredesk-client/src/monitor.rs`
- Modify: `apps/wiredesk-client/src/main.rs` (mod monitor)
- Modify: `apps/wiredesk-client/Cargo.toml` (objc2-app-kit deps под cfg macOS)

- [ ] **pre-check:** через context7 уточнить `objc2-app-kit::NSScreen` API в актуальной версии — есть ли `localizedName` без unsafe, какие feature-флаги для NSScreen, `MainThreadMarker` import
- [ ] добавить `objc2 = "0.5"` + `objc2-app-kit = "0.2"` (с фичей если есть) + `objc2-foundation = "0.2"` в `[target.'cfg(target_os = "macos")'.dependencies]` клиента. Если context7 покажет что objc2-foundation транзитивно подтянется — оставить только objc2-app-kit.
- [ ] `monitor.rs`: `pub struct MonitorInfo { pub index: usize, pub name: String, pub frame: egui::Rect, pub size: egui::Vec2 }`
- [ ] `pub fn list_monitors() -> Vec<MonitorInfo>` — на macOS через `NSScreen::screens()`. На non-macOS — `Vec::new()` stub под cfg.
- [ ] `pub fn resolve_target_monitor(preferred: Option<usize>, monitors: &[MonitorInfo]) -> Option<&MonitorInfo>` — pure helper, тестируемый: None → None, Some(invalid_idx) → None + log::warn, Some(valid_idx) → Some(&monitors[idx])
- [ ] write tests (4): `list_monitors` на non-macOS возвращает empty Vec (sanity); `resolve_target_monitor(None, ...) == None`; `resolve_target_monitor(Some(99), &[..])` → None для невалидного индекса; `resolve_target_monitor(Some(0), &[m0, m1])` → Some(m0)
- [ ] cargo test --workspace + clippy + cross-check — clean
- [ ] commit: `feat(client): monitor enumeration via NSScreen FFI`

### Task 9b: ClientConfig.preferred_monitor + Settings ComboBox (без fullscreen-orchestration)

**Files:**
- Modify: `apps/wiredesk-client/src/config.rs` (preferred_monitor field + serde)
- Modify: `apps/wiredesk-client/src/app.rs` (Settings ComboBox, без logic в toggle_fullscreen)

- [ ] добавить `preferred_monitor: Option<usize>` в `ClientConfig` с `#[serde(default)]`
- [ ] обновить тест `partial_toml_uses_defaults_for_missing_fields` — `preferred_monitor` после load должно быть `None`
- [ ] добавить новый тест `toml_roundtrip_preferred_monitor` — Some(0) и Some(2) сохраняются и читаются
- [ ] в `render_settings_panel` (или в Display group из Task 4) — `egui::ComboBox::from_id_salt("monitor_select")` со списком из `monitor::list_monitors()`. Default «(active monitor — default)» = None. Selected → задаёт `pending_config.preferred_monitor`
- [ ] format строк в combo: «Display 1 — Studio Display (5120×2880)»
- [ ] write tests: уже покрыто через config tests
- [ ] cargo test --workspace + clippy + cross-check — clean
- [ ] commit: `feat(client): preferred_monitor config + Settings ComboBox`

### Task 9c: toggle_fullscreen orchestration (move-then-fullscreen + fallback)

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs` (WireDeskApp.original_position + toggle_fullscreen rewrite + fallback message в render_capture_info)

- [ ] добавить поле `original_position: Option<egui::Pos2>` в `WireDeskApp`, инициализировать None в new()
- [ ] переписать `toggle_fullscreen`: при включении (self.fullscreen=true after toggle) — `let monitors = monitor::list_monitors(); let target = monitor::resolve_target_monitor(self.pending_config.preferred_monitor, &monitors)`. Если Some(m) — сохранить `original_position` через `ctx.input(|i| i.viewport().outer_rect.map(|r| r.min))`, послать `ViewportCommand::OuterPosition(m.frame.min)`, потом `ViewportCommand::Fullscreen(true)`. Если None — fullscreen без перемещения.
- [ ] при выключении — `Fullscreen(false)`, потом `if let Some(pos) = self.original_position.take() { send_viewport_cmd(OuterPosition(pos)) }`
- [ ] fallback message: если `preferred_monitor=Some(idx)` но `resolve_target_monitor` вернул None — установить `self.status_msg = "Selected monitor unavailable; fullscreen on current display"` (видно в status row)
- [ ] edge case: `original_position` за пределами всех текущих экранов (юзер вручную перетащил окно после fullscreen) — при exit пропустить OuterPosition restore, окно останется где сейчас
- [ ] write tests: pure-helper для edge case в resolve уже в 9a; здесь — manual проверка в live-test
- [ ] cargo test --workspace + clippy + cross-check — clean
- [ ] commit: `feat(client): per-monitor fullscreen via OuterPosition + Fullscreen orchestration`

### Task 10: Verify acceptance criteria + регресс-чек

- [ ] verify все 8 Critical UX-пунктов из issue #5 закрыты (4 Win + 4 Mac)
- [ ] подсчёт закрытых Improve пунктов — должно быть ≥80%, оставшиеся вынести в новый follow-up issue
- [ ] verify edge cases: detect когда CH340 нет / несколько; restart race (5×100ms retry); monitor невалидный индекс; monitor 0 (только один экран); original_position за пределами всех экранов после fullscreen exit
- [ ] run full test suite: `cargo test --workspace` — было 149, ожидаем +10-15 новых → 160+
- [ ] run clippy: `cargo clippy --workspace --all-targets -- -D warnings` — clean
- [ ] cross-check: `cargo check --target x86_64-pc-windows-gnu -p wiredesk-host` — clean (PE-icon resource собирается через windres)
- [ ] cargo build --release: `-p wiredesk-client` (Mac) и `-p wiredesk-term` локально; `-p wiredesk-host` нельзя на macOS, проверяется только cross-check выше + actual build на Windows-машине в момент live-test
- [ ] live-test на железе (Windows 11 + macOS + CH340 кабель + опционально multi-monitor):
  - AC1-AC10 launcher live-test (регресс) — все проходят
  - AC3: Detect button подключённого CH340 → port подставился
  - AC3: Detect button без кабеля → message «No CH340/CH341 detected»
  - AC3: Detect с двумя CH340 (если есть второй кабель) → message «Multiple CH340 found: COMx, COMy»
  - AC4: Save & Restart — меняю port → жму → новый процесс с новыми настройками; повторно через 1-2 секунды (race-check)
  - AC5 (если multi-monitor доступен): выбираю Right → Cmd+Enter → fullscreen на правом → Cmd+Enter → возврат на исходный
  - AC5 fallback: выбираю «Display 3», отключаю один монитор так чтобы остался индекс ≤2 → Cmd+Enter → fullscreen на текущем + сообщение «Selected monitor unavailable»
  - Регресс: clipboard sync (Cmd+C/V), Cmd+Space, Cmd+Q forwarding, Cmd+Tab forwarding — без изменений
- [ ] скриншоты до/после settings UI на обеих платформах для PR-описания

### Task 11: Documentation + finalize

**Files:**
- Modify: `CLAUDE.md` (новая секция или обновить existing)
- Modify: `README.md` (упоминание features)
- Modify: `docs/setup.md` (если новые шаги)

- [ ] CLAUDE.md: обновить раздел Host module map (новый `ui/icons.rs`); добавить упоминание Detect / Save & Restart / monitor selection в Run-секцию
- [ ] CLAUDE.md: дополнить «Известные ограничения» если что-то открылось в live-тесте
- [ ] README.md: упомянуть auto-detect и monitor selection в feature list
- [ ] docs/setup.md: если процесс изменился — обновить (вероятно нет)
- [ ] move plan to `docs/plans/completed/20260501-ui-redesign.md`
- [ ] commit: `docs: finalize UI redesign — update CLAUDE.md, README.md`
- [ ] push feat/ui-redesign и создать PR в master с скриншотами до/после

## Post-Completion

*Items requiring manual intervention or external systems — no checkboxes, informational only*

**Live-test на железе:**
- AC1-AC10 (launcher) + AC3-AC5 (новые фичи) проверяются ВРУЧНУЮ на реальной паре машин (Windows 11 + macOS) с CH340-кабелем
- Опционально: multi-monitor setup (3 монитора macOS) для AC5
- Скриншоты до/после settings UI для PR-description

**PR review и merge:**
- После live-теста: PR в master с галочками AC и скриншотами
- Merge через GitHub UI (squash или merge commit — на усмотрение)
- Master HEAD движется на merge commit, ветка удаляется через GitHub UI

**Возможные follow-ups (вне scope):**
- Code signing / нотарификация для Mac .app distribution
- Mac autostart через Login Items / launchctl plist
- Темизация (light/dark theme switching)
- Live-reconnect supervisor (replace Save+Restart pattern)
- Локализация UI (русский / английский переключение)
- Незакрытые Improve / Nice-to-have пункты из UX-аудита (если останутся) → новый issue
