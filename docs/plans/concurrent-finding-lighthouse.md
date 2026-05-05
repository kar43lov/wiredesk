# Investigation: `wd --exec` host channel hang after broken-quoting timeout

> Auto-generated filename `concurrent-finding-lighthouse.md` оставлен (требование plan-mode). При implementation переименовать в `wd-exec-channel-hang-investigation.md`.

## Context

**Что произошло** (live-test 2026-05-05, ветка `chore/wd-exec-quoting-probe`):

1. `wd --exec --ssh prod-mup "echo hello"` → ok.
2. `wd --exec --ssh prod-mup "curl ... -d \"{\\\"query\\\":...}\""` → **timeout 124** (sentinel не пришёл).
3. `wd --exec --ssh prod-mup "echo recovery-check"` через 5s → ok (канал восстановился).
4. Повтор того же broken-quoting cmd → `transport: IPC read: failed to fill whole buffer`, exit 1.
5. User: «host-канал зависает, я его перезапускаю».

**Почему важно:** даже если quoting-bug отдельно не фиксим (есть workaround base64 в docs), сам факт что **timeout одной команды роняет канал на следующую** — reliability issue, который проявится при любых broken / network-stalled / кривых командах. Прод-агент должен уметь восстанавливаться без manual restart host'а.

**Цель этого расследования:** понять root-cause и решить что с ним делать.

## Root cause (диагноз)

Цепочка событий после Test 2:

1. **Bash на remote stuck в unclosed quote** — `\"` collapse'ится где-то по chain'у (location not localized; наша probe в `format_command` invariant'ы сохраняет, collapse downstream — PS native arg parsing для `{...}` или ssh-tt PTY echo).
2. **Runner timeout** — `crates/wiredesk-exec-core/src/runner.rs::run_oneshot` ждёт sentinel'а, не получает за 30s, возвращает `Err(ExecError::Timeout(buf))`.
3. **IPC handler шлёт response → потом ShellClose** — `apps/wiredesk-client/src/ipc.rs:290-326`:
   ```rust
   Err(ExecError::Timeout(_buf)) => IpcResponse::Exit(124)
   write_response(&mut stream, &final_frame);  // 124 ушло клиенту
   outgoing_tx.send(Packet::new(Message::ShellClose, 0));  // потом ShellClose
   ```
   `single_inflight` mutex (строка 191) освобождается на return — **до** того как host shell реально умер.
4. **Host shell не умирает мгновенно** — `apps/wiredesk-host/src/session.rs:397-399` вызывает `sh.kill()`, но stuck-в-readline ssh-child застрял в `WCHAN=read`, SIGTERM/SIGKILL разблокирует с lag'ом 1-2s.
5. **Recovery-check (через 5s) проходит** — race window закрылся, новый ShellOpen на чистом host'е работает.
6. **Test 3 (немедленно повтор) попадает в активный race** — `crates/wiredesk-exec-core/src/ipc.rs:79::read_frame::read_exact` получает `UnexpectedEof` mid-frame → форматируется как `"IPC read: failed to fill whole buffer"`.

**Дополнительный фактор:** `apps/wiredesk-host/src/session.rs:157-176` heartbeat 6s/30s — host убивает shell если нет heartbeat'а, но `run_oneshot` синхронный и блокирует GUI main loop. На long-running cmd (>6s idle) host сам делает кооперативный disconnect, что усугубляет state-confusion.

**Логи:** `host.log.YYYY-MM-DD` есть на Win11 (`%APPDATA%\WireDesk\`). Mac client'а **файлового логирования нет** — `apps/wiredesk-client/src/main.rs:45` пишет только в stderr через `env_logger`. Post-mortem live-инцидента на Mac стороне сейчас невозможен.

## Recommended approach

**Two-step minimum-risk path: добавить Mac-side file logging + зафиксировать findings в memory. Hang fix откладываем.**

### Step 1: Mac client file logging (~1-2 часа)

Скопировать паттерн с `wiredesk-host` (уже есть `tracing-appender::rolling::daily` в `apps/wiredesk-host/src/main.rs`):

- Init: `tracing-appender::rolling::daily(dir, "client.log")` где `dir = ~/Library/Application Support/WireDesk/`.
- `tracing-log::LogTracer` чтобы legacy `log::*` macros шли через тот же appender (как на host, см. `apps/wiredesk-host/src/main.rs::install_panic_hook`).
- env-фильтр через `RUST_LOG` env var с дефолтом `info` (как сейчас).
- Stderr дублирующий sink тоже сохранить (для запуска из терминала / Xcode console).

**Why this first:** наблюдаемость — prerequisite для любого debug'а. Сейчас следующий live-инцидент мы не разберём без воспроизведения.

**Risk:** минимальный. Только initialisation hook, runtime код не трогается. Master стабилен.

### Step 2: Memory write

Зафиксировать root-cause в `~/.claude/projects/.../memory/feedback_wd_exec_timeout_cleanup_race.md` (новый файл):
- Симптом + chain
- Recovery procedure (5s sleep ИЛИ restart host)
- Точки кода (refs выше)
- Workaround в commit-time (sequential calls после timeout — sleep 5s минимум, не доверять channel'у мгновенно)

Это страхует от повторного recompiling того же исследования в future-сессиях.

## Critical files (touch)

Только два, оба в `apps/wiredesk-client/`:
- `apps/wiredesk-client/Cargo.toml` — добавить `tracing-appender = "0.2"`, `tracing-log = "0.2"` (host версии для consistency)
- `apps/wiredesk-client/src/main.rs:45` — заменить `env_logger::Builder` на `tracing_subscriber` setup со file appender и stderr layer

Reuse:
- Паттерн в `apps/wiredesk-host/src/main.rs` (init + LogTracer + panic hook). Скопировать почти 1:1, только path другой и role label другой.

## What is NOT in scope

- **Hang race fix** (Вариант C из черновика — explicit `sh.close() + wait + sh.kill()` + UnexpectedEof retry в term/main.rs). Это medium-effort правка в недавно стабилизированный IPC bridge (PR #15 неделю назад). Откладываем до отдельного брифа когда соберётся больше эпизодов hang'а ИЛИ это уже мешает прод-эксплуатации регулярно.
- **Quoting bug fix** (`--stdin` flag). Уже отложен — workaround base64-passthrough в docs.
- **Heartbeat-during-IPC** (parallel heartbeat thread из reader_thread). Тоже C-tier, не критично.

## Verification

После Step 1:

```bash
# 1. Запустить WireDesk.app fresh:
open target/release/WireDesk.app
sleep 3

# 2. Вызвать wd --exec, который timeout'ится:
./target/release/wiredesk-term --exec --timeout 5 --ssh prod-mup "sleep 30"

# 3. Найти client.log:
ls -la ~/Library/Application\ Support/WireDesk/client.log.*

# 4. Проверить что инцидент-trace есть:
tail -50 ~/Library/Application\ Support/WireDesk/client.log.$(date +%Y-%m-%d)
```

Ожидание: лог содержит lifecycle-events `info` уровня (ShellOpen, recv loop, timeout fired, ShellClose sent), плюс `warn` для timeout'а. Это первый baseline.

После Step 2: `MEMORY.md` index'е появилась строка с pointer'ом на новый feedback memory.

## Open questions для согласования

1. **Скинешь `host.log.2026-05-05` с Win11 для cross-check?** Дополнит диагноз host-side state-transitions. Не блокер для Step 1 (file logging) — Step 1 даёт инструмент для будущих инцидентов независимо.
2. **OK с откладыванием hang-fix'а?** Альтернатива: если hang мешает прод-эксплуатации сейчас сильно, сразу делаем Step 1 + Step 3 (cleanup race fix). Это +3-4 часа и trauma для IPC bridge.
3. **Возвращаемся к quoting probe (Test 3 + Test 4) после file logging?** Test 4 (workaround base64) скорее всего сработает; hang issue не помешает если делать с sleep 5s между запусками.
