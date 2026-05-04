# wd --exec post-prod fixes (P1 + P2)

## Overview

Snять реальные блокеры `wd --exec` после первого прод-использования (триаж prod-ES через wd):
- **P1**: bump `MAX_PAYLOAD` 512 → 4096 — типичные ES `_search` запросы (619+ байт) сейчас падают с `payload too large`. **Также нужно bump'нуть hardcoded'ный 1024-byte frame limit в `SerialTransport::recv`** — иначе live-trip упадёт несмотря на packet-roundtrip pass.
- **P2**: last-buffer dump перед `exit 124` — диагностика «где зависло» при timeout без phase-based machinery.

**B2 удалён из scope** — после plan-review нашлось что existing тест `run_oneshot_propagates_nonzero_exit` (apps/wiredesk-term/src/main.rs:1528) уже покрывает B2 kernel: sentinel-matching с ненулевым exit code prop'ается через state-machine. Конкретный exit code (1 vs 7) — литерал, не разница в pathway. State-machine `OneShotState::AwaitingSentinel → parse_sentinel → Some(N) → Ok(N)` identical. Существующий static test (1170–1175) проверяет payload format с `try`/`catch` → `$LASTEXITCODE=1`. Дополнительный test был бы copy-paste'ом 1528 с другой константой.

Подробное обоснование почему scope именно такой (и почему P3/P4/P5/B1/B3 отброшены) — в `docs/briefs/wd-exec-improvements.md`. План на бриф ссылается, не повторяет.

## Context (from discovery)

- **Wire-protocol**: `crates/wiredesk-protocol/src/packet.rs` — `MAX_PAYLOAD = 512` (строка 16), `len: u16` в header даёт hard-ceiling 65535. Roundtrip-тесты на стр. 219–230.
- **Serial transport**: `crates/wiredesk-transport/src/serial.rs` — hardcoded'ный 1024-byte limit в `recv` (строка 88) + capacity hint 1024 в `try_clone` (строка 144). **Без bump'а здесь любой packet >~1024 bytes silently дискар'дится receiver'ом с `"frame too large"`** — критическая зависимость для P1.
- **`run_oneshot`**: `apps/wiredesk-term/src/main.rs::run_oneshot` (строки 376–542). State-machine `OneShotState::{AwaitingRemotePrompt, AwaitingSentinel}`. Buffer `full_log: String` копится по строкам. Timeout-path: `Ok(124)` с `eprintln!` без диагностики (строки 535–541).
- **Existing tests baseline**: `cargo test --workspace -- --test-threads=1` — 348 тестов зелёные (148 client + 97 host + 44 term + 59 protocol).
- **Branch**: `feat/wd-exec-fixes` создана от `master`. Бриф `docs/briefs/wd-exec-improvements.md` — uncommitted, идёт с веткой.

## Development Approach

- **testing approach**: Regular (test после кода)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes
- **CRITICAL: all tests must pass before starting next task**
- **CRITICAL: update this plan file when scope changes during implementation**
- run tests after each change — `cargo test --workspace -- --test-threads=1`
- maintain backward compatibility — bump MAX_PAYLOAD требует деплоя обеих сторон, это норма для serial-link single-user setup

## Testing Strategy

- **unit tests**: required for every task
- **e2e/live**: проект имеет live verification AC'ы (CH340 + Win11 host), не automated CI. Helper'ы остаются pure. Live verify — после всех task'ов в финальной фазе.
- **regression**: existing 348 тестов должны оставаться зелёными.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope
- keep plan in sync with actual work done

## Solution Overview

Один скоупный PR в `feat/wd-exec-fixes`. Две независимые задачи:
1. **P1 (packet.rs + serial.rs)**: одна константа bumped + matched bump в SerialTransport (frame limit + capacity hint) + два теста (overflow update + 4 KB roundtrip).
2. **P2 (main.rs::run_oneshot)**: pure-helper `format_timeout_diagnostic` + ~3 строки в timeout-path для eprintln + один консолидированный тест.

Никаких новых opcode'ов, никаких phase-based machinery, никаких новых dependency'ев.

## Technical Details

### P1 — MAX_PAYLOAD + frame limit bump

```rust
// crates/wiredesk-protocol/src/packet.rs:16
pub const MAX_PAYLOAD: usize = 4096;  // was 512
```

```rust
// crates/wiredesk-transport/src/serial.rs:88
if self.read_buf.len() > MAX_FRAME_SIZE { ... }  // MAX_FRAME_SIZE = 8192

// crates/wiredesk-transport/src/serial.rs:144
read_buf: Vec::with_capacity(MAX_FRAME_SIZE),
```

**Сторонние эффекты:**
- COBS framing add'ит ~1 byte per 254 → 4096 → ~4112 framed. ОК.
- Wire-time на 115200 baud (~11 KB/s): 4 KB ≈ 370 ms full-frame. ОК.
- ClipChunk остаётся payload = 256 bytes (это design decision для прогресс-бара, не лимит протокола) — не трогаем.
- Frame limit 8192 даёт запас сверх MAX_PAYLOAD (4096 + header 8 + CRC 2 + COBS overhead ~16 ≈ 4122 — много места).
- `cobs.rs:155` использует hardcoded `512` (data sample, не лимит) — игнорировать при grep'е.

### P2 — last-buffer dump

Pure-helper в `apps/wiredesk-term/src/main.rs`:

```rust
fn format_timeout_diagnostic(buf: &str, timeout_secs: u64) -> String {
    let bytes = buf.as_bytes();
    let start = bytes.len().saturating_sub(256);
    let tail = String::from_utf8_lossy(&bytes[start..]);
    format!(
        "wiredesk-term: --exec timeout after {timeout_secs}s (no sentinel from host)\nlast bytes received: {tail:?}"
    )
}
```

В `run_oneshot::None` arm (строка ~535):

```rust
None => {
    eprintln!("{}", format_timeout_diagnostic(&full_log, timeout_secs));
    Ok(124)
}
```

**Дизайн-решения:**
- `{tail:?}` (Debug-format) экранирует ANSI escape-codes и `\r\n` в читаемые `\\x1b[1m`, `\\r\\n` — для diagnostic dump'а это лучше raw'а (escape-sequences в pipe'нутом stderr иначе corrupt'ят downstream-парсеры).
- 256 bytes — компромисс между «увидеть промпт + последний line» и «не залить терминал».
- UTF-8 boundary safety: `String::from_utf8_lossy` всегда safe — broken multi-byte chars становятся `?` replacement char. Косметически чуть страдает на прерванной кириллице, но diagnostic dump'у это приемлемо. Один регрессионный тест gerade'ит этот edge case.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): bump константы, dump-helper + один тест, bump frame limit в transport.
- **Post-Completion** (no checkboxes): live verify AC2 + AC4 на CH340 + Win11 (не automated).

## Implementation Steps

### Task 1: Bump MAX_PAYLOAD + SerialTransport frame limit

**Files:**
- Modify: `crates/wiredesk-protocol/src/packet.rs`
- Modify: `crates/wiredesk-transport/src/serial.rs`

- [x] grep `crates/ apps/` на hardcoded `512` — найдено только `cobs.rs:155` (data sample) и тест-fixtures в clipboard.rs (test data sizes); все игнорируем
- [x] изменить `pub const MAX_PAYLOAD: usize = 512;` → `4096` в `crates/wiredesk-protocol/src/packet.rs:16`
- [x] existing `over_max_payload` test использует `vec![0xAA; MAX_PAYLOAD]` через константу — auto-adapts, изменения не нужны
- [x] добавить новую константу `MAX_FRAME_SIZE: usize = 8192` в `crates/wiredesk-transport/src/serial.rs`
- [x] заменить hardcoded'ный `1024` в `serial.rs:88` (frame discard threshold) на `MAX_FRAME_SIZE`
- [x] заменить hardcoded'ный `1024` в `serial.rs:144` (`Vec::with_capacity` hint в `try_clone`) на `MAX_FRAME_SIZE`
- [x] ➕ дополнительно: `serial.rs:43` (`Vec::with_capacity(1024)` в `open`) тоже заменено на `MAX_FRAME_SIZE` — пропущено в plan'е, но обнаружено grep'ом
- [x] добавить тест `roundtrip_4kb_shell_input` в `packet.rs::tests` — `vec![0xAB; 4000]` через ShellInput
- [x] run `cargo test --workspace -- --test-threads=1` — 353 passed (было 352 + 1 new), все зелёные
- [x] run `cargo clippy --workspace -- -D warnings` — чисто

### Task 2: format_timeout_diagnostic helper + dump on timeout

**Files:**
- Modify: `apps/wiredesk-term/src/main.rs`

- [x] добавлен pure-helper `fn format_timeout_diagnostic(buf: &str, timeout_secs: u64) -> String` перед `clean_stdout`
- [x] в `run_oneshot::None` arm заменено на `eprintln!("{}", format_timeout_diagnostic(&full_log, timeout_secs));`
- [x] добавлен unit-test `format_timeout_diagnostic_truncates_and_handles_utf8` — три assertions: 1024-byte ASCII → last 256, empty buffer → no panic, mid-cyrillic boundary → lossy handles
- [x] existing `run_oneshot_timeout_returns_124` не нужно обновлять — он verify'ит exit code 124 (regression-trap), форматирование stderr не testable из cargo test без `assert_cmd`
- [x] run `cargo test --workspace -- --test-threads=1` — 354 passed (+1 new)
- [x] run `cargo clippy --workspace -- -D warnings` — чисто

### Task 3: Verify acceptance criteria

- [x] AC1 (P1 unit): `roundtrip_4kb_shell_input` проходит
- [x] AC3 (P2 unit): `format_timeout_diagnostic_truncates_and_handles_utf8` проходит
- [x] AC6 (regression): 354 passed (148 client + 97 host + 60 protocol + 45 term + 4 transport), 0 failed
- [x] AC7 (lint): `cargo clippy --workspace -- -D warnings` чисто
- [ ] AC2 (P1 live, после merge на master): отмечается в Post-Completion — требует Win11 host
- [ ] AC4 (P2 live): отмечается в Post-Completion — требует ssh chain

### Task 4: [Final] Update documentation

- [x] update `docs/wd-exec-usage.md` — добавлена строка про 4 KB лимит в «Что нужно знать ДО запуска» + примечание про last-bytes dump в Exit codes table
- [x] update `CLAUDE.md` — статус-параграф: «MAX_PAYLOAD = 4096» + matched `MAX_FRAME_SIZE = 8192` упомянут, test counts обновлены 348→354, добавлена строка про `format_timeout_diagnostic` рядом с `wd --exec` описанием
- [x] move plan to `docs/plans/completed/` через `git mv`

## Post-Completion

*Items requiring manual intervention or external systems — no checkboxes, informational only*

**Live verification (требуется Win11 host + CH340):**
- AC2: `wd --exec --ssh prod-mup "$(python3 -c 'print(\"echo \" + \"a\" * 700)')"` → проходит без `payload too large` (раньше падал на 619+).
- AC4: `wd --exec --ssh nonexistent-host "ping"` (или любой ssh chain который зависнет на handshake'е) → exit 124 после timeout. Stderr содержит `last bytes received: "..."` с фрагментом MOTD/error.
- AC2-equivalent: реальный компактный ES `_search` через `wd --exec --ssh prod-mup`, который раньше падал на 619 byte → теперь проходит.

**Deployment notes:**
- MAX_PAYLOAD bump затрагивает обе стороны (host + client). Старый host (≤512) при получении 4 KB пакета отвергнет с `payload too large` ошибкой → recoverable, не data corruption. Старый serial.rs reader (≤1024) silently дискар'дит frame с `frame too large`. Деплой обеих сторон одновременно — стандартный workflow для serial-link single-user setup.
- Никаких изменений в `~/.claude` configs, в config.toml на host/client сторонах, в registry autostart'е.

**Why no Task для B2 в этом плане:**
- Existing test `run_oneshot_propagates_nonzero_exit` (apps/wiredesk-term/src/main.rs:1528) покрывает B2 kernel — sentinel matching с ненулевым exit code прохрдит state-machine идентично для любого числа. Литерал 7 vs 1 — same path.
- Existing static test 1170–1175 проверяет что PS payload содержит `try { } catch { $LASTEXITCODE=1 }` — payload format coverage есть.
- Дополнительный test был бы copy-paste'ом 1528 с другой константой → no value, plain redundancy. YAGNI.
