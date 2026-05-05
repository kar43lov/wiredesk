# Бриф: `wd put-file LOCAL REMOTE` — built-in команда для chunked binary push

**Status:** ready for /planning:make. Branch предложен: `feat/wd-put-file`.

## Контекст

Реальный кейс из orchestrator-helper'а `mup/.claude/scripts/itsm.py` (сессия 2026-05-05). Чтобы запустить custom PowerShell-скрипт на Win-host'е, надо его туда залить. Сейчас — только через `wd --exec` с chunked-base64-protocol реализованным руками в helper'е:

```python
# Из itsm.py:push_file (упрощено):
def push_file(local_path, remote_path):
    b64 = base64.b64encode(local_path.read_bytes()).decode()
    chunks = [b64[i:i+1500] for i in range(0, len(b64), 1500)]
    for i, chunk in enumerate(chunks):
        verb = 'Set-Content' if i == 0 else 'Add-Content'
        cmd = f"{verb} -Path '{remote_path}.b64' -Value '{chunk}' -NoNewline -Encoding ASCII; 'OK'"
        wd_run(cmd)
        time.sleep(1.0)  # USB-serial breathing room
    finalize = (
        f"$b64=Get-Content '{remote_path}.b64' -Raw; "
        f"[System.IO.File]::WriteAllBytes('{remote_path}', [Convert]::FromBase64String($b64)); "
        f"Remove-Item '{remote_path}.b64'; "
        f"(Get-Item '{remote_path}').Length"
    )
    wd_run(finalize)
```

~50 строк ad-hoc кода, который должен быть **в каждом** orchestrator'е. Уже сейчас:
- `mup/.claude/scripts/itsm.py` — для пуша `itsm_run.ps1` и (раньше) `itsm_parser.py`.
- Будущие helper'ы (Дознание, Справочники, любой кастомный prod-инструмент) повторят то же самое.

## Грабли которые pattern избегает (см. `feedback_wd_chunked_push_lessons.md`)

1. `[System.IO.File]::AppendAllBytes` отсутствует в .NET Framework 4.x → нельзя append'ить binary напрямую. Нужно копить ASCII-base64 в text-file.
2. Sleep между chunks <0.5 сек → `IPC read fail`. Безопасно — 1.0 сек.
3. Empty stderr на error → нужен retry x3 с backoff.
4. Лимит payload 4 KB → max chunk ~1500 b64-chars (~1100 raw + ~150 байт обёртки).

Каждый author заново наступает на эти грабли.

## Кому это нужно

Любой Claude / agent / dev, пишущий orchestrator поверх `wd --exec`. Сейчас известно ≥1 потребителя (`itsm.py`), будет больше по мере того как агенты автоматизируют prod-доступ.

Альтернативно — стандартизировать pattern в **doc** (это второй открытый brief — `wd-exec-host-environment-quirks.md`). Но docs объясняют грабли и снippet'ы, не убирают boilerplate.

## Цель

Дать `wd` встроенную команду:

```bash
wd put-file --src ./script.ps1 --dst 'C:\Windows\Temp\script.ps1'
```

Под капотом — тот же chunked-base64-pattern, но реализован раз в самом `wd` (Rust-сторона), с правильным retry, верификацией размера и без подвохов host-side .NET 4.x.

## Подходы

### Вариант 1 (preferred) — `wd put-file` как полноценная sub-команда

Аналогично `wd --exec`, новая sub-команда `wd put-file` с флагами `--src` / `--dst` / `--chunk-size` / `--ssh ALIAS`. Логика:

1. Mac читает `--src` файл, base64-encode'ит.
2. Mac разбивает на чанки `chunk_size` (default 1500 b64, конфигурируемо).
3. Mac последовательно посылает на host'е: `Set-Content` (1й chunk) или `Add-Content` (последующие) в `<dst>.b64.tmp` через тот же `Message::ShellInput`.
4. Между chunks — внутренняя пауза (>= 1.0 сек на текущем CH340, configurable).
5. После всех chunks — finalize-команда: `WriteAllBytes` decode + remove tmp + emit size для верификации.
6. Mac сравнивает emitted size с `len(src)`, при mismatch — error.
7. Прогресс — на stderr (`pushed 3 KB / 7 chunks`), для CI-runtime — `--quiet`.

С `--ssh ALIAS` — те же шаги через ssh (host-shell на linux'е, decode через `cat | base64 -d > $dst`). Уже понятный path, переиспользует существующий ssh-chain в wd.

**Плюсы:**
- Builtin = единый source of truth, убираем 50+ строк boilerplate из каждого orchestrator'а.
- Можно тестировать в CI / unit-тестах самого wd (round-trip + edge cases).
- На host'е — единый pattern, а не «как кто реализовал».
- Экспортируется и для bash/non-Python потребителей одинаково.

**Минусы:**
- Новый user-facing API → maintenance burden.
- Зависит от текущего `Message::ShellInput` chunking; если в `wd-exec-payload-quoting.md` brief стартует stdin-режим — придётся согласовывать.

### Вариант 2 — `wd --exec --put-file LOCAL:REMOTE` (модификатор exec)

Тот же pattern, но как pre-step перед `--exec`:

```bash
wd --exec --put-file ./script.ps1:C:\Temp\script.ps1 \
  "powershell -File 'C:\Temp\script.ps1' arg1"
```

Wd сначала пушит файл, потом запускает команду. Удобно когда file-push **связан** с конкретной командой (типичный сценарий orchestrator'а).

**Плюсы:**
- Атомарно для одного сценария.
- Не вводит новую sub-команду.

**Минусы:**
- Меньшая гибкость (нельзя «просто залить файл» без последующего execute).
- Усложняет existing `--exec` API.

### Вариант 3 (nope) — оставить как есть, документировать pattern

Documented в brief `wd-exec-host-environment-quirks.md`. Каждый orchestrator повторяет код. Maintenance fragmented.

## Acceptance criteria

1. **AC1 (sub-команда):** `wd put-file --src LOCAL --dst REMOTE` (Вариант 1). Тривиальный случай — один chunk до 1100 байт raw — round-trip без потерь. Live-тест на Win11 + CH340.

2. **AC2 (multi-chunk):** файл 5 KB (4-5 chunks) — round-trip байт-в-байт, размер на host'е = размер local. Live-тест.

3. **AC3 (Cyrillic UTF-8 file content):** `.txt` с UTF-8 кириллицей и BOM — round-trip preserves bytes (включая BOM-байты). Не должно быть UTF-8 byte-loss как в `wd-exec-utf8-byte-loss.md` (round-trip через ASCII-base64 неуязвим).

4. **AC4 (--ssh):** `wd put-file --src ./local --dst /tmp/remote --ssh prod-mup` работает на Linux-host'е через ssh-chain, использует `cat | base64 -d > $dst` на удалённой стороне.

5. **AC5 (verify):** размер на host'е сравнивается с локальным — на mismatch wd возвращает rc≠0 и stderr с диффом. Не «тихо успешно».

6. **AC6 (regression):** все existing-тесты passes; `wd --exec` поведение не меняется.

## Риски

- **CH340 USB-serial bottleneck.** Push 50 KB файла = ~50/11 ≈ 5 секунд + handshake overhead. Большие файлы (PS1 ~5 KB, parser.py ~5 KB → итого ~10 KB ~10 сек) — приемлемо. Файлы >100 KB — спорное удобство (быстрее через ssh+scp если есть ssh; либо через `--ssh` flow).
- **Hardlink на Шаг 4 wd-exec-utf8-byte-loss brief'а.** Если там реализуется per-chunk-handshake — `put-file` может его переиспользовать вместо собственного sleep. Договорённость по приоритетам решит сама.
- **Backward compat.** `put-file` — новая команда, не ломает `--exec`. Вариант 2 (модификатор) сложнее с этой т.зрения.

## Сложность

**low-medium**. Реализация на Rust — ~150-300 строк (sub-command parsing + chunked send loop + finalize). Тесты ~100 строк. Ориентировочно 1-2 дня работы вкл. live-тестинг.

## Что НЕ входит в scope

- Получение файла обратно (`wd get-file`) — отдельный brief если возникнет use case (пока обходимся base64 round-trip через `--exec`, как в `itsm.py raw`).
- Compression — `wd-exec-compression.md` brief; ортогонально, можно сложить позже.
- Permissions / file mode preservation — на Win-host nontrivial, отложить.

## Первые шаги (для /planning:make)

1. Реализовать pure non-ssh path (PowerShell host) — repro-test от `itsm.py` (push 5 KB `.ps1`, проверить размер).
2. `--ssh` path через `cat | base64 -d` — добавить после non-ssh.
3. Live-тест от `itsm.py` (заменить ad-hoc push на `wd put-file`) — проверить регрессию по времени и надёжности.
4. Dokumentation в `docs/wd-put-file.md`.

## Связанное

- `wd-exec-utf8-byte-loss.md` — другая host→client integrity-проблема, ортогональна.
- `wd-exec-host-environment-quirks.md` — docs про host-side ограничения; этот brief убирает один из квирков (ad-hoc chunked push) первоклассной командой.
- `wd-exec-payload-quoting.md` — payload-quoting через `--stdin`. Другой подход к решению mac→host data-transfer; `put-file` про **файлы**, `--stdin` про **payload одной команде**. Совместимы.
- Real-world consumer: `mup/.claude/scripts/itsm.py:push_file()` — pattern-source.
- `feedback_wd_chunked_push_lessons.md` (auto-memory автора) — каталог host-side граблей которые pattern избегает.

---

**Один эпизод (orchestrator-helper'ы), одна тема (стандартизация file push) — без расширений.** После реализации можно убрать ad-hoc `push_file` из `itsm.py` и любых будущих orchestrator'ов.
