# Бриф: wd --exec — потеря байт в длинных UTF-8 строках на host→client пути

**Status:** ready for /planning:make. Branch предложен: `fix/wd-exec-utf8-stream-integrity`.

## Контекст

Реальный кейс из prod-эксплуатации `wd --exec` (сессия pg.support, 2026-05-05). Из стандартного PowerShell stdout (`py -3 itsm_parser.py … summary`) на host'е возвращается plain-text summary тикета ITSM на ~3-4 KB с большим количеством русских строк (UTF-8 multi-byte). Обратно на mac иногда **точечно теряется по 2 байта** в середине длинных строк.

Симптом — Unicode replacement character `��` (U+FFFD U+FFFD) в decoded выводе:

```
Body: Добрый день! Спасибо за обращение. О��исанная вами проблема...
                                          ^^ — потерян байт буквы "п" (CC BF EU)
Body: ... между ними в и��рархии есть промежуточный узел...
                       ^^ — потерян байт буквы "е"
Body: ... Подробная информация Причина маршр��тизации: ...
                                            ^^ — потерян байт буквы "у"
```

Закономерность: **по одной кириллической букве (2 байта UTF-8) в каждой второй-третьей длинной строке**. Не каждая строка теряет, не два рядом. Чисто ASCII строки (заголовки, имена файлов латиницей) — не теряют. Короткие русские (`Подробная информация` — целиком ОК).

**Воспроизведение:**

```bash
# В .claude/scripts/itsm.py запустить ticket-команду на любой тикет с длинными русскими body
python3 .claude/scripts/itsm.py ticket 124887
# Grep'ом отловить replacement chars
python3 .claude/scripts/itsm.py ticket 124887 | grep "$(printf '\xef\xbf\xbd')"
```

## Кому это нужно

Основной consumer `wd --exec` — Claude / агенты для триажа prod (логи ES, ITSM-тикеты, чтение русских полей БД). Любой текст с длинными русскими строками (поддержка, тикеты, отчёты) теряет точность. Для триажа критично — клиентское «не работает X в модуле Y» может потерять ключевую букву и стать «не работает X в модуе Y», что меняет интерпретацию.

Workaround есть (`itsm.py raw TID` качает binary через base64 round-trip), но это:
- лишний код в каждом потребителе
- +5 секунд на канал
- решает только specific case (свой бинарь), не общий plain-text-stdout сценарий

## Гипотеза по корневой причине

Точное место надо локализовать (см. AC1), но кандидаты:

1. **PowerShell stdout encoding mismatch.** PowerShell на ru-RU локали по умолчанию выводит в CP1251. wd-host читает stdout через ConPTY/serial без явного UTF-8 transcoding. Если PS внутренне держит UTF-16 строки и при выводе их byte-копирует с предположением CP1251 — multi-byte UTF-8 sequences ломаются. Но симптом не «всё кириллица в мусоре», а **изредка потеря 2-х байт** — это не encoding-mismatch, это byte loss.

2. **CH340 USB-serial buffer overrun.** На bandwidth ~11 KB/s длинная непрерывная строка может насытить host-side serial buffer (типично 64–128 байт). Если хост шлёт без back-pressure от mac, при заполнении буфера 1-2 байта могут потеряться. Поведение «теряется 2 байта на kilobyte» — характерно для UART-overflow scenarios.

3. **Sentinel-detection race.** В `feedback_wd_exec_practical_limits.md` зафиксирован sentinel-detection bug (исправлен в master `6c9b163`), но если current binary устаревший — он мог регрессировать. Маловероятно (символы теряются в середине, не на границах sentinel'а), но стоит проверить.

Гипотеза №2 — наиболее вероятная: 11 KB/s × непрерывный stream русского текста → периодическая потеря отдельных символов. Лечится hardware flow-control на serial или explicit chunked output с handshake.

## Возможные подходы

### Вариант 1 (preferred) — chunked output с per-chunk-handshake

Host-side эмиттит data в небольших chunks (например, по 256 байт), после каждого ждёт ACK от mac до продолжения. Это классический soft flow-control — не теряет байты при насыщении serial.

**Плюсы:**
- Решает root cause; работает для любого payload (не только UTF-8).
- Совместимо с CH340 без hardware-flow-control пинов.

**Минусы:**
- 50–100% overhead на короткие команды (RTT × N_chunks). На длинных — амортизируется.
- Требует state-машинки на обеих сторонах.

### Вариант 2 — hardware RTS/CTS flow-control

Если CH340-чип поддерживает на этом setup'е — включить hardware-flow-control, host тогда блокируется на write при насыщении буфера. Bytes не теряются.

**Плюсы:** zero-overhead, прозрачно для всех потребителей.

**Минусы:**
- Зависит от physical-уровня (FT232H upgrade в `ft232h-upgrade.md` brief упоминал улучшения здесь — может уже в roadmap).
- Не работает на CH340 без RTS/CTS пинов (зависит от конкретного adapter'а).

### Вариант 3 — base64-обёртка stdout по умолчанию

Каждый `--exec` автоматически оборачивает stdout host'а в base64, mac расшифровывает. Round-trip binary-safe.

**Плюсы:** простая реализация, не нужно flow-control.

**Минусы:**
- 33% overhead на bandwidth (и так дефицит).
- Меняет контракт `wd --exec` — все existing-консументы ломаются (или нужен opt-in флаг типа `--encoded-stdout`).

### Вариант 4 — opt-in `--exec --binary-stdout`

Аналог Варианта 3 но opt-in: agent явно говорит «жду binary», wd оборачивает. Без флага — старое поведение.

**Плюсы:** backward-compatible.

**Минусы:** 
- Не решает root cause — для пользователей которые забыли флаг проблема остаётся.
- Усложняет API (ещё один режим).

## Acceptance criteria

1. **AC1 (репро):** добавить failing test в `crates/wiredesk-core/src/serial.rs::tests` (или аналог) — генерирует на host'е plain-text 8 KB (русский Lorem Ipsum + кириллица), проверяет round-trip байт-в-байт. Тест должен **падать** на текущем коде, **проходить** после фикса.

2. **AC2 (preferred):** реализован per-chunk-handshake (Вариант 1) или hardware flow-control (Вариант 2 если поддержано). Сценарий из bash:
   ```bash
   wd --exec "1..2000 | ForEach-Object { 'тестовая русская строка номер ' + $_ }" | wc -l
   ```
   возвращает ровно 2000 (без пропусков и без повторов).

3. **AC3 (UTF-8 integrity):** на выходе `wd --exec ...` число `\xef\xbf\xbd` (replacement char) — 0 для любого pure-text stdout с UTF-8 кириллицей до 16 KB.

4. **AC4 (regression):** `cargo test --workspace -- --test-threads=1` — все тесты passes. Скорость передачи bulk-данных не ухудшается > 10% относительно baseline (если flow-control добавляет небольшой overhead — приемлемо).

5. **AC5 (документация):** в `docs/wd-exec-usage.md` раздел «Encoding» обновлён — гарантия UTF-8-safe stdout заявлена явно, со ссылкой на этот brief.

## Риски

- **CH340 без RTS/CTS** — Вариант 2 не сработает. Есть упоминание `ft232h-upgrade.md` — возможно adapter уже планируется заменить, но это hardware изменение, не доступно всем пользователям. Лучше fallback'ом — Вариант 1 (chunked) который не зависит от physical-уровня.
- **Регрессия скорости** — chunked-handshake добавляет RTT на каждый chunk. На 256-байтовых chunks и serial-RTT ~5-10 ms это +5-10 сек на 16 KB output. Для большинства консументов wd (одиночные команды по 100-500 байт) overhead не виден; для bulk-сценариев (`docker logs`, длинные dump'ы) — может стать заметно. Возможно делать adaptive: маленький output → no-handshake, большой → handshake.
- **Backward compatibility** — Варианты 1 и 2 прозрачны для consumer'ов (плюс к надёжности, контракт не меняется). Вариант 3 ломает текущий API. Вариант 4 — opt-in, безопасно но не closes the gap.

## Сложность

**medium**. Точная локализация bug'а займёт 4-6 часов (нужно физическое тестирование на serial с пакетным sniffer'ом, либо точные UART-логи). Реализация chunked-handshake — 1-2 дня. Live-тестирование на различных payload'ах (русский, китайский, бинарь, mixed) — 1 день.

## Что НЕ входит в scope

- Ускорение bandwidth (это про `ft232h-upgrade.md`).
- Чтение от host'а большого binary content — `wd-exec-compression.md` и `wd-exec-payload-quoting.md` об этом смежно.
- Параллельный multiplex — `daemon-multiplex.md` brief.

## Первые шаги (для /planning:make)

1. AC1 — repro-тест на текущей master, fail документирует bug. Возможно чуть-чуть изменить wave-pattern теста до того как падает 100% (а не «иногда»).
2. UART-trace через logic-analyzer на 60 секундах bulk-output — увидеть где байты пропадают физически.
3. Прототип Варианта 1 с CHUNK=512 байт + ACK-handshake — измерить regression на коротких команд.
4. Если CH340 не позволяет (нет RTS/CTS) — фиксируем Вариант 1 как final. Если позволяет — добавить опциональный hardware-flow тоже.

## Связанное

- `feedback_wd_exec_practical_limits.md` (auto-memory автора) — bandwidth 11 KB/s, sentinel-detection bug.
- `wd-exec-compression.md` brief — обратное направление сжатия host stdout. Связано как опция «сжатие vs flow-control».
- `ft232h-upgrade.md` brief — hardware upgrade adapter'а. Может включить hardware flow-control.
- `wd-exec-payload-quoting.md` brief — тоже про data-integrity, но в обратном направлении (mac → host).
- Реальный эпизод: `mup/.claude/scripts/itsm.py` (на 2026-05-05) — рабочий обход через `raw` команду с base64 round-trip.

---

**Один эпизод, одна тема — расширения скоупа в этом брифе нет.** Проблема изолированная (UTF-8 byte-loss в host→client потоке) и не пересекается с открытыми wd-exec brief'ами.
