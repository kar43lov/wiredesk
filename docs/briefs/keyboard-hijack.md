# Бриф: Keyboard hijack + fullscreen + UI-cleanup в capture-mode

**Цель:** Превратить capture-mode из «egui-окно ловит часть клавиатуры» в полноценный takeover уровня операционной системы — клавиатура целиком (включая системные шорткаты Cmd+Space, Cmd+C), окно в fullscreen по Alt+Enter, и только текстовые подсказки в окне без кликабельных элементов. Это нужно для удобной работы с WireDesk как «третьим монитором» через HDMI-capture.

**Выбранный подход:** **Hybrid CGEventTap** — нативный macOS event tap, активный только в capture-mode. Когда capture выключен, macOS работает как обычно (никаких хвостов).

**Почему этот подход:**
- Единственный способ перехватить системные шорткаты типа Cmd+Space, которые macOS обрабатывает раньше app-уровня (egui/AppKit). NSEvent-monitor этого не делает.
- Стандартный путь для подобных утилит (Karabiner, BetterTouchTool, Hammerspoon).
- Hybrid-вариант (tap on/off по флагу) даёт нулевой overhead и нулевой риск, когда capture выключен.

**Стек:**
- `core-graphics 0.25` — `CGEventTap`, `CGEventType`, `KeyCode`, `CallbackResult`
- `objc2`/`cocoa` (если понадобится для AX permission проверки — `AXIsProcessTrustedWithOptions`)
- Отдельный поток с CFRunLoop — на нём живёт tap. Callback быстрый: декодит event → `outgoing_tx.send(Packet)`.

**Требования (функциональные):**
- F1: При запуске проверка Accessibility permission. Если нет — окно показывает инструкцию (текстом) как её дать; functional capture отключён до получения permission.
- F2: В capture-mode CGEventTap активен, перехватывает все KeyDown/KeyUp/FlagsChanged. Системные комбинации (Cmd+Space, Cmd+Q, Cmd+Tab, Cmd+C) форвардятся на Host вместо macOS.
- F3: Вне capture-mode tap отключён, macOS работает 100% нормально.
- F4: **Cmd+Enter** — toggle fullscreen (через `egui::ViewportCommand::Fullscreen`). Перехватывается **локально**, не форвардится на Host. Решение по риску №4: используем Cmd+Enter (не Alt+Enter), потому что Alt+Enter — стандартный Windows-шорткат «Свойства», который пользователь захочет форвардить на Host. Cmd+Enter не занят ни на Mac, ни в Windows.
- F5: Ctrl+Alt+G — toggle capture. Работает и в, и вне capture (как сейчас).
- F6: В capture-mode UI окна — только текстовая справка: имя приложения, активные комбинации, как выйти. Никаких кнопок и кликабельных элементов.

**Требования (нефункциональные):**
- Tap callback должен возвращаться <100µs (по факту mpsc::send), чтобы macOS не отключила tap по таймауту 1с.
- Без регрессий: мышь, clipboard auto-sync, shell-over-serial, кириллица — продолжают работать.
- Без новых рантайм-зависимостей кроме `core-graphics` (уже транзитивная) и, если нужно, `accessibility-sys`.

**Acceptance criteria (live-тест):**
1. Cmd+Space в capture: ChatGPT/Spotlight НЕ открывается, Host получает Win+Space → переключение языка работает.
2. Cmd+C в capture: текст копируется на Windows (Ctrl+C effect), через 500мс приходит в Mac clipboard через auto-sync.
3. Cmd+Enter (любой режим): окно ↔ fullscreen.
4. В capture mode: клик мыши на любое место окна не вызывает действий (нет кнопок).
5. Первый запуск без permission: окно показывает инструкцию «дай Accessibility в System Settings → Privacy & Security», capture запускается только после.
6. Ctrl+Alt+G выводит из capture-mode → macOS снова получает все клавиши нормально.
7. 71 существующий тест проходит.

**Тесты:**
- Unit: маппинг `CGKeyCode` → Win-scancode (как существующий `egui_key_to_scancode`).
- Unit: state machine capture+fullscreen (toggle комбинаций).
- Integration: моковый CGEvent через тестовую обёртку — сложно, может быть пропустим.
- Manual: live-тест на реальном железе по AC1-AC7.

**Что НЕ входит в scope:**
- Auto-grant permission через `tccutil` (требует sudo, делает пользователь).
- Multi-monitor smarts / capture region.
- Картинки/файлы в буфере обмена.
- Замена egui на нативный AppKit.
- Кнопки Ctrl+Alt+Del / Win+Space убираем — они дублировали функционал, который теперь автоматический. (Может оставим Win key как alternative для редких случаев — обсудим в плане.)

**Риски:**
1. macOS auto-disables tap при медленном callback. Решение: callback только `mpsc::send`, тяжёлая логика в writer_thread.
2. CFRunLoop конфликт с eframe runloop. Решение: tap живёт на отдельном thread, eframe не трогаем.
3. Permission UX. Решение: понятное сообщение + ссылка-команда `open x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility`.
4. Alt+Enter в Windows — стандартный шорткат «свойства». Перехват его делает невозможным отправку на Host. Альтернатива: использовать Cmd+Enter (не конфликтует с Mac). Уточнить с пользователем перед реализацией.

**Первые шаги:**
1. Добавить deps: `core-graphics = "0.25"` в client `Cargo.toml`. Возможно `accessibility-sys` для permission check.
2. Написать `apps/wiredesk-client/src/keyboard_tap.rs` — модуль с CGEventTap thread. Public API: `start(outgoing_tx)`, `enable()`, `disable()`, `is_permission_granted()`.
3. Маппинг `CGKeyCode` → scancode (расширение `keymap.rs`).
4. Интегрировать в app.rs: enable/disable tap по `self.capturing`, обновить UI (info-only в capture).
5. Alt+Enter → ViewportCommand::Fullscreen.
6. Проверка permission на старте, fallback UI с инструкцией.

**Сложность:** medium (≈300-500 строк нового Rust, FFI к Core Graphics, аккуратная работа с потоками и runloop).

**Где живёт работа:** ветка `feat/keyboard-hijack`. Master стабилен. Мерж только после live-теста.
