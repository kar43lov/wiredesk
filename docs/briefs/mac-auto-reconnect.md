# Бриф: Mac client auto-reconnect после host disconnect

**Status:** SHIPPED 2026-06-04, master `6f35c34` → реализован как компонент 2 объединённого scope `docs/briefs/serial-error-storm-recovery.md` (план `docs/plans/completed/20260604-error-storm-recovery.md`). Mac LinkSupervisor (`apps/wiredesk-client/src/link.rs`) делает полный in-process reconnect с backoff 1s→30s — закрывает все сценарии этого брифа (host quit, кабель, любой disconnect). Live-verified. Этот файл остаётся детальной спекой компонента (Вариант 1, AC, риски).

**Ранее:** SUPERSEDED 2026-06-04 → поглощён `docs/briefs/serial-error-storm-recovery.md` (Mac reconnect loop = компонент 2 объединённого scope; этот файл остаётся детальной спекой компонента — Вариант 1, AC, риски актуальны).

## Контекст

Сейчас когда `WireDesk.app` теряет связь с host'ом (heartbeat-timeout, кабель выдернули, host'у плохо, host'у crashed) — **Mac client не восстанавливается автоматически**. Пользователю приходится вручную делать одно из:

1. Killall + relaunch `wiredesk-client` (или Quit + reopen WireDesk.app)
2. Save & Restart через Settings UI

В сессии **2026-05-06** наблюдалось **5+ ручных reopen'ов** только за один investigation cycle (heavy `wd --exec` cmd'ы оставляли канал в bad state, хотя PR #20+#21 покрыли cascade-bug — disconnect ещё может случиться по другим причинам).

PR #21 (post-run drain) сломал главную причину disconnect-cascade (heavy-output cmd'ы), но **полностью устранить disconnect никогда нельзя** — кабель действительно может выдернуться, host действительно может слететь, Continent VPN действительно может стопнуть Win11 host'а на минуту.

## Кому это нужно

- Solo-user setup автора. Каждый disconnect = manual reopen, потеря контекста.
- AI-агенты использующие `wd --exec` — после disconnect'а wd-term падает с `transport: IPC read: failed to fill whole buffer` (`crates/wiredesk-exec-core/src/ipc.rs:79`). Сейчас агенту негде узнать «канал восстановится через минуту, попробуй снова». С auto-reconnect first-party retry — wd-term подождёт re-handshake и пройдёт сам.

## Цель

Mac client'у автоматически детектит disconnect и пытается reconnect'нуться без user-action'а. Restore handshake → возобновить heartbeat'ы → IPC handler'ам отдать ошибку «transport unavailable» (не unexpected EOF) пока reconnect в процессе → next IPC request попадает на восстановленный канал.

## Гипотеза по реализации

В `apps/wiredesk-client/src/main.rs::writer_thread` сейчас `transport.send` ошибка → `events_tx.send(TransportEvent::Disconnected)` → return из thread'а. После этого писатель мёртв.

Reader thread аналогично: на serial read error → возвращает Disconnected. Тоже мёртв.

Соответствующая работа:
1. **Disconnect-detection consolidation** — main loop собирает `TransportEvent::Disconnected` из reader + writer, переходит в `Reconnecting` state.
2. **Reconnect loop** в main thread (или dedicated thread): пытается `SerialTransport::open` каждые 2s, exponential backoff up to 30s между попытками.
3. **На успешном open**: respawn reader_thread + writer_thread с новыми transport handles, отправить новый Hello, дождаться HelloAck.
4. **Во время reconnect'а**: IPC handler'ы видят `outgoing_tx.send` ошибку (writer-thread dead) → возвращают клиенту `IpcResponse::Error("transport reconnecting")`. Term-side показывает "канал восстанавливается, retry через N сек" вместо unexpected-EOF.
5. **Status-bar UI**: дополнительный `SessionStatus::Reconnecting { attempt: u32 }` для visual feedback в WireDesk.app.

## Подходы

### Вариант 1 (preferred) — In-process reconnect loop

Один `ReconnectController` в main thread который:
- Watches `events_rx` для `Disconnected` event'а
- Стопит running threads (drop'ает их `outgoing_tx`/`exec_slot` для clean shutdown)
- Loops `SerialTransport::open` с backoff
- Respawn'ит threads with fresh transport
- Re-Hello

**Плюсы:** Никакого process-level restart, GUI остаётся открытым (capture-mode state, settings panel state preserved).

**Минусы:** state-machine сложнее (5+ phases). Сложно закрыть все edge case'ы (e.g., reconnect во время clipboard transfer).

### Вариант 2 — Self-respawn через `restart.rs`

При disconnect — `restart::self_relaunch()` (existing helper). Process exit'ится, новый процесс открывается через `open -n WireDesk.app`.

**Плюсы:** Простота. Reuse existing infrastructure. State полностью clean (никаких race-window'ов).

**Минусы:** GUI window-state теряется (capture/fullscreen/window position сбрасываются). User видит "WireDesk пере-открылся" — не auto-recovery, а auto-restart.

### Вариант 3 (hybrid) — In-process попытка + fallback на restart

In-process reconnect (Вариант 1) с timeout 30s или 5 попыток. Если не recовert'ил'ось — Self-respawn (Вариант 2). Best of both.

**Recommend:** Вариант 1 (preferred), Вариант 3 если live-test'ы покажут что in-process reconnect недостаточно robust.

## Acceptance criteria

1. **AC1 (disconnect → reconnect):** Симулятор unplug — отключить `wiredesk-host.exe` через tray Quit, подождать 30s, запустить заново. Mac client должен **автоматически** показать `Reconnecting` status, потом `Connected`. Без user-action'а.

2. **AC2 (in-flight wd --exec не теряется):** Если в момент disconnect'а активен `wd --exec`, term-процесс получает `IpcResponse::Error("transport reconnecting")` (не unexpected EOF). Exit-код 125 (transport-class). После reconnect'а next `wd --exec` идёт нормально.

3. **AC3 (reader/writer threads cleanly respawn):** Логи показывают `INFO transport reconnecting attempt=1`, `INFO transport reconnected after 5.2s`. Никаких leaked thread'ов, никаких double-Hello.

4. **AC4 (UI feedback):** В chrome-mode WireDesk.app status-bar показывает «Reconnecting…» во время попыток. В capture-mode — банер становится yellow или с message «host disconnected».

5. **AC5 (regression):** Если host действительно недоступен (cable unplugged) — reconnect loop НЕ блокирует main thread / UI. WireDesk.app остаётся interactive (юзер может открыть Settings, выйти из capture mode и т.п.).

## Риски

- **Race с in-flight clipboard transfer:** Disconnect посреди image push'а оставляет partial state в `pending_outbox` host'а. После reconnect'а host получает свежий Hello → `pending_outbox.clear()`? Уже? Проверить и at добавить если нет.
- **IPC connections в момент disconnect'а:** Активные IPC connection'ы должны получить ошибку, не висеть. Sentinel-detection в run_oneshot уже handle'ит `Err(ExecError::Closed)` — после disconnect'а transport.recv_event возвращает Closed → run_oneshot exit'ится с error → IPC handler шлёт error frame клиенту.
- **Reconnect storm:** Если host реально down (cable disconnected), reconnect loop работает в background. Backoff 2s→4s→8s→16s→30s→30s→...→30s — не съедает ресурсы.
- **Settings/config reload:** Если user'ом меняли config (через Settings panel) пока был disconnect — после reconnect'а используется memory-cache config'а, не перечитываем с диска. Принципиально (config.toml применяется только на restart). Документировать в acceptance.

## Сложность

**medium-high.** State-machine в main thread + thread-respawn logic + IPC error-translation + UI status. Оценка ~2-3 дня для Варианта 1, +1 день если докинуть Вариант 3 fallback.

## Что НЕ входит в scope

- **Auto-reconnect host-side**: host уже handle'ит reconnect (см. session.rs `WaitingForHello` state). Этот бриф только про Mac.
- **Heartbeat decoupling в отдельный thread на Mac side** (Variant C из старого `concurrent-finding-lighthouse.md` плана) — отдельный fix, ортогональный auto-reconnect'у. PR #20+#21 уже покрыли наблюдаемый pain, decoupling deferred.

## Связанное

- `feedback_wd_exec_timeout_channel_hang.md` — старая cascade-проблема, теперь FIXED через PR #20+#21. Auto-reconnect — следующий уровень reliability.
- `concurrent-finding-lighthouse.md` plan (теперь COMPLETED) — упоминает auto-reconnect как Variant C followup.
- `apps/wiredesk-client/src/restart.rs` — existing self-relaunch helper, reusable для Вариант 2.

## Первые шаги

1. Repro-test live: tray Quit → 30s → relaunch host. Подтвердить что Mac client сейчас НЕ recovers автоматически.
2. Прототип Variant 1: добавить `Reconnecting` в `SessionStatus`, in-process reconnect loop в main thread.
3. Live-тест против AC1.
4. AC2 (in-flight `wd --exec` не теряется): проверить вручную через `wd --exec --timeout 30 sleep 25` + tray Quit во время.
5. AC4 (UI feedback): добавить status-bar строку.
