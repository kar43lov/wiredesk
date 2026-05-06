# Бриф: `wd --exec` follow-up quick wins (drain-on-Err + host RUST_LOG)

**Status:** ready for direct implementation (не нужен `/planning:make`, оба пункта 1-line). Branch: `chore/wd-exec-quickwins`.

## Контекст

Два минорных follow-up'а после сессии 2026-05-06 (PR #18-21). Каждый — 1-line код change + минимальный test, оба в одной области (`wd --exec` ergonomics). Объединены в один micro-PR чтобы не плодить ветки.

## Пункт 1: drain only on Err paths

### Проблема

PR #21 ввёл IPC post-run drain — handler ждёт пока wire не стихнет (2s idle deadline) **перед** освобождением `single_inflight`. Это сломало cascade-bug (✓), но добавило **+2s overhead на каждый `wd --exec`**, включая Ok exit'ы.

В sessions с `wd --exec` цепочками (агент-orchestrator скрипты, отладка) этот 2s заметен — на 10 быстрых cmd'ов набегает 20+ секунд лишнего ожидания.

### Логика fix'а

Для **Ok paths** runner уже видел sentinel или ShellExit event. Это означает что host's shell завершил cmd чисто. Tail после sentinel — ничтожный (few bytes prompt redraw). Drain не нужен.

Для **Err paths** (Timeout, CompressionFailed) — host shell **ещё активен** в момент возврата runner'а из `run_oneshot`. После ShellClose он продолжит шипить tail (наблюдалось 407KB). Drain тут **нужен** чтобы next handler попал на чистый канал.

### Fix

Файл: `apps/wiredesk-client/src/ipc.rs`, в block после `let _ = write_response(&mut stream, &final_frame);`:

```rust
// Drain only when the runner ended with the host shell still alive.
// Ok paths already saw sentinel / ShellExit — no in-flight tail to drain.
let need_drain = !matches!(result, Ok(_));
if need_drain {
    // ... existing drain loop ...
}
```

### AC

1. Test `handler_round_trip_via_unix_socket` всё ещё passes без regression'а (в нём mock host emit'ит ShellExit → drain короткий уже).
2. Live-test 10× `wd --exec "echo hello"` — total time **~10× быстрее** чем было (раньше 10×~4s=40s, после ~10×~2s=20s).
3. Live-test heavy-failed cmd → next cmd survives (cascade всё ещё broken).
4. Логи `INFO IPC: post-cleanup drain: ...` теперь появляются **только** на Err paths.

## Пункт 2: host RUST_LOG=debug works

### Проблема

`apps/wiredesk-host/src/logging.rs::init_logging_at` использует `tracing_subscriber::fmt()` без env-filter. Это значит host **игнорирует** `RUST_LOG=debug` env-var — логи всегда INFO-level.

В сессии 2026-05-06 при investigation channel-hang'а это reduced visibility — мы могли получить debug-логи только с Mac (`RUST_LOG=debug,wiredesk_exec_core=trace ./target/release/WireDesk.app/Contents/MacOS/wiredesk-client &`), но не с host. Cross-stack debug-trace был неполный.

Mac client уже использует env-filter (PR #19, `apps/wiredesk-client/src/logging.rs:74`):
```rust
let filter = tracing_subscriber::EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
```

Cargo.toml host'а уже содержит feature `env-filter` (`tracing-subscriber = { version = "0.3", features = ["fmt", "env-filter"] }`) — просто не применяется в init.

### Fix

Файл: `apps/wiredesk-host/src/logging.rs`, добавить filter в init:

```rust
let filter = tracing_subscriber::EnvFilter::try_from_default_env()
    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

let _ = tracing_subscriber::fmt()
    .with_writer(non_blocking)
    .with_env_filter(filter)  // ← NEW
    .with_ansi(false)
    .with_target(false)
    .try_init();
```

Зеркалит pattern Mac client'а 1-в-1.

### AC

1. Запуск host'а с `set RUST_LOG=debug` (или `$env:RUST_LOG="debug"` в PS) → `host.log.YYYY-MM-DD` содержит DEBUG-events (например `[exec] uuid=... payload=...`).
2. Default behaviour (без env-var) — INFO level, как раньше. Не regress'им.
3. Тест: добавить `init_logging_with_env_filter_respects_rust_log` в `apps/wiredesk-host/src/logging.rs::tests` — set `RUST_LOG=trace` через `std::env::set_var`, init, emit `tracing::trace!`, verify в log file.

## Сложность

**Trivial.** ~30 минут общим:
- Пункт 1: 5 минут code + 10 минут live-test
- Пункт 2: 5 минут code + 10 минут test

## Что НЕ входит в scope

- Mac auto-reconnect — отдельный бриф `mac-auto-reconnect.md` (medium-high effort)
- Mac heartbeat decoupling в отдельный thread — отдельный followup из `concurrent-finding-lighthouse.md` (Variant C)
- `wd --exec --stdin` — отдельный бриф `wd-exec-payload-quoting.md` (medium effort)

## Связанное

- PR #20 (`6ec869d`) — heartbeat extends to shell-busy
- PR #21 (`68ffe6d`) — IPC post-run drain
- `feedback_wd_exec_timeout_channel_hang.md` — fixed status, эти follow-up'ы уточняют
- `apps/wiredesk-client/src/logging.rs:74` — reference pattern для пункта 2

## Первые шаги

1. `git checkout -b chore/wd-exec-quickwins` from master `68ffe6d`
2. Edit `ipc.rs` — добавить `need_drain` check
3. Edit host `logging.rs` — добавить `with_env_filter`
4. `cargo test -p wiredesk-client && cargo test -p wiredesk-host -- --test-threads=1` — все passes
5. Live-test Mac side (drain skip on Ok) — серия `wd --exec "echo X"` time'ить
6. Live-test Win side (host RUST_LOG=debug) — start with env, look at log
7. PR оба пункта в одном — как обычно через `/pg.review` + `/pg.ship`
