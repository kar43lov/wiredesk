# Бриф: wd --exec — задокументировать host-environment квирки в README

**Status:** ready for direct implementation (не нужен `/planning:make`, это docs-only). Branch: `docs/wd-exec-host-quirks`.

## Контекст

При реализации helper'а `mup/.claude/scripts/itsm.py` (сессия 2026-05-05) уперся в три не-очевидных констрейнта Win-host окружения, которые **не задокументированы** в README/usage-docs `wd`. Каждый стоил часов отладки потому что симптомы выглядят как баг wd, хотя на самом деле — особенность host'а.

Те же грабли попадутся каждому, кто будет писать orchestrator-helpers поверх `wd --exec` (а это основной use-case Claude/agent integration).

**Эпизоды отладки:**

1. `[System.IO.File]::AppendAllBytes($path, $bytes)` падает rc=1 с пустым stderr. **Причина:** `AppendAllBytes` появился в .NET Core / .NET 5+. Win-host PowerShell использует .NET Framework 4.x — там нет этого метода. **Решение:** копить data как text-base64 через `Add-Content -Encoding ASCII -NoNewline`, в финале — один `WriteAllBytes` с decode.

2. `.ps1` файл с русскими комментариями записывается через push-binary, на Win-host выглядит корректно при `Get-Content`, но `powershell -File script.ps1` падает с `TerminatorExpectedAtEndOfString` на line N где никакой проблемы нет. **Причина:** PowerShell parser на ru-RU локали без UTF-8 BOM считает файл CP1251. Multi-byte UTF-8 кириллицы ломают подсчёт кавычек/скобок. **Решение:** ASCII-only в `.ps1`, либо запись с BOM (`0xEF, 0xBB, 0xBF` префикс перед содержимым).

3. Bash orchestrator вида:
   ```bash
   probe=$(wd --exec "...")
   if [ ... ]; then
       wd --exec "..."  # ← рушит канал: ShellOpen send: sending on a closed channel
   fi
   ```
   Со второго `wd --exec` через `$()` subshell — канал «closed channel». При том что прямой `wd --exec` из shell работает 5+ раз подряд. **Причина:** bash subshell как-то держит pipe к serial-устройству, второй subshell видит занятый порт. **Решение:** orchestrator на Python с прямым `subprocess.run([WD_BIN, '--exec', ...])` (не через shell). Проверено на 5+ последовательных вызовах — без проблем.

## Кому это нужно

Любой пользователь, который пишет automation-helper поверх `wd --exec` (Claude / агенты, ad-hoc скрипты для prod-доступа). Эти три грабли — обязательный path: пушить файлы (`AppendAllBytes`), писать PS-скрипты (`BOM`), оркестрировать (`bash subshell`).

Сейчас знание сохраняется в auto-memory consumer'ов (`mup/.claude/.../feedback_wd_chunked_push_lessons.md`), но это per-user. В docs самого `wd` это знание отсутствует — каждый новый пользователь будет наступать.

## Цель

Один раздел в `docs/wd-exec-usage.md` (или новый `docs/wd-exec-host-quirks.md`) — «Win-host environment notes» — с тремя короткими подразделами для каждого квирка: симптом, причина, snippet workaround. Без идеи фиксить эти проблемы — они host-side, не wd-side. Просто docs.

## Подходы

Тут только один подход — написать docs. Нет вариантов.

Структура для каждого квирка:

```markdown
### Q1: [System.IO.File]::AppendAllBytes returns rc=1
**Symptom:** binary chunked-push fails with empty stderr after first WriteAllBytes.
**Reason:** .NET Framework 4.x (default on Win11 PS) doesn't have AppendAllBytes.
**Workaround:** ...снippet...
```

И ссылка на pattern-реализацию (например, `mup/.claude/scripts/itsm.py:push_file()` на конкретный коммит).

## Acceptance criteria

1. **AC1:** в `docs/wd-exec-usage.md` или новом `wd-exec-host-quirks.md` есть раздел «Host environment quirks» с минимум 3 entries (3 квирка из контекста).

2. **AC2:** каждый entry — symptom + reason + workaround-snippet. Snippet runnable (не псевдокод).

3. **AC3:** в `README.md` `wd` есть ссылка на этот раздел в секции «For agent/automation authors» (или эквивалент).

4. **AC4:** примеры helper'ов (если есть `examples/` директория в репо) — обновлены, чтобы избежать этих граблей. Если нет examples-директории — добавить минимум один (`examples/win-host-helper-bootstrap.py` ~50 строк) с правильным push_file и orchestration.

## Риски

- **Стиль доков:** README сейчас русско-язычный. Сохранить русский — но snippets оставить как есть (английский в commits/code-comments хорошо).
- **Дубликат с auto-memory.** Если в будущем auto-memory consumer'ов имеет приоритет — docs могут устаревать. Решение: docs — single source of truth, auto-memory ссылаются на docs URL.

## Сложность

**low**. ~2-3 часа: написать раздел, добавить ссылку, добавить examples. Не требует кода wd.

## Что НЕ входит в scope

- Фиксить квирки на стороне wd (это про host, не про wd).
- Документировать общие PowerShell-pitfalls — только те три что реально пересекаются с wd-чанелом.
- Поддерживать live-тесты на разных Win-версиях — пока документируем только Win11 ru-RU + .NET 4.x (basic).

## Первые шаги

1. Начать раздел с трёх квирков из контекста.
2. Live-проверить snippets на текущем wd-host'е (~1 час).
3. PR с docs + один example.
4. Обновить ссылку в README.

## Связанное

- `wd-exec-payload-quoting.md` — другой класс quoting-проблем (mac → host).
- `wd-exec-utf8-byte-loss.md` — отдельная связанная проблема (host → mac).
- `feedback_wd_chunked_push_lessons.md` (auto-memory автора) — содержит те же 3 квирка, source.
- Реальный пример: `mup/.claude/scripts/itsm.py` (2026-05-05).

---

**Docs-only бриф, scope узкий — три зафиксированных host-квирка, без расширений.**
