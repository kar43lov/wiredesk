# Бриф: `wd --exec` миссит sentinel когда тот приходит в одном chunk'е со Starship-prompt'ом / ANSI

**Цель:** починить sentinel-detection в `run_oneshot` так, чтобы строка `__WD_DONE_<uuid>__N` корректно матчилась даже когда вместе с ней в один `ShellOutput`-chunk прилетают байты Starship-prompt'а / ANSI escape codes (типичный случай для `--ssh ALIAS` через `ssh -tt`).

**Найден:** 04.05.26, при первом же тесте `wd --exec` после merge `feat/wd-exec-fixes` (master `7f8dd92`). Раньше не проявлялся потому что P2 (`format_timeout_diagnostic`) не существовал — без `last bytes received` в stderr баг выглядел как «команда висит до timeout», и казалось что виноват ES / медленный канал. P2 диагностика немедленно показала что sentinel уже в буфере.

## Симптом

`wd --exec --ssh prod-mup "<reasonable read-only command>"` отрабатывает на remote за ~4 сек, sentinel генерируется и попадает в client buffer, но `wd --exec` **не распознаёт его** и продолжает ждать до `--timeout` истечения. Exit code — 124 (timeout convention) хотя реальный exit команды был 0.

## Воспроизведение

Команда (один из подтверждённых случаев — ES `_search` с тремя aggregations, payload ~619 байт, помещается в новый `MAX_PAYLOAD = 4096`):

```bash
wd --exec --timeout 90 --ssh prod-mup "curl -s -XPOST 'http://10.24.200.219:9200/mup-srv-production-*/_search?size=0' -H 'Content-Type: application/json' -d '{\"query\":{\"bool\":{\"filter\":[{\"range\":{\"@timestamp\":{\"gte\":\"now-1h\"}}},{\"terms\":{\"level.keyword\":[\"Error\",\"Fatal\"]}}]}},\"aggs\":{\"by_min\":{\"date_histogram\":{\"field\":\"@timestamp\",\"fixed_interval\":\"5m\"}},\"by_class\":{\"terms\":{\"field\":\"exceptions.ClassName.keyword\",\"size\":5}},\"by_path\":{\"terms\":{\"field\":\"fields.RequestPath.keyword\",\"size\":5}}}}' | head -c 800"
```

Ожидаемое: stdout с ES JSON (≤ 800 байт после `head -c`), exit 0, latency ~5 сек.

Фактическое:
- stdout пуст
- stderr: `wiredesk-term: --exec timeout after 90s (no sentinel from host)\nlast bytes received: "..."` (см. ниже)
- exit 124
- latency 90 сек (упёрлись в `--timeout`)

`last bytes received` (P2 diagnostic) — обрезок в 256 байт, дословно:

```
"2c-4e46-bf2c-62566843b74d__0\r\n\u{1b}[1m\u{1b}[7m%\u{1b}[27m\u{1b}[1m\u{1b}[0m                                                                               \r \r\r\u{1b}[0m\u{1b}[27m\u{1b}[24m\u{1b}[J\u{1b}[1;33muser\u{1b}[0m in \u{1b}[1;2;32m🌐 cgu-knd-firecards-1\u{1b}[0m in \u{1b}[1;36m~\u{1b}[0m ⏱ 4s \r\n➜ \u{1b}[K\u{1b}[?1h\u{1b}=\u{1b}[?2004h"
```

Видно:
1. **Хвост `__WD_DONE_<uuid>__0\r\n`** — sentinel целиком в буфере (UUID обрезан 256-байт окном до окончания `2c-4e46-bf2c-62566843b74d`, но конец `__0\r\n` интактен).
2. **Сразу за `\r\n` начинаются ANSI escape codes** Starship-prompt'а (`\u{1b}[1m\u{1b}[7m%`...).
3. `⏱ 4s` в prompt'е → команда сама отработала за **4 секунды**, не за 90.

Sentinel прибыл в буфер, но `parse_sentinel` (или вышестоящая логика чтения buffer'а) пропустил его.

## Что наблюдалось

- Воспроизводимо. Один и тот же запрос — один и тот же эффект.
- Меньшие команды (типа `echo alive`, `hostname`, `docker ps`) — работают штатно. Это значит критерий не «всегда промахивается», а зависит от того, **что приходит после sentinel'а до следующего read-tick'а** read-loop'а. Для болтливого Starship-prompt'а с тяжёлой ANSI-шапкой шанс попасть в этот случай высокий.
- Без P2 (старые билды) проблема **выглядела бы как ES timeout** или «канал глюканул» — и тратилось бы время на неверный диагноз. P2 — необходимая опора для воспроизведения.

## Гипотеза

Парсер sentinel'а (анкеренный regex по строке, типа `^__WD_DONE_<uuid>__(\d+)\s*$`) применяется к буферу line-by-line. Один из двух сценариев:

**(а) Strip-ANSI работает на уровне output'а в stdout, а sentinel-detection — на сыром буфере.** Если в строке с sentinel'ом перед `__WD_DONE_` есть ANSI escape (например, prompt `\r` без `\n` оставляет курсор в той же строке, и Starship potом выкидывает свои byte'ы прямо перед sentinel'ом) — `^` regex не находит начало.

**(б) Sentinel и Starship-prompt пришли вместе в одном chunk'е, разделение по строкам пошло по `\n` буфера, но один из split'ов слипся.** Например, если split на `\n` (без обработки `\r\n` как пары), хвост строки sentinel'а `__0\r` остаётся склеенным с началом следующей line — и regex `\d+\s*$` тогда `\s*` поглощает `\r`, но дальше идёт ещё ANSI и regex `$` не сработает.

Скорее всего **(б)** — split по `\n` с попаданием `\r` в trailing-часть, плюс жадный `\s*` или strict `$`. Точная диагностика — через `RUST_LOG=debug` запустить тот же тест и посмотреть какие именно строки попадают в `parse_sentinel`.

## Acceptance criteria

- **AC1.** Тот же запрос (см. «Воспроизведение») — `wd --exec` завершается за ≤ 10 сек, exit 0, stdout содержит начало ES-ответа.
- **AC2.** Регрессия — все 354 существующих теста проходят (`cargo test --workspace -- --test-threads=1`).
- **AC3.** Новый unit-test `parse_sentinel_after_starship_prompt_chunk` (table-driven):
  - input: `"output\n__WD_DONE_<uuid>__0\r\n\x1b[1m\x1b[7m%...➜ "` (sentinel + ANSI Starship tail в одном chunk'е)
  - expected: detected exit code = 0
  - Вариации: exit codes 1, 7, 130; CRLF / LF normalization; sentinel в самом конце буфера vs sentinel + ещё output после.
- **AC4.** Новый integration-test с `MockTransport::pair`: host имитирует ssh-сценарий, шлёт `ShellOutput` с payload'ом, в котором `__WD_DONE_<uuid>__0\r\n` в одном chunk'е с Starship ANSI tail. Wd-term корректно завершается, не упирается в timeout.
- **AC5.** `clean_stdout` после fix'а корректно отрезает sentinel-line из output'а — никаких `__WD_DONE_<uuid>__N` в stdout пользователя.

## Тестирование

**Unit (table-driven)** в `apps/wiredesk-term/src/main.rs::tests`:
- `parse_sentinel_with_ansi_after`: `"__WD_DONE_<uuid>__7\r\n\x1b[1mfoo"` → `Some(7)`.
- `parse_sentinel_with_starship_glob`: реальный capture из bug report'а (выше) → `Some(0)`.
- `parse_sentinel_at_buffer_end_with_crlf`: `"prefix\n__WD_DONE_<uuid>__0\r\n"` → `Some(0)`.
- `parse_sentinel_no_match_when_uuid_wrong`: с другим UUID → `None`. (regression)
- `parse_sentinel_no_match_on_echoed_literal_command`: входная команда `echo "__WD_DONE_<uuid>__$?"` echo'ится bash'ем как литерал — `__WD_DONE_<uuid>__$?` без `\d+` — `None` через `\d+` regex (regression, уже есть).

**Integration** в `apps/wiredesk-term/tests/`:
- `oneshot_completes_when_sentinel_chunked_with_prompt`: `MockTransport::pair`, host шлёт `ShellOpen`-handshake, потом несколько `ShellOutput`'ов с output, последний chunk = `<command output>\n__WD_DONE_<uuid>__0\r\n<ANSI Starship junk>\r\n➜ `. Wd-term завершается за < 1 сек, exit 0, clean stdout.

**Live re-test** на CH340 + prod-mup:
- Тот же запрос из «Воспроизведение». Ожидаемое: завершение за ~5 сек, exit 0.

## Не в scope

- Полная нормализация всех ANSI sequences в буфере перед detection (если решение — strip ANSI на line-by-line при detect'е, ОК; полный CSI-state-machine — overkill).
- Изменение wire-формата sentinel'а (UUID + exit code остаются как есть).
- ConPTY-mode для `wd --exec` — exec осознанно остаётся pipe-mode (см. `docs/wd-exec-usage.md`), здесь не трогаем.

## Сложность

**Low.** Точечная правка в parser'е sentinel'а + 4-5 unit-тестов + 1 integration. Скорее всего ≤ 50 строк нового кода + ~80 строк тестов. Bug чисто алгоритмический — нет архитектурных изменений.

## Связанные

- master `7f8dd92` (squash `feat/wd-exec-fixes` в master, 04.05.26) — добавил P2 `format_timeout_diagnostic`, которое сделало воспроизведение этого bug'а очевидным.
- master `aaee62c` — ConPTY для interactive `wd`. Не относится к exec-pipe path, но напоминание что в exec mode TTY симулируется через `ssh -tt` на remote, отсюда ANSI escape codes от Starship.
- memory у клиента: `feedback_wd_exec_practical_limits.md` — описывает workaround на стороне agent'а до фикса (смотреть `last bytes received`, не повторять команду).
