# Бриф: `wd --exec --compress` — gzip+base64 для большого текстового вывода

**Цель:** Опциональный флаг `--compress` для `wd --exec`, который сжимает stdout команды на host'е и разворачивает обратно на client'е. Для крупных текстовых выводов (логи, ES-ответы, дампы конфигов) даёт кратное ускорение по wire (5-10×) без изменения base-канала.

**Scope (после брейншторма 2026-05-05):** Поддерживаются **обе path'и** — bash через `--ssh` и PowerShell на host'е напрямую. Изначально бриф ограничивал MVP только `--ssh` режимом, но live-практика показала, что значительная часть тяжёлых команд (логи, дампы конфигов с host'а) идёт без --ssh, поэтому PS-side вошёл в MVP.

**Branch:** `feat/wd-exec-compress`. Master остаётся стабильным; merge только после live-тестов трёх размеров (1 KB, 50 KB, 500 KB) на обоих путях.

**Контекст:** `wd --exec` сейчас гоняет stdout в чистом виде через 115200 baud (~11 KB/s). На реальных prod-задачах часто нужны выводы 50–500 KB:
- `docker logs --tail 1000` на болтливом ASP.NET-контейнере → ~200 KB.
- ES `_search` с stack-trace'ами за 2 часа → ~100 KB.
- Полный `appsettings.json` из контейнера → 5–20 KB (мелочи, без compression ок).

Большие выводы либо упираются в `--timeout`, либо тянутся минутами. На текстовых данных gzip даёт ratio 5–10× → fold throughput до 50–110 KB/s effective.

**Почему opt-in flag, а не auto:** binary output (уже сжатый файл, изображение) от gzip не выигрывает или становится хуже. Auto-detect по entropy дороже чем дать пользователю явный выбор.

## Что делает

```bash
wd --exec --compress --ssh prod-mup "docker logs --tail 1000 mup.srv.main.NNNNNN 2>&1"
```

С точки зрения вызывающего — то же что и без `--compress`: `stdout` чистый текст, `exit-code` пробрасывается, sentinel-парсинг работает как обычно. Compression — internal.

## Реализация

### Host-side (внутри payload отправляемой команды)

`wd --exec` уже формирует bash-sandwich типа `<cmd>; echo __WD_DONE_<uuid>__$?`. С `--compress` обёртка усложняется:

```bash
echo __WD_READY_<uuid>__
{ <cmd>; } | gzip -c | base64 -w0
echo
echo "__WD_DONE_<uuid>__$?"
```

**Что важно:**
- `gzip -c` (stdout) + `base64` (default 76-char wrapping) → multi-line ASCII блок. `-w0` (single-line) **не используем** — `ssh -tt` PTY на остальной стороне может резать ультра-длинные строки на ~4-8 KB.
- Явный `echo` после `base64` чтобы блок завершился `\n` перед sentinel'ом (parser ищет sentinel через `rfind` по line boundary).
- `2>&1` сливает stderr в stdout — единый поток для compress'а (иначе stderr leak'ает на host'е без синхронизации).
- `$?` берётся от `<cmd>`, **не** от `gzip` или `base64`. `${PIPESTATUS[0]}` — bash-only, но у нас `bash -c` уже в --ssh пути:
  ```bash
  echo __WD_READY_<uuid>__
  { <cmd>; } 2>&1 | gzip -c | base64; rc=${PIPESTATUS[0]}
  echo
  echo "__WD_DONE_<uuid>__$rc"
  ```
- ⚠️ **Не использовать** `{ <cmd>; rc=$?; } | gzip` — `rc=$?` внутри subshell'а левой стороны pipe'а **теряется** в parent'е. Только `${PIPESTATUS[0]}`.
- На стороне PowerShell-шеф (`wd --exec` без `--ssh`): обёртка через `[System.IO.Compression.GZipStream]` + `[Convert]::ToBase64String`. Шаблон:
  ```powershell
  [Console]::OutputEncoding = [Text.Encoding]::UTF8
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
  **Encoding-крит:** обязательно явный `[Console]::OutputEncoding = UTF8` перед командой — иначе кириллица через `Out-String` развалится через cp1251/cp866 default-codepage. Live-тестить с командой типа `Get-Content C:\русский.txt`.

### Client-side (внутри `run_oneshot` в wiredesk-exec-core)

После того как поймали sentinel и собрали output:
- В `--compress` режиме output между `__WD_READY_` и `__WD_DONE_` — base64 (с `\n`-обёрткой каждые 76 символов либо single line — поддержать оба).
- Strip `\n`, `\r`, whitespace.
- `base64::decode` → `flate2::read::GzDecoder` → исходный stdout (UTF-8).
- Передать в `FnMut(&[u8])` callback runner'а.

**Локализация:** новые pure helpers в `wiredesk-exec-core/src/helpers.rs`:
- `format_compressed_command(cmd, uuid, kind: ShellKind) -> String` — выбирает bash или PS обёртку.
- `decode_compressed_stream(b64: &str) -> Result<Vec<u8>, ExecError>` — strip whitespace + decode.

Runner получает поле `compress: bool` через `RunOpts`. IPC: `IpcRequest` обзаводится `compress: bool`.

Зависимости: `base64` (новая, прямая dep), `flate2` (новая dep, ~50 KB compiled).

### CLI

```rust
#[arg(long)]
compress: bool,
```

Без значения. Если флаг есть — wd обёртывает команду gzip-sandwich'ем (bash для `--ssh`, PS для host-direct); если нет — текущее поведение.

## Acceptance criteria

| # | Критерий |
|---|---|
| **AC1** | `wd --exec --compress --ssh prod-mup "docker logs --tail 5000 mup.srv.main 2>&1"` отрабатывает за <30 сек на выводе ~200 KB (без compress — ~18 сек или timeout). |
| **AC2** | Stdout идентичен байт-в-байт варианту без `--compress` для того же вывода. Exit code тот же. |
| **AC3** | Команда роняется с non-zero exit — exit-code корректно пробрасывается (НЕ exit-code от gzip/base64/Out-String). Тесты: bash `false`, PS `Get-Item /nonexistent`, PS external `cmd /c "exit 42"`. |
| **AC4** | **Кириллица в output** не корраптится. Тест PS-host: `wd --exec --compress "Get-Content C:\test\русский.txt"` → текст идентичен файлу. Тест bash: `wd --exec --compress --ssh prod 'echo "Привет мир"'`. |
| **AC5** | **PS-host без `--ssh`** работает: `wd --exec --compress "Get-ChildItem C:\Users -Recurse -ErrorAction SilentlyContinue | Select-Object -First 5000"` — отрабатывает быстрее не-compress версии. |
| **AC6** | Binary output не падает (ratio ~1×, но команда отрабатывает): `wd --exec --compress --ssh prod 'cat /usr/bin/ls'`. |
| **AC7** | Малый output (<1 KB) — overhead ≤ +0.5 сек: `wd --exec --compress --ssh prod-mup "echo alive"` → exit 0. |
| **AC8** | Регрессия: 395 существующих тестов проходят без изменений. Без флага `--compress` поведение wd 1-в-1 как до. |
| **AC9** | Через **IPC bridge** (GUI запущен) `wd --exec --compress` работает идентично direct-serial mode. `IpcRequest::compress` пробрасывается. |
| **AC10** | `--timeout` корректно срабатывает с base64-payload: sentinel `__WD_DONE_` парсится **после** base64-блока. |
| **AC11** | Unit-тесты в `wiredesk-exec-core`:<br>- `decode_compressed_stream(&str) -> Result<Vec<u8>>` — table-driven (валидный, невалидный b64, невалидный gzip, empty, CRLF-обёрнутый). Минимум 6 cases.<br>- `format_compressed_command(cmd, uuid, ShellKind::Bash)` — корректный bash-sandwich с `${PIPESTATUS[0]}`.<br>- `format_compressed_command(cmd, uuid, ShellKind::PowerShell)` — корректный PS-sandwich с `Out-String` + GZipStream + `[Console]::OutputEncoding = UTF8`. |
| **AC12** | Integration с `MockExecTransport`: host имитирует `__WD_READY_xxx__\n<b64-of-gzipped>\n__WD_DONE_xxx__0\n`, runner отдаёт decompressed bytes в callback. |

## Тестирование

**Unit (в `wiredesk-exec-core`):**
- `decode_compressed_stream` — 6+ cases (валид, невалид b64, невалид gzip, empty, CRLF, single-line vs multi-line).
- `format_compressed_command` для **двух** `ShellKind` — bash + PowerShell (shape, uuid placement, encoding line).
- `parse_sentinel` / `clean_stdout` — должны работать с `--compress` payload (b64 string в середине).

**Integration:**
- `MockExecTransport` — три scenario: compressed bash payload, compressed PS payload, mixed output (READY → garbage → b64 → DONE).

**Live (final gate перед merge в master):**
- **bash path:** `docker logs --tail 5000` через prod-mup. Замер: time без compress vs с compress.
- **PS path:** `Get-EventLog -LogName System -Newest 5000` или `Get-Content C:\<big.log>`. Замер аналогичный.
- **Кириллица PS:** `Get-Content C:\test\русский.txt` (создать заранее).
- **Smoke на трёх размерах:** 1 KB, 50 KB, 500 KB через обе path'и.
- **IPC через GUI запущенным:** проверить `--compress` работает с `WireDesk.app` открытым.

## Не в scope

- **Streaming decompression** — собираем весь base64-блок целиком, потом декодируем. Для 500 KB → memory <4 MB, приемлемо. Streaming не нужен.
- **Auto-detect compression-worthiness** — не делаем, opt-in флаг проще и предсказуемее.
- **Compression на input (длинная команда)** — у нас 4 KB MAX_PAYLOAD достаточно. Сжимать stdin к команде не имеет смысла.
- **stderr separation** — отдельная задача, независимая.
- **Compression для clipboard sync** — тоже отдельная задача; здесь только `wd --exec`.

## Риски (из брейншторма 2026-05-05)

| Риск | Вероятность | Mitigation |
|---|---|---|
| **PS encoding (кириллица)** через `Out-String` → cp1251 default | medium-high | Явный `[Console]::OutputEncoding = [Text.Encoding]::UTF8` в обёртке + AC4 live-тест |
| **PS exit-code через scriptblock** — cmdlet'ы не сетят `$LASTEXITCODE` | medium | `$LASTEXITCODE = 0` pre-init + try/catch с явным `$LASTEXITCODE = 1` для catch path |
| **Bash $? через pipe** | low | `${PIPESTATUS[0]}` (bash-only, у нас уже `bash -c` в --ssh пути) |
| **base64 line-length в ssh -tt PTY** | low | Использовать `base64` без `-w0` (76-char wrapped) + client `replace("\n", "")`. Безопаснее single-line. |
| **Memory bloat на 500 KB output** | low | Out-String буферит ~2 MB peak, gzip+b64 ещё ~1.5 MB. ≤4 MB total — acceptable. |

## Сложность

**Medium.** ~150 строк нового кода (helpers в exec-core + CLI flag + PS-обёртка + IPC field). +2 deps (`flate2` + `base64`). ~80 строк тестов. Bash-sandwich уже отработан, PS-обёртка нова, но шаблон стандартный (System.IO.Compression).

**Главный риск — PS encoding на кириллице.** Тест с `Get-Content C:\test\русский.txt` обязателен перед merge.

## Связанные

- master `7f8dd92` (P1 + P2) — payload limit + timeout diagnostic.
- master `6c9b163` (sentinel-detection-ansi-tail fix) — расширил `clean_stdout`, нужно учесть совместимость с compressed-payload (sentinel парсится **до** decompression — сначала найти `__WD_DONE_`, потом decode b64-блок до него).
- memory клиента (агент): `feedback_wd_exec_practical_limits.md` — описывает 11 KB/s bandwidth-проблему как причину «маленьких кусков, не агрегатов».
- `docs/wd-exec-usage.md` — после реализации добавить пример с `--compress`.
