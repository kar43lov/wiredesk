# Бриф: Terminal uplift

**Цель:** Сделать GUI shell-panel в Mac client'е usable (focus сохраняется), починить idle-disconnect в `wiredesk-term`, описать в документации оба варианта работы с Host shell и взаимоисключение между ними.

**Выбранный подход:** Минимальная полировка существующего — Подход A из брейншторма. Daemon-multiplexing для одновременного GUI+CLI пока не делаем (эксклюзивность serial-порта приемлема). PTY-улучшения (vim/htop/sudo) тоже вне scope.

## Требования

**Функциональные**
- **F1.** В GUI shell-panel после нажатия Enter focus остаётся в input field — следующую команду можно вводить без клика.
- **F2.** При первом open Terminal-collapsing'а курсор автоматически в input field.
- **F3.** `wiredesk-term` шлёт `Message::Heartbeat` раз в 2 сек так, чтобы host не disconnect'нул сессию по тишине ввода.
- **F4.** Startup-баннер `wiredesk-term` показывает host_name и screen size (как в существующем `wiredesk-client`'е по `HelloAck`).
- **F5.** Документация (CLAUDE.md + README): секция «Use wiredesk-term from your terminal» с примером alias и пометкой про взаимоисключение с GUI client'ом.

**Нефункциональные**
- Heartbeat-thread не должен race'ить с stdin-thread на serial-mutex'е — отдельный поток, та же `Arc<Mutex<Transport>>` lock-pattern что и сейчас.
- GUI focus fix не должен ломать существующий keyboard-tap path (когда capture mode активен tap должен оставаться приоритетным; shell-panel виден только в chrome-mode → конфликт исключён).

## Acceptance criteria

- **AC1.** Открыть Terminal panel в GUI → курсор уже в input field, без клика можно печатать.
- **AC2.** В GUI ввести `dir` + Enter → ответ приходит → сразу можно вводить следующую команду без клика.
- **AC3.** Запустить `wiredesk-term` в Ghostty при работающем GUI client → один из них fail'ит на serial-port-busy (приемлемо, не регрессия).
- **AC4.** Запустить `wiredesk-term` после закрытия GUI → видеть баннер с host_name, ввести `dir`, увидеть output, ждать > 30 сек idle → набрать ещё команду — работает (heartbeat поддержал session).
- **AC5.** Ctrl+] graceful exit — terminal cleanly restored, host лог показывает ShellClose.

## Тестирование

**Unit-тесты**
- Pure helper в `app.rs` для focus-state (если получится без AppKit dependency'ей). Если не выходит — пропускаем, AC1+AC2 покрываются live-тестом.
- Heartbeat-thread в `wiredesk-term`: pure-функция выбора timeout, плюс mock-test с MockTransport на регулярную отправку Heartbeat.

**Live-тесты (на железе)**
- AC1, AC2, AC3, AC4, AC5 — каждый отдельным сценарием.

## Риски

- `egui::Response::request_focus()` имеет нюансы со frame timing — на одном кадре может не подхватиться. Mitigation: тестировать на железе сразу, при необходимости дропать `Memory::request_focus(Id)` напрямую.
- Heartbeat-thread в `wiredesk-term` шлёт через тот же mutex что и stdin-thread → если `transport.send()` блокируется надолго (CH340 buffer overflow) — heartbeat может миксоваться с user data. Низкий риск: heartbeat пакет коротенький (~10 байт), а user input — байты по одной клавише.

## Первые шаги

1. **GUI focus fix** в `apps/wiredesk-client/src/app.rs:1505-1519` — `resp.request_focus()` после send'а + при первом frame'е после `shell_open=true`.
2. **wiredesk-term heartbeat thread** в `apps/wiredesk-term/src/main.rs:144` (`bridge_loop`) — третий thread, `Arc<Mutex<Transport>>`, `thread::sleep(2s)` + `Heartbeat`.
3. **wiredesk-term banner** в `handshake()` — после получения HelloAck показать `host_name` + screen size, не только short message.
4. **Docs**: новая секция в README + CLAUDE.md «Run from your terminal» с примером alias.

## Сложность

**low** — два разных бинаря, в каждом ~10-30 строк нового кода + docs. Самое непредсказуемое — egui focus timing.

## Что НЕ входит

- Daemon multiplexing (одновременный GUI+CLI).
- PTY support для vim/htop/sudo-пароля.
- Reconnect после mid-session disconnect.
- SIGINT routing на Host (Ctrl+C в local terminal сейчас не пробрасывается как сигнал в Host shell — отдельная задача).
- Output-логирование в файл.
- Тонкий wrapper-скрипт `scripts/wd` (опционально, можно после основного).
