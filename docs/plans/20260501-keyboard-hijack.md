# Keyboard Hijack + Fullscreen + UI Cleanup в capture-mode

## Overview

Превратить capture-mode из «egui-окно ловит часть клавиатуры» в полноценный takeover уровня операционной системы — клавиатура целиком (включая системные шорткаты Cmd+Space, Cmd+C), окно в fullscreen по Cmd+Enter, и только текстовые подсказки в окне без кликабельных элементов.

**Проблема:** В текущем MVP `egui` видит только те клавиши, которые macOS не успел перехватить. Cmd+Space идёт в ChatGPT (mapping пользователя), Cmd+C/V может потеряться, переключение языка не работает. Кроме того, кликабельные кнопки в окне мешают, когда пользователь использует WireDesk как «третий монитор» через HDMI capture (легко ткнуть мимо в режиме «вслепую»).

**Решение:** Hybrid CGEventTap. Нативный macOS event tap создаётся при старте и **активируется только в capture-mode**. Когда capture выключен — macOS работает без вмешательства. Tap живёт в отдельном потоке с CFRunLoop, callback быстрый (только декод + mpsc::send).

**Решённые pre-implementation вопросы (после plan-review):**
- Fullscreen toggle = **Cmd+Enter** (не Cmd+Enter). Cmd+Enter в Windows = «Свойства», его пользователь захочет форвардить на Host. Cmd+Enter свободен на обеих ОС.
- В capture-mode egui-key-forward в `app.rs::update()` **отключается** (tap единственный источник, иначе double KeyDown).
- Tap-thread сохраняет `CFRunLoopRef` в `Arc<Mutex<Option<...>>>` для graceful shutdown через `CFRunLoopStop` из drop'а handle.
- Tap auto-disable (`kCGEventTapDisabledByTimeout`, `ByUserInput`) — handler в callback вызывает `CGEventTapEnable(tap, true)`.
- prev_flags для FlagsChanged-events — `Arc<AtomicU64>` в struct, который замыкается в callback closure.
- Cmd→Ctrl mapping: при FlagsChanged Cmd-press мы шлём **и** scancode 0x1D KeyDown (Ctrl-как-клавиша) **и** modifier bit на следующих буквенных KeyDown. Симметрично для Cmd-release.
- Permission re-check throttled: `Instant`-based, раз в 2с.
- Sticky modifiers cleanup: при `tap.disable()` шлём KeyUp для всех модификаторов, которые были нажаты, чтобы Host не остался с залипшим Ctrl.

**Где живёт работа:** ветка `feat/keyboard-hijack`. Master стабилен на `cd998d6`. Мерж только после live-теста.

## Context (from discovery)

- **Файлы клиента**: `apps/wiredesk-client/src/{main.rs, app.rs, clipboard.rs, input/{keymap.rs, mapper.rs, mod.rs}}`
- **Текущий keymap**: `egui::Key → Win scancode` уже есть (`egui_key_to_scancode`). Нужен симметричный для CGKeyCode.
- **Текущий threading**: writer + reader + clipboard poll. Добавится 4-й — keyboard tap. Все общаются через mpsc.
- **Бриф**: `docs/briefs/keyboard-hijack.md` (полный контекст и trade-offs)
- **Стек**: Rust workspace, eframe 0.31, core-graphics 0.25 (уже транзитивная зависимость от eframe)

## Development Approach

- **Тестирование**: Regular (код сначала, тесты в той же задаче)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
- **CRITICAL: all tests must pass before starting next task**
- **CRITICAL: update this plan file when scope changes**
- run tests after each change
- maintain backward compatibility (мышь, clipboard, shell — без регрессий)

## Testing Strategy

- **unit tests**: для `keymap` маппинга, state machine TapHandle, для UI state transitions (capture/fullscreen/permission)
- **integration tests**: не пишем — CGEventTap нужен живой macOS framework, моки не дают ценности. Проверка через live-тест на железе.
- **e2e tests**: проект не имеет UI e2e (egui без headless render), поэтому AC1-AC7 проверяются вручную после реализации.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope

## Solution Overview

```
┌──────────────── wiredesk-client (macOS) ────────────────┐
│                                                          │
│   ┌─ egui UI thread ──────────────────────────────┐     │
│   │  WireDeskApp (capturing, fullscreen, perm)    │     │
│   │  - reads tap_events_rx, applies state         │     │
│   │  - reads input events, forwards via outgoing  │     │
│   └────────────────────────────────────────────────┘     │
│                                                          │
│   ┌─ writer_thread ──────────────────────────────┐      │
│   │  drains outgoing_rx → SerialTransport.send   │      │
│   └───────────────────────────────────────────────┘      │
│                                                          │
│   ┌─ reader_thread ──────────────────────────────┐      │
│   │  SerialTransport.recv → events_tx            │      │
│   └───────────────────────────────────────────────┘      │
│                                                          │
│   ┌─ clipboard poll thread ──────────────────────┐      │
│   │  arboard polling → outgoing_tx               │      │
│   └───────────────────────────────────────────────┘      │
│                                                          │
│   ┌─ NEW: keyboard tap thread ───────────────────┐      │
│   │  CFRunLoop + CGEventTap                       │      │
│   │  callback (when enabled flag = true):         │      │
│   │    Cmd+Enter → tap_events_tx::ToggleFS        │      │
│   │    Ctrl+Alt+G → tap_events_tx::ReleaseCapture │      │
│   │    other → outgoing_tx::Packet (KeyDown/Up)   │      │
│   │    return Drop (consume event)                │      │
│   │  callback (when disabled): return Pass        │      │
│   └───────────────────────────────────────────────┘      │
└──────────────────────────────────────────────────────────┘
```

## Technical Details

**Permission**: `AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": false])` — без авто-промпта, только проверка. Если `false` — UI показывает инструкцию.

**TapEvent enum** (новый канал):
```rust
enum TapEvent {
    ReleaseCapture,    // Ctrl+Alt+G
    ToggleFullscreen,  // Cmd+Enter
}
```

**TapHandle API**:
```rust
pub struct TapHandle {
    enabled: Arc<AtomicBool>,
}
impl TapHandle {
    pub fn enable(&self);
    pub fn disable(&self);
    pub fn is_enabled(&self) -> bool;
}
```

**CGEventTap parameters**:
- location: `kCGSessionEventTap`
- placement: `kCGHeadInsertEventTap`
- options: `kCGEventTapOptionDefault` (active tap, can drop events)
- mask: `KeyDown | KeyUp | FlagsChanged`

**Hotkey ladder** (capture mode):
| Hotkey | Действие | Куда |
|--------|----------|------|
| Cmd+Enter | Toggle fullscreen | TapEvent → UI |
| Ctrl+Alt+G | Exit capture | TapEvent → UI |
| Cmd+Space | Switch input language | Forward as Win+Space → Host |
| Cmd+C / Cmd+V | Copy/Paste | Forward as Ctrl+C/V → Host |
| Все остальные | — | Forward → Host |

**Hotkey ladder** (out of capture, через egui):
| Hotkey | Действие |
|--------|----------|
| Cmd+Enter | Toggle fullscreen |
| Ctrl+Alt+G | Enter capture |

## What Goes Where

- **Implementation Steps** (`[ ]`): код, тесты, CLAUDE.md/README обновление
- **Post-Completion** (без чекбоксов): live-тест на железе, мерж в master

## Implementation Steps

### Task 1: Расширить keymap.rs маппингом CGKeyCode → Win scancode

**Files:**
- Modify: `apps/wiredesk-client/src/input/keymap.rs`

- [ ] добавить `pub fn cgkeycode_to_scancode(keycode: u16) -> Option<u16>` рядом с существующим `egui_key_to_scancode`
- [ ] заполнить таблицу для букв A-Z (CGKeyCodes из `core-graphics::event::KeyCode`)
- [ ] добавить цифры 0-9, F1-F12, Space, Enter, Tab, Escape, Backspace, стрелки
- [ ] добавить модификаторы (Cmd, Opt, Ctrl, Shift) — приходят как `kCGEventFlagsChanged`. Pure-функция: `cg_flag_change_to_scancodes(flags: u64, prev: u64) -> Vec<(u16, bool)>` (state хранится в callback, не в этой функции). Cmd→Ctrl mapping: при появлении в flags бита Command возвращать (0x1D, true), при пропадании — (0x1D, false). Аналогично Opt→Alt (0x38), Shift→0x2A, Ctrl→0x1D (если Cmd не активен).
- [ ] write tests для всех букв (table-driven)
- [ ] write tests для модификаторов (down/up по разнице flags), edge case: Cmd активен → не выдавать second Ctrl-press при Ctrl down (избежать дублирования mapping)
- [ ] write tests для специальных клавиш и стрелок
- [ ] cargo test -p wiredesk-client — must pass before next

### Task 2: Создать keyboard_tap.rs со скелетом и permission check

**Files:**
- Modify: `apps/wiredesk-client/Cargo.toml`
- Create: `apps/wiredesk-client/src/keyboard_tap.rs`
- Modify: `apps/wiredesk-client/src/main.rs`

- [ ] добавить в Cargo.toml `[target.'cfg(target_os = "macos")'.dependencies]`: `core-graphics = "0.25"`, `core-foundation = "0.10"`, `accessibility-sys = "0.1"` (для `AXIsProcessTrustedWithOptions` — он в ApplicationServices, не в core-graphics)
- [ ] создать `keyboard_tap.rs` с public API: `TapHandle`, `start(outgoing_tx, tap_events_tx) -> TapHandle`, `is_permission_granted() -> bool`
- [ ] на не-macOS: stub-реализации (no-op tap, permission всегда true)
- [ ] на macOS: пока что только permission check через `AXIsProcessTrustedWithOptions` (без самого tap'а)
- [ ] подключить модуль в `main.rs` (`mod keyboard_tap;`)
- [ ] write test: `is_permission_granted()` возвращает bool без паники
- [ ] write test: `start()` не падает на пустых каналах (placeholder)
- [ ] cargo build --release && cargo test -p wiredesk-client — must pass before next

### Task 3: Реализовать CGEventTap в отдельном потоке

**Files:**
- Modify: `apps/wiredesk-client/src/keyboard_tap.rs`

- [ ] в `start()` спавнить thread, в нём создавать `CGEventTap::new(Session, HeadInsertEventTap, Default, mask=KeyDown|KeyUp|FlagsChanged, callback)`
- [ ] mask также включает `kCGEventTapDisabledByTimeout` и `kCGEventTapDisabledByUserInput` чтобы получать re-enable события
- [ ] callback при `kCGEventTapDisabledBy*`: вызывает `CGEventTapEnable(tap_ref, true)` и возвращает `None` (drop)
- [ ] callback при KeyDown/KeyUp/FlagsChanged: log::debug! и возвращает `Some(event)` (placeholder, пропускаем — реальная логика в Task 4). Возврат API core-graphics 0.25 = `Option<CGEvent>`: None=drop, Some(evt)=pass.
- [ ] добавить tap source в `CFRunLoop::current()`, **сохранить `CFRunLoopRef` в `Arc<Mutex<Option<CFRunLoopRef>>>`** перед `CFRunLoopRun()` (для shutdown)
- [ ] graceful shutdown в `Drop for TapHandle`: вызвать `CFRunLoopStop(saved_ref)` через сохранённый ref → runloop выходит, thread завершается. Если ref == None (тап не стартанул) — no-op.
- [ ] join thread в Drop с таймаутом 1с (на случай если runloop не реагирует — fallback warn-лог)
- [ ] write test: `start()`+drop не паникует, нет утечек потоков (можно проверить thread count)
- [ ] cargo build --release && cargo test -p wiredesk-client — must pass before next
- [ ] live-проверка: запустить клиент, дать permission, увидеть log::debug сообщения о клавишах (callback вызывается, события пропускаются на macOS); потом cleanly выйти и убедиться что процесс завершается без zombie-thread

### Task 4: TapHandle enable/disable + декод событий в Packet

**Files:**
- Modify: `apps/wiredesk-client/src/keyboard_tap.rs`

- [ ] `TapHandle.enable() / disable()` через `Arc<AtomicBool> enabled`. Callback читает флаг первой строкой.
- [ ] state-struct замыкается в callback closure через `move`: содержит `enabled: Arc<AtomicBool>`, `prev_flags: Arc<AtomicU64>`, `outgoing_tx: mpsc::Sender<Packet>`, `tap_events_tx: mpsc::Sender<TapEvent>`
- [ ] callback при `enabled=false` возвращает `Some(event)` (не вмешивается, macOS получает как обычно)
- [ ] callback при `enabled=true`:
  - KeyDown/KeyUp: декод `CGEventField::kCGKeyboardEventKeycode → cgkeycode_to_scancode → outgoing_tx.send(Packet::new(Message::KeyDown/Up { scancode, modifiers: 0 }, 0))`. modifiers=0 потому что Host'у важны только scancodes (modifier bits игнорируются в WindowsInjector)
  - FlagsChanged: получить current `CGEvent::flags`, прочитать `prev_flags.load`, через `cg_flag_change_to_scancodes(current, prev)` получить список (scancode, pressed), для каждого послать `Message::KeyDown/Up { scancode, modifiers: 0 }`. Записать `prev_flags.store(current)`.
- [ ] callback возвращает `None` чтобы macOS не получило событие (когда enabled и событие не служебное)
- [ ] **Sticky modifier cleanup**: при `disable()` отправить `Message::KeyUp` для всех битов в `prev_flags` (Ctrl 0x1D, Shift 0x2A, Alt 0x38), сбросить `prev_flags` в 0. Иначе Host останется с залипшим Ctrl.
- [ ] write tests: state machine TapHandle (enable/disable, is_enabled, sticky-cleanup отправляет KeyUp по списку модификаторов)
- [ ] write tests: pure-функция `cg_flag_change_to_scancodes` — табличные кейсы для всех модификаторов и комбинаций (без CGEvent-mock'ов)
- [ ] cargo test -p wiredesk-client — must pass before next

### Task 5: Локальные hotkey-перехваты Cmd+Enter и Ctrl+Alt+G

**Files:**
- Modify: `apps/wiredesk-client/src/keyboard_tap.rs`
- Modify: `apps/wiredesk-client/src/app.rs`

- [ ] добавить enum `TapEvent { ReleaseCapture, ToggleFullscreen }` в `keyboard_tap.rs`
- [ ] callback (при enabled=true) перед декодом проверяет: Cmd+Enter → `tap_events_tx.send(ToggleFullscreen)`, Drop. Ctrl+Alt+G → `ReleaseCapture`, Drop.
- [ ] в app.rs (вне capture, через egui input check) дублировать те же два хоткея: Cmd+Enter → toggle fullscreen, Ctrl+Alt+G → enter capture
- [ ] WireDeskApp owns `TapHandle`, в update() читает `tap_events_rx.try_recv()` и применяет
- [ ] write test: combo detection функция — `is_cmd_enter(keycode, flags)`, `is_ctrl_alt_g(keycode, flags)` table-driven с edge cases:
  - Cmd+Enter (только Cmd) — match
  - Cmd+Shift+Enter — НЕ match (лишний modifier)
  - Ctrl+Alt+G — match
  - Cmd+Ctrl+Alt+G — НЕ match (Cmd «лишний», anti-маска)
  - просто G — НЕ match
- [ ] cargo test -p wiredesk-client — must pass before next

### Task 6: UI cleanup в capture/fullscreen + ViewportCommand::Fullscreen

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs`

- [ ] добавить поле `fullscreen: bool` в `WireDeskApp`
- [ ] обработка `TapEvent::ToggleFullscreen` → `fullscreen = !fullscreen; ctx.send_viewport_cmd(ViewportCommand::Fullscreen(fullscreen))`
- [ ] в `update()`: если `capturing || fullscreen` — рисовать только текстовый info-блок: имя приложения, активные хоткеи, как выйти. Никаких `ui.button(...)`, никаких `ComboBox`, скрыть terminal panel.
- [ ] вне capture — текущий UI без изменений
- [ ] добавить `tap_handle.enable() / disable()` в `toggle_capture()`
- [ ] **CRITICAL**: в `update()` (стр. 386-431 текущего app.rs) убрать egui-key-forward когда `capturing=true` — теперь tap единственный источник KeyDown/KeyUp. Иначе Host получит каждое нажатие дважды. Mouse-форвард через egui остаётся (мышь не идёт через клавиатурный tap).
- [ ] write test: `WireDeskApp::should_show_chrome()` (или аналог) — возвращает false в capture/fullscreen, true иначе
- [ ] cargo test -p wiredesk-client — must pass before next
- [ ] live: переключение в capture скрывает кнопки, показывает текст; Cmd+Enter включает fullscreen; Ctrl+Alt+G выходит — кнопки возвращаются

### Task 7: Permission UX — info screen + кнопка открытия Settings

**Files:**
- Modify: `apps/wiredesk-client/src/main.rs`
- Modify: `apps/wiredesk-client/src/app.rs`

- [ ] в `main.rs` при старте: `let permission = keyboard_tap::is_permission_granted()`, передать в WireDeskApp
- [ ] WireDeskApp хранит `permission_granted: bool` + `last_perm_check: Instant`. В `update()`: если `last_perm_check.elapsed() >= Duration::from_secs(2)` — `permission_granted = is_permission_granted()`, `last_perm_check = Instant::now()`. Throttle обязателен — `update()` вызывается на каждый repaint, AXIsProcessTrustedWithOptions делать на каждом frame нельзя.
- [ ] если `!permission_granted`: показать в окне info-панель с инструкцией («Open System Settings → Privacy & Security → Accessibility → add WireDesk»), кнопка `Open Settings` запускает `Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")`
- [ ] кнопка `Capture Input` disabled при `!permission_granted`, hover-tooltip объясняет почему
- [ ] tap thread не активировать если `!permission_granted` (проверка в `start()`)
- [ ] write test: state transitions WireDeskApp по permission_granted (отдельный helper-метод)
- [ ] cargo test -p wiredesk-client — must pass before next

### Task 8: Verify acceptance criteria

- [ ] AC1 live: Cmd+Space в capture → ChatGPT/Spotlight НЕ открывается, Host получает Win+Space → переключение языка работает
- [ ] AC2 live: Cmd+C в capture → текст копируется на Windows (Ctrl+C effect), в течение 1-2с приходит в Mac clipboard через auto-sync (poll-интервал в clipboard.rs = 500мс)
- [ ] AC3 live: Cmd+Enter (любой режим) → окно ↔ fullscreen
- [ ] AC4 live: в capture mode клик мыши на любое место окна не вызывает действий
- [ ] AC5 live: первый запуск без permission → окно показывает инструкцию + кнопку Open Settings; capture button disabled
- [ ] AC6 live: Ctrl+Alt+G выводит из capture → macOS снова получает все клавиши нормально
- [ ] AC7: `cargo test --workspace` — 71 существующий тест + новые тесты проходят
- [ ] `cargo clippy --workspace -- -D warnings` чистый
- [ ] AC8 live: Cmd+Q в capture → форвардится на Host (закрывает приложение в Windows), НЕ закрывает wiredesk-client на Mac
- [ ] AC9 live: capture-mode не активирует Secure Input на macOS — проверить через `ioreg -l -w 0 | grep SecureInput`. Если активен (например, при работе с полем пароля в Mac-приложении) — задокументировать как известное ограничение

### Task 9: Документация и финализация

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md`
- Modify: `docs/setup.md`

- [ ] CLAUDE.md: новая секция «Keyboard hijack» — описание архитектуры, permission, TapEvent flow, hotkey ladder в обоих режимах
- [ ] README.md: упомянуть необходимость Accessibility permission в run-секции, обновить «What WireDesk does» с актуальными хоткеями (Cmd+Enter fullscreen, упомянуть Cmd+Space/Cmd+C через капчер)
- [ ] docs/setup.md: добавить шаг про grant Accessibility permission в первом запуске
- [ ] move plan: `mkdir -p docs/plans/completed && git mv docs/plans/20260501-keyboard-hijack.md docs/plans/completed/`
- [ ] финальный коммит на ветке, готовность к мержу в master (после live-теста)

## Post-Completion

*Items requiring manual intervention:*

**Live-тест на железе:**
- Все AC1-AC7 проверяются ВРУЧНУЮ на реальной паре машин (Windows+Mac), без этого мерж в master не делается.
- Особое внимание: Cmd+Q в capture (важно НЕ закрыть приложение случайно), Cmd+Tab (не уйти в другое Mac-приложение).

**Мерж в master:**
- После успешного live-теста: `git checkout master && git merge feat/keyboard-hijack --no-ff` (no-ff для сохранения истории фичи).
- Push в origin.
- Удаление feature-ветки локально и на origin (опционально).

**Известные ограничения** (зафиксировать в README после мержа):
- macOS Secure Input — когда фокус на поле пароля (любое приложение), CGEventTap отключается системой, capture перестанет работать. Workaround: переключиться в другое окно перед capture.
- Sandboxing/notarization — для распространения подписанного билда понадобится entitlement; для personal-use не требуется.

**Возможные follow-ups (вне scope этого плана):**
- Удалить кнопку «Ctrl+Alt+Del» (не работает из-за SAS, обсудить отдельно).
- Удалить кнопку «Lang (Win+Space)» (теперь работает напрямую через CGEventTap).
- Убрать кнопку «Win key» если не нужна (или оставить как fallback).
- Auto-detection screen resolution на Host (сейчас захардкожено в дефолтах).
