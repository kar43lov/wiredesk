# `wd --exec --compress` — implementation plan

## Overview

Опциональный флаг `--compress` для `wd --exec`, который сжимает stdout команды на host'е (gzip+base64) и разворачивает обратно на client'е. Для крупных текстовых выводов (логи, ES-ответы, дампы конфигов) даёт кратное ускорение по wire (5–10×) без изменения base-канала. Поддерживаются **обе path'и**: bash через `--ssh` и PowerShell на host'е напрямую.

Полный спек — в [`docs/briefs/wd-exec-compression.md`](../briefs/wd-exec-compression.md).

**Branch:** `feat/wd-exec-compress`. Master остаётся стабильным; merge только после live-тестов трёх размеров (1 KB, 50 KB, 500 KB) на обеих путях + кириллица + IPC bridge.

## Context (from discovery)

- **Files involved:**
  - `crates/wiredesk-exec-core/src/helpers.rs` — `format_command`, `clean_stdout`, `parse_sentinel`, `parse_ready`, `strip_ansi`, `is_powershell_prompt`, `is_remote_prompt` + 31 unit-тест. Сюда же добавляются `format_compressed_command` и `decode_compressed_stream`.
  - `crates/wiredesk-exec-core/src/runner.rs` — `run_oneshot<T, F>(transport, cmd, ssh, timeout_secs, on_chunk)`. Phase-tracker `Mute → Streaming`. Нужно добавить `compress: bool` в сигнатуру + branch который буферит base64 в Streaming-фазе и декодирует на Done (compressed-mode не streaming).
  - `crates/wiredesk-exec-core/src/types.rs` — `ShellKind`, `OneShotState`, `ExecEvent`, `ExecError`. Добавить `ExecError::CompressionFailed(String)`.
  - `crates/wiredesk-exec-core/src/ipc.rs` — `IpcRequest { cmd, ssh, timeout_secs }`. Добавить `compress: bool` поле (bincode positional → field в конце, single-binary deployment без mismatch-окна).
  - `crates/wiredesk-exec-core/Cargo.toml` — добавить `flate2 = "1"` и `base64 = "0.22"`.
  - `apps/wiredesk-term/src/main.rs` — clap `--compress` флаг, проброс в `run_oneshot` через новый параметр.
  - `apps/wiredesk-client/src/ipc.rs` — `handle_connection` достаёт `compress` из `IpcRequest`, передаёт в runner.
  - `docs/wd-exec-usage.md` — пример с `--compress` в раздел «Examples».
- **Patterns observed:**
  - Pure helpers с table-driven unit-тестами рядом — `#[cfg(test)] mod tests` в каждом файле.
  - `ShellKind { Bash, PowerShell }` — discriminator для двух обёрток в `format_command`.
  - Sentinel `__WD_DONE_<uuid>__<exit>` парсится через `rfind` + leading-digit-run (memory `feedback_sentinel_anchor_anywhere.md`).
  - bincode `IpcRequest`/`IpcResponse` — frame codec через length-prefix u32 BE, 16 MB cap.
- **Architectural note:** В compress-режиме streaming-семантика runner'а нарушается (callback зовётся **один раз** в самом конце с decoded-bytes), потому что base64-блок надо собрать целиком до decode. Это intentional trade-off opt-in флага: latency vs throughput. На малых выводах overhead ≤0.5 сек (AC7).

## Development Approach

- **testing approach:** Regular (code first, then tests) — соответствует проекту.
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** — без исключений
- **CRITICAL: all tests must pass before starting next task** — `cargo test --workspace -- --test-threads=1` зелёный (host parallel-flake обходим через `--test-threads=1`, см. `feedback_macos_test_thread_flake.md`)
- **CRITICAL: update this plan file when scope changes during implementation**
- maintain backward compatibility — без флага `--compress` поведение wd 1-в-1 как до (AC8 — 395 существующих тестов проходят)

## Testing Strategy

- **unit tests:** required для каждой задачи. Pure helpers — table-driven, edge cases (валид, невалид b64, невалид gzip, empty, CRLF, single-line/multi-line).
- **integration tests:** через `MockExecTransport` — host имитирует `__WD_READY_xxx__\n<b64-of-gzipped>\n__WD_DONE_xxx__0\n`, runner отдаёт decompressed bytes в callback.
- **e2e tests:** проект не имеет UI-based e2e (Playwright/Cypress нет). Live-проверка вручную на реальной паре Mac+Win по сценариям AC1-AC10.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope

## Solution Overview

**Wire-format (bash, --ssh path):**
```
__WD_READY_<uuid>__
<base64_of_gzipped_stdout — multi-line, 76 chars per line>
__WD_DONE_<uuid>__<rc>
```
где обёртка:
```bash
echo __WD_READY_<uuid>__
{ <cmd>; } 2>&1 | gzip -c | base64; rc=${PIPESTATUS[0]}
echo
echo "__WD_DONE_<uuid>__$rc"
```
- `base64` без `-w0` (76-char wrapping default) — безопаснее в `ssh -tt` PTY, client'у несложно `replace("\n", "")` перед decode
- ⚠️ `${PIPESTATUS[0]}` — bash-only. `rc=$?` ВНУТРИ `{ <cmd>; rc=$?; } | gzip` **теряется** (subshell). У нас `bash -c` уже в --ssh пути — OK
- `2>&1` сливает stderr в stdout — единый поток для compress'а
- explicit `echo` после `base64` — гарантирует `\n` перед sentinel (чтобы он не приклеился к последней base64-строке)

**Wire-format (PowerShell, host-direct):**
```
__WD_READY_<uuid>__
<long_base64_of_gzipped_stdout>
__WD_DONE_<uuid>__<rc>
```
где обёртка:
```powershell
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Write-Output "__WD_READY_<uuid>__"
$LASTEXITCODE = 0; $ErrorActionPreference = 'Stop'
try { $out = & { <cmd> } 2>&1 | Out-String }
catch { $out = $_.ToString(); $LASTEXITCODE = 1 }
$rc = $LASTEXITCODE
$ms = New-Object System.IO.MemoryStream
$gz = New-Object System.IO.Compression.GZipStream($ms, [System.IO.Compression.CompressionMode]::Compress)
$bytes = [Text.Encoding]::UTF8.GetBytes($out)
$gz.Write($bytes, 0, $bytes.Length); $gz.Close()
Write-Output ([Convert]::ToBase64String($ms.ToArray()))
Write-Output "__WD_DONE_<uuid>__$rc"
```
- `[Console]::OutputEncoding = UTF8` **критично** — иначе кириллица через cp1251/cp866 default-codepage развалится (AC4)
- `$LASTEXITCODE = 0` pre-init — cmdlet'ы не сетят, остаётся 0 если catch не сработал
- `try`/`catch` с `$LASTEXITCODE = 1` — для PS terminating errors

**Runner branch:**
- Без `compress` — streaming как сейчас (callback per line).
- С `compress` — accumulate в local `String` в `Streaming` фазе (base64 — ASCII subset, `String` чище для `decode_compressed_stream(&str)` API). На sentinel: `decode_compressed_stream` → callback **один раз** с decoded bytes.
- `parse_sentinel` работает в обоих режимах одинаково (base64 alphabet `A-Za-z0-9+/=` не содержит `_` — sentinel находится надёжно через `rfind`).
- ⚠️ Pre-prefix unterminated-output recovery (`runner.rs:162` — pre-sentinel chunks без `\n`) при `compress=true` тоже идёт в buffer, не в callback — иначе теряется кусок base64-payload и decode упадёт.
- ⚠️ На timeout с partial buffer'ом (host оборвался mid-base64) — `Err(Timeout(_))` возвращается **до** decode-попытки, partial buffer не decode'ится (это не данные, а fragment).

**Sequence (компилирует все слои):**
1. `wd --exec --compress "..."` или `wd --exec --compress --ssh prod-mup "..."` пользователем.
2. `wiredesk-term` парсит clap, формирует `RunRequest { cmd, ssh, compress: true, timeout: 90 }`.
3. Если IPC-socket доступен → `IpcRequest { cmd, ssh, timeout_secs, compress: true }` через UnixStream к GUI handler'у.
4. GUI handler / direct-term зовёт `format_compressed_command(cmd, uuid, kind)` → отсылает на host через `Message::ShellInput`.
5. Host выполняет PS/bash обёртку, выдаёт base64 stdout + sentinel.
6. Runner буферит base64-блок до sentinel, после — `decode_compressed_stream` → callback с decoded bytes → stdout пользователю.

## Technical Details

### Новые типы и сигнатуры

**`crates/wiredesk-exec-core/src/types.rs`:**
```rust
pub enum ExecError {
    // existing variants...
    CompressionFailed(String),
}
```

**`crates/wiredesk-exec-core/src/helpers.rs`:**
```rust
pub fn format_compressed_command(cmd: &str, uuid: &Uuid, kind: ShellKind) -> String;
pub fn decode_compressed_stream(b64_with_whitespace: &str) -> Result<Vec<u8>, ExecError>;
```

**`crates/wiredesk-exec-core/src/runner.rs`:**
```rust
pub fn run_oneshot<T, F>(
    transport: &mut T,
    cmd: &str,
    ssh: Option<&str>,
    timeout_secs: u64,
    compress: bool,    // new
    mut on_chunk: F,
) -> Result<i32, ExecError>
```

**`crates/wiredesk-exec-core/src/ipc.rs`:**
```rust
pub struct IpcRequest {
    pub cmd: String,
    pub ssh: Option<String>,
    pub timeout_secs: u64,
    pub compress: bool,    // new — appended at end
}
```

### Зависимости

`crates/wiredesk-exec-core/Cargo.toml`:
```toml
flate2 = "1"
base64 = "0.22"
```
Workspace `Cargo.toml` — версии добавляются в `[workspace.dependencies]` либо direct в crate.

### Backward compatibility — IPC

`IpcRequest::compress` — новое поле. bincode positional → старый GUI не сможет десериализовать новый запрос. **Acceptable**: GUI и `wd` собираются вместе из workspace, deploy через rebuild — нет mismatch-окна. Single-user Mac, не клиент-серверная разнесённая система.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): код, тесты, доки в этом репо.
- **Post-Completion** (no checkboxes): live-тесты на real hardware (Mac+Win11 пара), кириллица в PS-host, замеры before/after, push branch + PR.

## Implementation Steps

### Task 1: Добавить `flate2` + `base64` deps + `ExecError::CompressionFailed`

**Files:**
- Modify: `Cargo.toml` (workspace root, `[workspace.dependencies]` секция если она есть)
- Modify: `crates/wiredesk-exec-core/Cargo.toml`
- Modify: `crates/wiredesk-exec-core/src/types.rs`

- [ ] добавить `flate2 = "1"` и `base64 = "0.22"` в `crates/wiredesk-exec-core/Cargo.toml` (через workspace deps если используется паттерн)
- [ ] добавить `CompressionFailed(String)` variant в `ExecError` с `#[error("compression failed: {0}")]`
- [ ] прогнать `cargo clippy -p wiredesk-exec-core -- -D warnings` — без warning'ов
- [ ] добавить unit-тест `compression_failed_error_format` в `types.rs` тестах: `format!("{}", ExecError::CompressionFailed("bad b64".into()))` → ожидаемая строка
- [ ] run `cargo test -p wiredesk-exec-core` — пройдёт перед Task 2

### Task 2: `decode_compressed_stream` pure helper + table-driven tests

**Files:**
- Modify: `crates/wiredesk-exec-core/src/helpers.rs`

- [ ] реализовать `pub fn decode_compressed_stream(input: &str) -> Result<Vec<u8>, ExecError>`:
  - strip whitespace (`\r`, `\n`, ` `, `\t`) из input
  - `base64::engine::general_purpose::STANDARD.decode(...)` → `ExecError::CompressionFailed` на ошибку
  - `flate2::read::GzDecoder` + `read_to_end` → `Vec<u8>` → `ExecError::CompressionFailed` на ошибку
- [ ] добавить test-helper `make_compressed_b64(payload: &[u8]) -> String` в `#[cfg(test)] mod tests` который генерирует gzip+base64 на лету (через `GzEncoder` + `STANDARD.encode`) — устраняет дрифт между fixture и реальным wire-форматом
- [ ] добавить unit-тесты (table-driven, fixture'ы через `make_compressed_b64`):
  - `decode_valid_singleline` — `b"hello world"` round-trip → bytes match
  - `decode_valid_multiline_crlf` — payload с `\r\n` обёрткой каждые 76 символов → identical result
  - `decode_invalid_base64` — мусор `"!!!not base64!!!"` → `Err(CompressionFailed)`
  - `decode_valid_b64_invalid_gzip` — корректный b64 от `b"hello"` (не gzip) → `Err(CompressionFailed)`
  - `decode_empty_string` → `Err(CompressionFailed)`
  - `decode_cyrillic_payload` — UTF-8 байты «Привет мир» round-trip → restored bytes match
- [ ] run `cargo test -p wiredesk-exec-core --lib helpers::tests::decode` — passes

### Task 3: `format_compressed_command` для bash + PowerShell

**Files:**
- Modify: `crates/wiredesk-exec-core/src/helpers.rs`

- [ ] реализовать `pub fn format_compressed_command(cmd: &str, uuid: &Uuid, kind: ShellKind) -> String`:
  - `ShellKind::Bash` → bash-обёртка с `__WD_READY_`, `gzip -c | base64`, `${PIPESTATUS[0]}`, `__WD_DONE_`
  - `ShellKind::PowerShell` → PS-обёртка с `[Console]::OutputEncoding = UTF8`, `try/catch`, `GZipStream`, `[Convert]::ToBase64String`
- [ ] добавить unit-тесты:
  - `format_compressed_bash_shape` — output содержит `__WD_READY_<uuid>__`, `gzip -c | base64`, `${PIPESTATUS[0]}`, `__WD_DONE_<uuid>__`
  - `format_compressed_powershell_shape` — содержит `[Console]::OutputEncoding`, `GZipStream`, `[Convert]::ToBase64String`, `__WD_DONE_<uuid>__$rc`
  - `format_compressed_bash_escapes_quotes` — cmd с одинарными кавычками не ломает обёртку
  - `format_compressed_powershell_preserves_cmd_verbatim` — cmd попадает в `& { <cmd> }` блок
  - `format_compressed_uuid_consistent` — оба маркера используют одинаковый uuid
- [ ] run `cargo test -p wiredesk-exec-core --lib helpers::tests::format_compressed` — passes

### Task 4: Интегрировать `compress` в runner — buffer-then-decode branch

**Files:**
- Modify: `crates/wiredesk-exec-core/src/runner.rs`
- Modify: `apps/wiredesk-term/src/main.rs` (call sites + tests)
- Modify: `apps/wiredesk-client/src/ipc.rs` (call site + tests)

- [ ] добавить параметр `compress: bool` в сигнатуру `run_oneshot` (после `timeout_secs`, перед `on_chunk`)
- [ ] если `compress=true` — использовать `format_compressed_command` вместо `format_command` (выбор шаблона остаётся в helpers через `ShellKind`)
- [ ] **buffer-then-decode logic в `Streaming` фазе**:
  - при `compress=true` — НЕ вызывать `on_chunk` per-line; accumulate каждую completed line в local `String` буфер (base64 — ASCII subset, `String` чище чем `Vec<u8>` для последующего `decode_compressed_stream(&str)`)
  - pre-prefix recovery (unterminated output, текущий runner.rs:162) при `compress=true` тоже идёт в buffer, не в callback
  - на sentinel-detect (runner.rs:158-169) при `compress=true`:
    - `decode_compressed_stream(&buffer)` → `Vec<u8>`
    - один callback `on_chunk(&decoded_bytes)`
    - decode error → return `Err(ExecError::CompressionFailed(...))` (не теряем output молча)
  - при `compress=false` — текущее поведение байт-в-байт, callback per-line как сейчас
- [ ] **обновить ВСЕ call-site'ы `run_oneshot`** (8 мест):
  - prod: `apps/wiredesk-term/src/main.rs:422` (передать `args.compress` — будет в Task 6)
  - prod: `apps/wiredesk-client/src/ipc.rs:270` (передать `request.compress` — будет в Task 7)
  - in-crate tests: `crates/wiredesk-exec-core/src/runner.rs:285+` (5 тестов: happy_path_powershell, happy_path_ssh, timeout_returns_124, propagates_nonzero_exit, handles_unterminated_output_with_ansi_tail) — добавить `false`
  - term tests: `apps/wiredesk-term/src/main.rs:994-1099` (~6 интеграционных) — добавить `false`
  - client ipc tests: `apps/wiredesk-client/src/ipc.rs:~1223` (1 тест) — добавить `false`
- [ ] добавить integration-тест в `runner.rs` через `MockExecTransport`:
  - заскриптовать events: `Output("__WD_READY_xxx__\n")`, `Output(<b64-of-gzipped-hello-world>)` (через `make_compressed_b64` helper), `Output("\n__WD_DONE_xxx__0\n")`, `Exit(0)`
  - вызвать `run_oneshot(..., compress=true, on_chunk)` → callback зовётся **один раз** с `b"hello world"`, exit=0
- [ ] добавить тест `runner_compress_invalid_b64_returns_compression_failed` — host даёт `"!!!garbage!!!"` между READY и DONE → `Err(CompressionFailed)`
- [ ] добавить тест `runner_compress_timeout_with_partial_buffer_returns_timeout_not_compression_failed` — host шлёт READY + 5 base64-строк + idle навечно → `Err(Timeout(_))` (НЕ `CompressionFailed` на partial gzip)
- [ ] добавить тест `runner_compress_pre_prefix_unterminated_recovery_buffered` — pre-sentinel output без `\n` (case как `parse_sentinel_after_unterminated_output`) при `compress=true` идёт в buffer, decode'ится корректно
- [ ] run `cargo test -p wiredesk-exec-core --lib runner` — passes
- [ ] run `cargo test --workspace -- --test-threads=1` — все 395+ тестов passes (ничего не сломали в legacy callers)

### Task 5: Расширить `IpcRequest` полем `compress: bool`

**Files:**
- Modify: `crates/wiredesk-exec-core/src/ipc.rs`

- [ ] добавить `#[serde(default)] pub compress: bool` поле в `IpcRequest` (в конце struct'а — bincode positional, `serde(default)` даёт толерантность если bincode-конфигурация поддерживает skip-on-eof)
- [ ] обновить все ручные конструкторы `IpcRequest { ... }` в workspace (`grep -rn 'IpcRequest {'`) — добавить `compress: false` для default cases
- [ ] добавить unit-тест `ipc_request_roundtrip_with_compress` — encode→decode → field preserved (true и false)
- [ ] добавить unit-тест `ipc_request_old_payload_compatibility` — попробовать deserialize короткий payload без `compress` поля; если bincode-конфиг не поддерживает default — pass'нуть тест с явным comment'ом о single-binary deployment (нет mismatch-окна между GUI и `wd`-binary в этом проекте). Документировать ограничение в comment рядом с полем
- [ ] добавить unit-тест `ipc_request_payload_size_under_cap` — с `compress: true` payload остаётся в 16 MB лимите
- [ ] run `cargo test -p wiredesk-exec-core --lib ipc` — passes

### Task 6: CLI `--compress` флаг в `wiredesk-term`

**Files:**
- Modify: `apps/wiredesk-term/src/main.rs`

- [ ] добавить `#[arg(long)] compress: bool` в clap-структуру (рядом с существующими `ssh`, `timeout`)
- [ ] help-text: `Compress stdout via gzip+base64 (5-10x speedup for large text output)`
- [ ] пробрасить `args.compress` в:
  - direct-serial path (вызов `run_oneshot` напрямую)
  - IPC path (`IpcRequest { ..., compress: args.compress }`)
- [ ] добавить один integration-style smoke-test в `apps/wiredesk-term/src/main.rs` test mod: `args_compress_propagates_to_ipc_request` — собрать `IpcRequest` из `args` с `--compress` → field == true. Чистого clap-parsing теста не делаем (clap сам тестирует свой parsing — fluff)
- [ ] run `cargo test -p wiredesk-term` — passes

### Task 7: IPC handler в `wiredesk-client` — пробросить `compress` в runner

**Files:**
- Modify: `apps/wiredesk-client/src/ipc.rs`

- [ ] в `handle_connection` достать `request.compress`, передать в `run_oneshot(..., compress=request.compress, ...)`
- [ ] добавить smoke-тест `ipc_handler_extracts_compress_field` — собрать `IpcRequest { compress: true, ... }` через bincode encode → decode в handler-fixture → assert что field правильно дочитан и доступен в local var. Round-trip уже покрыт Task 5 (`ipc_request_roundtrip_with_compress`), здесь только handler-side extraction
- [ ] run `cargo test -p wiredesk-client` — passes
- [ ] run `cargo test --workspace -- --test-threads=1` — все 395+ тестов зелёные (новые добавлены, старые не сломаны)

### Task 8: Verify acceptance criteria

- [ ] AC8 + workspace gate: `cargo test --workspace -- --test-threads=1` — все тесты passes (legacy + новые из Tasks 1-7)
- [ ] `cargo clippy --workspace -- -D warnings` — чистый
- [ ] `cargo build --release --workspace` — собирается
- [ ] live-тесты (AC1, AC2, AC3, AC4, AC5, AC6, AC7, AC9, AC10) — выполняются вручную на real hardware, см. Post-Completion ниже

### Task 9: Update documentation

**Files:**
- Modify: `docs/wd-exec-usage.md`
- Modify: `CLAUDE.md`
- Modify: `README.md`
- Move: `docs/plans/20260505-wd-exec-compress.md` → `docs/plans/completed/`

- [ ] добавить раздел `### Compression` в `docs/wd-exec-usage.md` с примерами bash и PS + когда включать (большой текст — да, бинарь — нет)
- [ ] обновить TL;DR: добавить `wd --exec --compress "<cmd>"` в краткий список
- [ ] обновить exit-codes таблицу если ввели mapping `CompressionFailed → 125`; иначе skip
- [ ] обновить раздел «Под капотом» с описанием base64+gzip wire-format
- [ ] в `CLAUDE.md` упомянуть `--compress` в `wd --exec` секции — одна строка
- [ ] в `README.md` — упомянуть `--compress` в фиче-листе про `wd --exec`
- [ ] move plan: `mkdir -p docs/plans/completed && mv docs/plans/20260505-wd-exec-compress.md docs/plans/completed/`

> Auto-memory (`MEMORY.md` в `~/.claude/projects/...`) — пользовательские файлы вне репо. Шаг руководящий, не automatable: упомянуть в PR description что `project_wd_exec_compression.md` стоит пометить shipped после merge.

## Post-Completion

*Items requiring manual intervention or external systems — no checkboxes, informational only.*

**Live-тесты (final gate перед merge в master):**

Все тесты на real hardware (Mac + Win11 host через serial). Записать timing в PR description.

1. **bash path baseline + compress:**
   - `time wd --exec --ssh prod-mup "docker logs --tail 5000 mup.srv.main 2>&1"` (без compress)
   - `time wd --exec --compress --ssh prod-mup "docker logs --tail 5000 mup.srv.main 2>&1"` (с compress)
   - **Expected:** второй <30 сек на ~200 KB output (AC1), первый ~18 сек или timeout
   - **Verify:** stdout байт-в-байт идентичен (AC2) — `diff` двух сохранённых выводов

2. **bash path exit-code propagation (AC3):**
   - `wd --exec --compress --ssh prod-mup 'false'` → exit 1
   - `wd --exec --compress --ssh prod-mup 'exit 42'` → exit 42 (если SSH limitation позволяет — пре-existing baseline)

3. **PS path baseline + compress:**
   - `time wd --exec "Get-EventLog -LogName System -Newest 5000"` (или аналогичный 100+ KB вывод)
   - `time wd --exec --compress "Get-EventLog -LogName System -Newest 5000"`
   - **Expected:** compress быстрее, stdout идентичен

4. **Кириллица PS path (AC4 — главный risk):**
   - на Win11 host'е создать `C:\test\русский.txt` с UTF-8 содержимым «Привет мир\nТестовый файл»
   - `wd --exec --compress "Get-Content C:\test\русский.txt"` → текст идентичен файлу
   - если encoding разваливается → debug `[Console]::OutputEncoding`, `[Text.Encoding]::UTF8.GetBytes` flow

5. **PS-host без --ssh (AC5):**
   - `wd --exec --compress "Get-ChildItem C:\Users -Recurse -ErrorAction SilentlyContinue | Select-Object -First 5000 | Out-String"` — отрабатывает быстрее non-compress версии

6. **Binary output (AC6):**
   - `wd --exec --compress --ssh prod-mup 'cat /usr/bin/ls | base64'` — не падает (ratio ~1×, но завершается)

7. **Малый output overhead (AC7):**
   - `time wd --exec --compress --ssh prod-mup "echo alive"` — overhead ≤0.5 сек по сравнению с non-compress

8. **IPC bridge (AC9):**
   - запустить `WireDesk.app`
   - `wd --exec --compress --ssh prod-mup "docker logs --tail 1000 ..."` — отрабатывает идентично direct-serial
   - проверить что GUI clipboard sync продолжает работать параллельно

9. **Smoke 1 KB / 50 KB / 500 KB:**
   - `wd --exec --compress --ssh prod 'head -c 1024 /dev/urandom | base64'` (1 KB)
   - `wd --exec --compress --ssh prod 'head -c 50000 /dev/urandom | base64'` (50 KB)
   - `wd --exec --compress --ssh prod 'head -c 500000 /dev/urandom | base64'` (500 KB)
   - все три отрабатывают, exit 0, output корректный

**Branch + PR workflow:**

- `git checkout -b feat/wd-exec-compress` (если ещё не создана)
- commit'ить incrementally per Task
- после live-тестов и зелёного `cargo test --workspace -- --test-threads=1`:
  - `git push -u origin feat/wd-exec-compress` (только когда хочется обновлять host — host-side не меняется в этой задаче, скорее всего push только для PR)
  - PR с before/after timings из live-тестов
  - merge в master squash'ем после approval
