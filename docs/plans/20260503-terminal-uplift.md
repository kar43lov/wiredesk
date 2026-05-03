# Terminal uplift

## Overview

Полировка двух способов доступа к Host shell, которые уже есть в коде:

1. **GUI shell-panel** в Mac client'е (Terminal collapsing внутри `wiredesk-client`) после нажатия Enter теряет фокус на input field — следующую команду нельзя ввести без клика. Цель: вернуть фокус автоматически и устанавливать его сразу при открытии панели.

2. **`wiredesk-term`** (отдельный CLI bridge для Ghostty/iTerm) не отправляет heartbeat'ы host'у — после ~6/30 сек idle host разрывает session по таймауту. Цель: добавить heartbeat-thread так чтобы interactive-сессия выживала между нажатиями.

3. **Документация** не описывает использование `wiredesk-term` отдельно от GUI и взаимоисключение между ними.

Подход: минимальная полировка (Подход A из брейншторма `docs/briefs/terminal-uplift.md`). Daemon multiplexing, PTY (для vim/htop/sudo), reconnect, SIGINT routing — **вне** этого scope.

## Context (from discovery)

- Файлы:
  - `apps/wiredesk-client/src/app.rs:1505-1519` — render shell input в `TextEdit::singleline`. `resp.lost_focus() && Enter pressed` → send. Нет re-focus.
  - `apps/wiredesk-client/src/app.rs:907-936` — `shell_open_request`/`shell_close_request`/`shell_send_input`.
  - `apps/wiredesk-client/src/app.rs:1138-1149` — `shell_append_output` с MAX-trim.
  - `apps/wiredesk-term/src/main.rs:144-195` — `bridge_loop`. Spawn'ит `reader_thread` (serial→stdout) и main-thread (stdin→serial). **Heartbeat нет.**
  - `apps/wiredesk-term/src/main.rs:100-141` — `handshake`. После `HelloAck { host_name, .. }` печатает только `connected to '<host_name>'`. Screen size игнорируется.
  - `wiredesk-protocol::message::Message::Heartbeat` — уже определён, host корректно его обрабатывает (см. `apps/wiredesk-host/src/session.rs`).
- Паттерны:
  - В `wiredesk-client` heartbeat'ы шлёт writer thread каждые 2 сек по таймауту recv (`main.rs::writer_thread`). На `wiredesk-term` той же логики нет.
  - `Arc<Mutex<Box<dyn Transport>>>` в term'е — общий мьютекс для stdin/reader threads. Heartbeat будет третьим thread'ом с тем же mutex'ом.
  - В egui для удержания фокуса используется `Response::request_focus()` — egui сам трекает focused widget по Id'у (auto-id или `id_salt`).

## Development Approach

- **Testing approach**: Regular (code first → tests immediately после в той же task). Это выбор из брифа — Quick fix UX'а не критичен для test-first; важнее — чтобы тест существовал и был полезен.
- Каждая task закрывается полностью со своими тестами до перехода к следующей.
- **CRITICAL: каждая task должна включать новые/обновлённые тесты для изменений в этой task'е.**
- **CRITICAL: все тесты должны проходить перед началом следующей task'и.**
- Запускать `cargo test --workspace` после каждой task'и.
- `cargo clippy --workspace -- -D warnings` чисто.
- Сохранять обратную совместимость с протоколом — никаких новых message types.
- Обновлять этот план если scope меняется.

## Testing Strategy

- **Unit-тесты** обязательны для каждой task'и:
  - GUI focus task: pure helper-функция `should_focus_shell_input(...)` если получается без AppKit-зависимостей. Если egui Id-логика делает это нетестируемым — задокументировать AC1/AC2 как live-only.
  - Heartbeat task: pure helper для интервала + интеграционный тест с `MockTransport` который проверяет что heartbeat отправляется регулярно.
  - Banner task: pure формат-функция типа `format_connected_banner(host_name, screen_w, screen_h)` с unit-тестами.
- **E2E / live-тесты**: AC1-AC5 из брифа. Запуск `wiredesk-term` в Ghostty + GUI client → live-проверка focus, response, heartbeat-survival.
- WireDesk не имеет автоматизированных UI/E2E тестов — live-тестирование вручную после каждой task'и.

## Progress Tracking

- Завершённые items помечать `[x]` сразу.
- Новые task'и (если найдутся) — с префиксом `➕`.
- Блокеры — `⚠️`.
- План синхронизировать с реальной работой.

## Solution Overview

**Архитектурно ничего не меняется** — оба бинаря остаются. Three точечных правки:

1. GUI: `resp.request_focus()` после send'а + при первом frame'е после `shell_open=true`.
2. CLI: третий thread в `bridge_loop` для periodic Heartbeat. Использует существующий `Arc<Mutex<Transport>>`.
3. CLI: расширение баннера в `handshake` чтобы показывать host_name + screen size.
4. Docs: новая секция в README + CLAUDE.md.

## Technical Details

### GUI focus

egui `Response::request_focus()` запрашивает фокус на следующем frame'е. Pattern:

```rust
let resp = ui.add(TextEdit::singleline(&mut self.shell_input).id_salt("shell_input"));
let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
if enter && !self.shell_input.is_empty() {
    want_send = true;
    resp.request_focus(); // сразу после send'а возвращаем фокус
}
// Auto-focus при первом open: trekаем `shell_just_opened: bool` флаг.
if self.shell_just_opened {
    resp.request_focus();
    self.shell_just_opened = false;
}
```

`shell_just_opened` ставится в `true` внутри `shell_open_request`.

### CLI heartbeat

```rust
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);

let hb_transport = transport.clone();
let hb_stop = stop.clone();
let heartbeat = thread::spawn(move || {
    while !hb_stop.load(Ordering::Relaxed) {
        thread::sleep(HEARTBEAT_INTERVAL);
        if hb_stop.load(Ordering::Relaxed) { break; }
        if let Ok(mut t) = hb_transport.lock() {
            let _ = t.send(&Packet::new(Message::Heartbeat, 0));
        }
    }
});
```

При выходе bridge_loop — `stop.store(true)` и `heartbeat.join()`.

### CLI banner

`HelloAck { host_name, screen_w, screen_h, .. }` — `screen_w` и `screen_h` уже в payload (используется в client'е). Формат:

```
wiredesk-term: connected to 'wiredesk-host' (2560×1440). Press Ctrl+] to quit.
```

Pure helper `format_connected_banner(host_name: &str, w: u16, h: u16) -> String` — testable.

## What Goes Where

- **Implementation Steps (`[ ]`)**: код + тесты + docs внутри репо.
- **Post-Completion**: live-тесты на железе (закрытие GUI, запуск term'а в Ghostty, проверка idle-survival).

## Implementation Steps

### Task 1: GUI shell — auto-focus on open and after send

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs`

- [ ] добавить поле `shell_just_opened: bool` в `WireDeskApp` (рядом с `shell_open`).
- [ ] инициализировать `false` в `WireDeskApp::new` и тестовом `make_app`.
- [ ] в `shell_open_request` (line ~907) ставить `self.shell_just_opened = true` когда `shell_open` переходит из `false` в `true`.
- [ ] в render shell input (line ~1505): дать `id_salt("shell_input")` TextEdit'у. После `if want_send` (после Enter): вызвать `resp.request_focus()`. Если `self.shell_just_opened` истинен — `resp.request_focus()` и сбросить флаг в `false`.
- [ ] написать unit-тест `shell_just_opened_set_on_open` в `app.rs::tests`: вызвать `shell_open_request` дважды (первый — false→true, второй — true→true) и проверить что флаг устанавливается только на первом переходе. Если floor-нюансы делают это hard — fall back на live-only AC.
- [ ] `cargo test -p wiredesk-client` — должен проходить, перед task 2.

### Task 2: wiredesk-term — heartbeat thread

**Files:**
- Modify: `apps/wiredesk-term/src/main.rs`

- [ ] в `bridge_loop` (line ~144) после `let stop = ...` и перед `reader_thread` spawn — spawn'нуть `heartbeat_thread(transport.clone(), stop.clone())`.
- [ ] функция `heartbeat_thread(transport, stop)`: while !stop, `thread::sleep(2s)` (короткими intervalами по 100ms чтобы быстро выйти на stop), затем `transport.lock().send(Heartbeat)`. Игнорировать send-error'ы (heartbeat best-effort).
- [ ] в shutdown-блоке `bridge_loop` дождаться heartbeat-thread'а через `.join()` после reader-thread'а.
- [ ] написать unit-тест с `MockTransport`: запустить heartbeat, подождать 2.5 сек, остановить; проверить что мок получил минимум 1 Heartbeat-packet. Использовать `wiredesk-transport::mock` если он есть; иначе скип с note и покрыть live-тестом.
- [ ] `cargo test -p wiredesk-term` — должен проходить, перед task 3.

### Task 3: wiredesk-term — startup banner с host_name + screen size

**Files:**
- Modify: `apps/wiredesk-term/src/main.rs`

- [ ] добавить pure helper `format_connected_banner(host_name: &str, w: u16, h: u16) -> String` возвращающий `"connected to 'X' (W×H). Press Ctrl+] to quit."`.
- [ ] в `handshake` после `HelloAck { host_name, screen_w, screen_h, .. }` использовать этот helper для `eprintln!`.
- [ ] написать unit-тесты: `format_banner_typical_resolution`, `format_banner_zero_size_does_not_panic`.
- [ ] `cargo test -p wiredesk-term` — должен проходить, перед task 4.

### Task 4: Verify acceptance criteria (live)

- [ ] AC1 — открыть Terminal panel в GUI client'е → курсор сразу в input field, можно печатать без клика.
- [ ] AC2 — ввести `dir` + Enter → ответ → следующая команда без клика.
- [ ] AC3 — попытаться запустить `wiredesk-term` при работающем GUI → один из них fail'ит на serial-port-busy (acceptable).
- [ ] AC4 — закрыть GUI, запустить `wiredesk-term` → баннер с host_name+size, ввести команду, ждать > 30 сек, ввести ещё → должно работать.
- [ ] AC5 — Ctrl+] → graceful exit, terminal restored cleanly, host log показывает ShellClose.
- [ ] `cargo test --workspace` — финальный прогон.
- [ ] `cargo clippy --workspace -- -D warnings` — чисто.

### Task 5: Update documentation

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`

- [ ] README: новая секция «Run from your terminal» (или дополнить существующую `Client (macOS) — terminal only`) с примером alias `alias wd='wiredesk-term'`, пометкой про взаимоисключение с GUI client'ом.
- [ ] CLAUDE.md: упомянуть `format_connected_banner` helper в Architecture / Module map. Добавить gotcha про heartbeat-thread и mutex.
- [ ] Перенести этот план в `docs/plans/completed/`.

## Post-Completion

**Manual verification:**
- Live-тест на железе с CH340-кабелем подключённым к Win host'у. Все AC1-AC5 — глазами пользователя.
- Проверить что Ghostty/iTerm корректно отрисовывают вывод (`ls`, цветной prompt powershell'а, Cyrillic text).
- Опционально — измерить время на disconnect при отключённом heartbeat'е (для regression baseline).

**Внешние интеграции:**
- Нет — изменения только в репо.
