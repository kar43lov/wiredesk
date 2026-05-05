# Бриф: wd --exec — escape-safe payload для inline JSON / multi-quote команд

**Status:** ready for /planning:make. Branch предложен: `feat/wd-exec-payload-stdin`.

## Контекст

Реальный кейс из prod-эксплуатации `wd --exec` (триаж pg.support, 2026-05-05). Цепочка quoting'а у `wd --exec` сейчас 4-этапная:

```
macOS bash double-quotes  →  wd serial transport  →  PowerShell single-quotes  →  curl.exe -d
```

Простые однопоточные команды (`docker ps`, `psql -c 'select 1'`, `_count`-запросы) проходят. Но для **inline JSON в `-d`** одного из слоёв escape'ов «съедается»: внешние `\"` вокруг имён полей теряются на пути от bash до curl.exe, и payload приходит в ES искажённым.

**Симптом** (воспроизведено вживую):

```bash
wd --exec "curl.exe -s -XPOST 'http://10.24.200.219:9200/mup-srv-production-*/_search?size=10' \
  -H 'Content-Type: application/json' \
  -d '{\"query\":{\"bool\":{\"filter\":[{\"range\":{\"@timestamp\":{\"gte\":\"now-24h\"}},...]}}}'"
```

ES возвращает HTTP 400:

```json
{"error":{"root_cause":[{"type":"json_parse_exception",
  "reason":"Unexpected character ('q' (code 113)): was expecting double-quote to start field name
            at [Source: ...; line: 1, column: 3]"}]}}
```

Колонка 3 — это первое имя поля `query`. Внешние кавычки вокруг него потерялись по дороге, payload пришёл как `{query:...` вместо `{"query":...`.

**Скоп:** проявляется **только** на сложных payload'ах — вложенный bool/filter/range, многоуровневые объекты, длинные `_source` arrays. Простые JSON (`{"query":{"match_all":{}}}`) обычно проходят. То есть это не категорный break — это «иногда не работает», что хуже из-за непредсказуемости.

## Кому это нужно

Основной consumer `wd --exec` — Claude / агенты для триажа prod (логи ES, инспекция Docker, БД-запросы). Аналитика через Elasticsearch — частая операция. Сейчас обходим через psql + локальные знания, но ES логи остаются closed area.

## Цель

Дать `wd --exec` способ передавать payload-строку (JSON, multi-quote command, long literal) **без escape'инга на вызывающей стороне** — чтобы команда отправлялась в host'овский shell вербатим, как один блок данных, без зависимости от quoting-уровней macOS bash → PowerShell.

## Гипотеза по корневой причине

Где-то в цепочке (точнее — на участке wd serial encoding ↔ PowerShell single-quote unescaping) экранированные `\"` либо unwrap'аются дважды, либо PowerShell single-quoted contexts не сохраняют их как литералы. Точное место надо локализовать репро-тестом (см. ниже AC1).

Косвенный показатель: `feedback_powershell_es_copypaste.md` в auto-memory автора уже описывает похожий класс ошибок при copy-paste через мессенджер (там `\n` ломает `_source`). Это родственный класс — multi-layer transport теряет invariants payload'а.

## Возможные подходы

### Вариант 1 (preferred) — `wd --exec --stdin`

Новый флаг `--stdin`: payload (raw bytes) приходит на stdin процесса `wd`. Wd транспортирует binary-safe в host через текущий serial-канал (либо новый opcode, либо в существующий `Message::ShellInput` с base64-обёрткой), host пишет на stdin запущенной команды.

```bash
echo '{"query":{"bool":{"filter":[...]}}}' | \
  wd --exec --stdin "curl.exe -s -XPOST 'http://10.24.200.219:9200/_search' -H 'Content-Type: application/json' --data-binary @-"
```

curl `--data-binary @-` читает payload со stdin без любых преобразований. Никаких escape'ов на вызывающей стороне.

**Плюсы:**
- binary-safe, нулевой quoting overhead.
- Работает для любых payload'ов (не только JSON: shell-script-блоки, бинари в base64, и т.п.).
- Идиоматично для UNIX-tooling.

**Минусы:**
- Нужен новый wire-message или расширение `ShellInput` (существующий `MAX_PAYLOAD = 4096`, для бóльших payload'ов — chunking — это уже было обсуждено и отброшено в `wd-exec-improvements.md` как overengineering, но для именно stdin-режима может быть оправдано).
- На host'е PowerShell shell-pipe-mode → нужен механизм передачи stdin запущенному child-процессу. Не тривиально через текущий `host-conpty`-roadmap, но возможно через `Start-Process -RedirectStandardInput`.

### Вариант 2 — `wd --exec --payload-file <local-path>`

Wd сначала копирует локальный файл на host (через тот же serial), потом запускает команду с references на host-side temp-file (auto-cleanup при exit).

```bash
echo '{"query":...}' > /tmp/q.json
wd --exec --payload-file /tmp/q.json \
  "curl.exe -s -XPOST '...' --data-binary '@%PAYLOAD%'"
```

Wd подставляет в команду путь host-side temp-файла (`C:\Temp\wd-payload-<uuid>.json`) вместо `%PAYLOAD%`.

**Плюсы:**
- Нет нового runtime-кейса со stdin, использует существующий `Message::ShellInput` для двух последовательных операций (write file → run command).
- Работает с любыми утилитами host'а (curl, openssl, jq), не только теми что читают stdin.

**Минусы:**
- Двухходовая операция (загрузка + execute) — больше overhead на малых payload'ах.
- Cleanup temp-файлов нужен надёжный (timeout + best-effort delete on exit).
- Ограничение `MAX_PAYLOAD` всё ещё применяется per-chunk — при >4 KB payload нужен implicit chunking.

### Вариант 3 (nope) — задокументировать обходы

Сейчас обход — `Set-Content -NoNewline -Path C:\Temp\q.json -Value '{...}'; & curl.exe --data-binary '@C:\Temp\q.json'` через `;` в одной команде. Работает, но:
- Не решает root cause — следующий пользователь снова напорется.
- Сам `Set-Content -Value '{...}'` подвержен **той же проблеме** quoting если JSON сложный (PS-парсер single-quoted strings обычно ок, но `{}` блоки с nested escape'ами — flaky).
- Для агентов — лишний шаг, который надо помнить и применять корректно.

Документирование как fallback — приемлемо, но как **единственное** решение — нет.

## Acceptance criteria

1. **AC1 (репро-тест):** добавить failing-test в `crates/wiredesk-exec-core/src/helpers.rs::tests` (или новый модуль) — формирует команду через текущий `format_command` для PowerShell с inline JSON в одинарных кавычках с вложенными `\"…\"`, проверяет что output'е сохранены **все** двойные кавычки внутри payload'а (без потерь). Тест должен **падать** на текущем коде, **проходить** после фикса.

2. **AC2 (preferred — `--stdin`):** реализован `--stdin` flag в `wiredesk-term`. Сценарий из bash:
   ```bash
   echo '{"q":{"match_all":{}}}' | wd --exec --stdin --ssh prod-mup \
     "curl.exe -s -XPOST 'http://10.24.200.219:9200/_search' -H 'Content-Type: application/json' --data-binary @-"
   ```
   проходит, ES возвращает 200 с реальными hits.

3. **AC3 (binary-safe):** test на random binary payload (256 KB через base64-cycle) — round-trip через `--stdin` восстанавливается байт-в-байт. Покрывает корректность wire-encoding.

4. **AC4 (большие payload'ы):** payload >4 KB транспортируется без `payload too large` ошибки (chunking либо в новом opcode, либо в существующем `ShellInput` с явной поддержкой fragmented sends).

5. **AC5 (документация):** в `docs/wd-exec-usage.md` добавлен раздел «Передача данных на stdin» с минимум 2 примерами (curl `--data-binary @-`, `psql -f -`).

6. **AC6 (regression):** `cargo test --workspace -- --test-threads=1` — все существующие 395+ тестов passes (без регрессий в legacy non-stdin paths).

## Риски

- **Stdin-режим конфликтует с interactive ConPTY** — `wd --exec` уже pipe-mode (не TTY), tooling должен корректно деттектить stdin: если есть данные → передавать на host; если нет → не блокировать. Стандартный `isatty(stdin)` check + non-blocking read.

- **Chunking на host'е** — если идём через `Message::ShellInput` с >4 KB payload, host должен буферизировать chunks до получения END-marker. Это новая state-машинка на host'е (раньше каждый `ShellInput` self-contained). Risk: race на parallel-multiplex (см. `daemon-multiplex.md` brief), но `wd --exec` и так single-shot — не блокер.

- **Backward compatibility** — `--stdin` opt-in флаг, без него поведение не меняется. Совместимо с `--ssh` (передача stdin → host'у → ssh-child через ssh's stdin pipe — стандартное поведение ssh).

- **PowerShell на host'е без --ssh** — `Start-Process -RedirectStandardInput` принимает строку или файл, не stream. Возможно для PS-direct path придётся идти через temp-файл (Вариант 2 fallback'ом для stdin'а в PS-direct mode), а stdin true-streaming работает только в `--ssh` path.

## Сложность

**medium**. Новый flag, новая wire-семантика для chunked payload, тестирование на binary-edge cases. Оценка ~2–3 дня c live-тестами на CH340 + Win11.

Если ограничиться payload до 4 KB (без chunking) — можно уложиться в 1 день, и chunking сделать отдельным follow-up'ом когда ELK-запросы того потребуют. **Recommend:** начать с не-chunked версии, в plan'е оставить hook для future chunking.

## Что НЕ входит в scope

- Чтение от host'а через stdout / stderr separation — ортогонально, отдельный brief если потребуется.
- Compression payload'а — `wd-exec-compression.md` уже покрывает обратное направление (host → client). При желании можно симметрично сжать stdin-payload, но это micro-optimization, не сейчас.
- GUI-интеграция (например, drag-n-drop файла на `wd --exec` ярлык) — hold off, командная строка first.
- Authentication-fу (отдельный шифр для payload, контроль доступа) — out of scope, наш канал и так физически изолирован.

## Первые шаги (для /planning:make)

1. Repro-test (AC1) — failing на master, документирует проблему.
2. Прототип `--stdin` для `--ssh` path: pipe от mac stdin → `Message::ShellInput`-byte-stream → host shell-process stdin. Сначала без chunking (payload ≤ 4 KB), с явной ошибкой `payload too large` за пределами.
3. Live-тест на ES `_search` с реальной date_histogram + 2 terms-агг (payload ~600–800 байт).
4. Добавить chunking как follow-up если возникнет реальный кейс с payload >4 KB (большие mapping-update'ы и т.п.).
5. PS-direct path (без `--ssh`) — отдельной задачей или fallback'ом через временный файл (Вариант 2).

## Связанное

- `wd-exec-improvements.md` — `MAX_PAYLOAD` 512 → 4096 (уже сделано).
- `wd-exec-compression.md` + `plans/20260505-wd-exec-compress.md` — обратное направление (host stdout сжатие). Ортогонально.
- `wd-exec-via-gui-ipc.md` — IPC bridge через GUI socket. На текущий бриф не влияет: `--stdin` пробрасывается через тот же `IpcRequest` payload (новое поле `stdin: Option<Vec<u8>>`).
- Реальный эпизод триажа: pg.support session 2026-05-05 (кейс «пропавшего» запроса в Конструкторе МУП), ES drill-down не удался — обходились без него за счёт SQL.

---

**Один эпизод, одна тема — расширения скоупа в этом брифе нет.** На текущий момент это единственная зафиксированная проблема `wd --exec` payload-quoting'а из реальной prod-эксплуатации; остальные active wd-exec briefs (compression, IPC, GUI-shell) — про другие направления.
