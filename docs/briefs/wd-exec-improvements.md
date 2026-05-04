# Бриф: wd --exec improvements (post-prod feedback)

**Status:** ready for /planning:make. Branch предложен: `feat/wd-exec-fixes`.

## Контекст

После shipping'а `wd --exec` (PR #9, merged) и ConPTY (PR #10, merged) первый прод-пользователь (триаж prod-ES через `wd --exec --ssh`) прислал список из 8 пунктов фидбека (P1–P5 + B1–B3). Скептический разбор:

- **5 пунктов отброшены** как уже сделанные / overengineering / paranoid:
  - P5 (ConPTY) — shipped 2026-05-04 в PR #10.
  - B1 (clean_stdout asymmetry) — корректно работает: PS pipe-mode не echo'ит stdin → нечего стрипать; `clean_stdout` falls back на sentinel-slice без READY-marker'а.
  - B3 (SSH cleanup) — `ShellClose + Disconnect` уже надёжно валит ssh-child, host's heartbeat-timeout 6s — backstop. Без повторяемого бага не лезем.
  - P3 (compress) — band-aid. FT232H upgrade в roadmap даст ×100 wire bandwidth, compression устареет до того как будет написан.
  - P4 (stderr separation) — пользователь сам пишет «не блокер».

- **3 пункта берём в работу** — реальные блокеры или дешёвые усиления.

## Цель

Снять блокеры реальной prod-эксплуатации `wd --exec`: bump payload limit для типичных ES-запросов, добавить диагностику при timeout (чтобы не гадать, где зависло), закрыть unit-test gap для PS terminating-error path.

## Выбранный подход

Один скоупный PR в ветке `feat/wd-exec-fixes`. Никаких новых opcode'ов, никакой архитектурной перестройки.

### Task 1: P1 — bump `MAX_PAYLOAD` 512 → 4096

**Где:** `crates/wiredesk-protocol/src/packet.rs:16`.

**Почему 4096, не 8192 и не chunked:**
- 619-байтный реалистичный ES `_search` (date_histogram + 2 terms-агг) — самый частый кейс пользователя. 4096 покрывает 4× запас.
- `len: u16` в header даёт hard ceiling 65535 — мы далеко от него.
- COBS framing add'ит ~1 byte per 254 → 4096 → ~4112 — не критично на 115200 baud (~370 ms wire-time для full-size frame'а).
- ClipChunk остаётся на 256-byte payload (это design decision для прогресс-бара, не лимит протокола).
- Chunked ShellInput (новый opcode + state-machine на host'е для reassembly) — overengineering: пользователь сам пишет «дробить запрос работает», т.е. проблема не в безлимите, а в типичных 0.5–4 KB.

**Совместимость:** Bump затрагивает обе стороны. Старый host (≤ 512) получит >512-байтный пакет → отвергнет с протокол-ошибкой → recoverable. Деплой client+host вместе (как обычно для serial-link single-user setup).

**Risk:** длинные пакеты на CH340 + Dupont'ах — теоретически рост bit-flip rate. Но на 115200 baud baseline corruption даёт нули по факту (см. CLAUDE.md `Leading 0x00 + drain on open` решение). 4 KB остаётся в зоне stability.

### Task 2: P2 — last-buffer dump при timeout

**Где:** `apps/wiredesk-term/src/main.rs::run_oneshot`, около строки 535–541.

**Что:** перед `Ok(124)` на timeout — `eprintln!` last 256 байт `full_log` (буфер уже копится по строкам, чтобы передать `clean_stdout`'у). Формат:

```
wiredesk-term: --exec timeout after 30s (no sentinel from host)
last 256 bytes received: "...<tail>..."
```

**Что НЕ делать:** phase-based timeouts (`PHASE 1..4`) — это 4 константы + 4 state-checkpoint'а + 4 отдельных error path'а ради разных сообщений. Один dump показывает где остановилось (`__WD_READY_…__` ⇒ ssh прошёл и ждём команду; MOTD текстом ⇒ ssh-handshake; пусто ⇒ host не вернул ничего). Это покрывает 80% кейсов за 5 строк кода.

**Один unit-test:** mock'нуть scenario «host тих после payload» → ожидать `Ok(124)` + stderr содержит "last 256 bytes received".

### Task 3: B2 — end-to-end test PS terminating error

**Где:** `apps/wiredesk-term/src/main.rs::tests` (или новый integration test in `apps/wiredesk-host/src/session.rs::tests`).

**Что:** mock-Transport симулирует host-side PS, который на input'е `Get-Item /nonexistent` сразу emit'ит `__WD_DONE_<uuid>__1` (попасть в `catch { $LASTEXITCODE=1 }`). Verify: `run_oneshot` returns `Ok(1)`, не timeout 124. Сейчас покрыто только static check'ом «payload содержит правильную форму try/catch» (1172–1175), end-to-end gap есть.

**Не нужны:** интеграционный тест с реальным PowerShell на host'е (нет CI на Windows для wd). Mock-test покрывает state-machine `OneShotState::AwaitingSentinel → matched → Ok(1)`.

## Acceptance criteria

1. **AC1 (P1):** unit test в `crates/wiredesk-protocol/src/packet.rs::tests` — `roundtrip(Message::ShellInput { data: vec![0xAA; 4000] })` проходит. Прежний test на overflow (`MAX_PAYLOAD + 1`) обновлён под новое значение.
2. **AC2 (P1 live):** `wd --exec "echo test"` с командой длиной 700 байт (можно собрать через `wd --exec --ssh prod-mup "$(python -c 'print(\"echo \" + \"a\" * 700)')"` или аналог) — проходит без `payload too large`. Verify локально на CH340 + Win11.
3. **AC3 (P2):** unit test — mock'нутый run_oneshot с тихим host'ом за `--timeout 1` возвращает `Ok(124)` + stderr содержит "last 256 bytes received".
4. **AC4 (P2 live):** реальный `wd --exec --ssh nonexistent-host "ping"` (ssh fail) — exit 124 + dump показывает MOTD-текст или ssh-error в stderr.
5. **AC5 (B2):** unit test — mock host emit'ит `__WD_DONE_<uuid>__1` сразу, без READY → `run_oneshot` returns `Ok(1)`, не timeout.
6. **AC6:** `cargo test --workspace -- --test-threads=1` все 348+ тестов зелёные.
7. **AC7:** `cargo clippy --workspace -- -D warnings` чисто.

## Риски

- **Bump payload triggers latent buffer-size assumption.** Reader's COBS frame buffer / serial reads. Есть один тест на full-payload roundtrip (packet.rs:228 `payload_too_large_fails`). Если где-то в transport layer hardcoded'но `512` — упадём в roundtrip-тесте. Mitigation: глобальный `grep -n "512" crates/ apps/` перед коммитом.
- **`last 256 bytes` может содержать ANSI escape-codes** (Starship prompts, ssh -tt colors). Stderr — обычно printed через terminal'овский TTY, цветовые ANSI рендерятся → читабельно. Но если кто-то pipe'ит stderr в файл — escape'ы будут видны as-is. Это норма для diagnostics dump'а; добавлять `strip_ansi` тут — overcomplication. Документирую в усеченном комментарии.
- **End-to-end PS error test не ловит реальный PS на CI.** Mock-test проверяет только client-side state machine. Регрессия в payload-format'е (`try`/`catch` сломан) не поймается. Mitigation: existing static test (1172–1175) уже покрывает payload formatting, новый тест — про state-machine. Это два независимых угла, оба нужны.

## Сложность

**low**. Один файл (packet.rs) bump'нуть константу. Один файл (main.rs) добавить ~5 строк перед exit 124. Три unit test'а. <1 дня работы.

## Тестирование

Каждая task = unit test'ы в той же директории кода. Live verification AC2/AC4 — на CH340 + Win11 (host).

## Что НЕ входит в scope

- Chunked ShellInput / новый opcode (overengineering для текущих кейсов).
- Phase-based timeouts с 4 константами и отдельными error path'ами (overengineering поверх простого dump'а).
- `--compress` / gzip wrapping (band-aid для проблемы решаемой FT232H upgrade'ом).
- Stderr separation (новый `Message::ShellStderr` opcode) — пользователь сам говорит «не блокер».
- Phantom `exit\r` перед ShellClose в --ssh path (paranoid speculation, нет повторяемого бага).
- Интеграционный test с реальным PowerShell на CI (нет Win-CI для wd).

## Первые шаги (для /planning:make)

1. Создать ветку `feat/wd-exec-fixes` от `master`.
2. Bump `MAX_PAYLOAD` в `crates/wiredesk-protocol/src/packet.rs:16` (512 → 4096). Update overflow-test'а.
3. Add roundtrip-test для 4 KB ShellInput payload'а.
4. Add last-buffer dump в `apps/wiredesk-term/src/main.rs::run_oneshot` перед `Ok(124)` (~строка 535).
5. Add 2 unit-test'а: timeout-with-dump, PS-terminating-error → exit 1.
6. `cargo test --workspace -- --test-threads=1`, `cargo clippy --workspace -- -D warnings`.
7. Live verify AC2 + AC4 на CH340 + Win11 host.
