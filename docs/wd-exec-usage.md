# `wd --exec` — usage guide for AI agents

Drop-in replacement for `Bash(...)` when the target machine is the Win11 host on the other end of WireDesk's serial link. Behaves like a normal shell call: clean stdout, propagated exit code, pipe-friendly.

## TL;DR

```bash
wd --exec "<powershell command>"                  # run on host PS
wd --exec --ssh <alias> "<bash command>"          # run on remote box via host PS → ssh
wd --exec --timeout <secs> "<command>"            # default 90s, exit 124 on timeout
wd --exec --compress "<command>"                  # gzip+base64 stdout (5-10x for text)
```

`wd` is a zsh alias for `./target/release/wiredesk-term`. The binary itself is at `target/release/wiredesk-term`.

## Что нужно знать ДО запуска

1. **`wd --exec` и `WireDesk.app` теперь работают параллельно** (Mac, начиная с feat/wd-exec-ipc). GUI на старте поднимает Unix-socket в `~/Library/Application Support/WireDesk/wd-exec.sock`, `wd --exec` коннектится к нему и ходит через тот же serial, который GUI использует для clipboard sync. Если GUI закрыт — `wd --exec` falls back на direct-open serial (поведение idential pre-implementation). **Interactive `wd`** (без `--exec`, PTY-mode bridge для Ghostty/iTerm) **остаётся single-port-owner** — если GUI запущен, interactive `wd` упадёт с busy; закрой GUI на время interactive-сессии.
2. **Macros в alias не работают с env-prefix.** Для трейса:
   ```bash
   export RUST_LOG=debug
   wd --exec "..." 2>&1 | tee /tmp/wd-trace.log
   unset RUST_LOG
   ```
   Или вызывай бинарь напрямую: `RUST_LOG=debug ./target/release/wiredesk-term --exec "..."`.
3. **Латенси handshake'а** ≈ 1.5–2 сек на каждый `wd --exec` (Hello → ShellOpen → spawn PS). Для batch'а — собирай команды в одну (`cmd1; cmd2; cmd3` через `--exec`).
4. **Wire-channel — 115200 baud (~11 KB/s)** — мелкий output ок, гигабайты не качай.
5. **Лимит команды — 4 KB.** Один `wd --exec "..."` packet'ом — payload до 4096 bytes (bump'нут с 512 в feat/wd-exec-fixes). Типичный ES `_search` с агрегациями (~600 байт) и средние shell-конвейеры умещаются. Длиннее — разбивай на несколько `wd --exec` или пиши скрипт в файл и зови `bash script.sh`.

## Примеры

### PowerShell на host'е
```bash
wd --exec "Get-ChildItem"
wd --exec "Get-Process | Where-Object { \$_.CPU -gt 100 }"
wd --exec "Test-Path C:\\some\\file.txt"
wd --exec "git -C C:\\repo status"
```

### SSH через host (на любую linux-машину куда у host'а есть доступ)
```bash
wd --exec --ssh prod-mup "docker ps"
wd --exec --ssh prod-mup "kubectl get pods -n prod"
wd --exec --ssh prod-mup "tail -100 /var/log/syslog"
wd --exec --ssh prod-mup "git -C /opt/app log --oneline -10"
```

`<alias>` — это alias из `~/.ssh/config` **на host'е** (не на Mac'е). Управление SSH — через стандартный OpenSSH ControlMaster, не наш код.

### Pipe-friendly
```bash
wd --exec --ssh prod-mup "docker ps" | grep mup.web
wd --exec --ssh prod-mup "ps aux" | head -20
```

### Long-running с custom timeout
```bash
wd --exec --timeout 300 --ssh prod-mup "apt-get update && apt-get -y dist-upgrade"
```

### Compression для больших текстовых выводов

```bash
# До: ~18 сек на 200 KB логов
wd --exec --ssh prod-mup "docker logs --tail 5000 mup.srv.main 2>&1"

# После: ~3 сек (×6 быстрее)
wd --exec --compress --ssh prod-mup "docker logs --tail 5000 mup.srv.main 2>&1"
```

Сжимает stdout на host'е (gzip+base64), разворачивает на Mac. Stdout байт-в-байт идентичен non-compress версии — pipe-friendly работает: `wd --exec --compress --ssh prod 'docker logs ...' | grep ERROR | head -20`.

Когда **включать**:
- `docker logs --tail N` на болтливом контейнере (ratio 5–10×)
- `kubectl logs / describe pod` с YAML/text-выводом
- `cat /var/log/<file>.log` на linux'е через `--ssh`
- `Get-EventLog -Newest N`, `Get-Content C:\big.log` на host PS

Когда **НЕ включать**:
- Уже сжатый бинарь (`cat /usr/bin/...`) — ratio ~1×, оверхед впустую
- Малые выводы (<1 KB): overhead +0.5 сек, нет выгоды
- `docker exec ... cat /some/binary.tar.gz` — двойной gzip ничего не даёт

Поддерживается **обе path'и**:
- `wd --exec --compress --ssh <alias>` — bash через `gzip -c | base64`
- `wd --exec --compress` без --ssh — PowerShell через `[System.IO.Compression.GZipStream]`

Кириллица в PS-выводе работает: обёртка явно ставит `[Console]::OutputEncoding = UTF8` перед запуском команды.

Decode error → exit 125 (transport-class) с диагностикой в stderr `--compress decode failed: <msg>`.

## Exit codes

| Code | Значение |
|---|---|
| 0–253 | Реальный exit code команды (PS `$LASTEXITCODE` или bash `$?`) |
| 1 | PS terminating error (catch'нулось через `try { } catch { }`) — например `Get-Item /nonexistent` |
| 124 | Sentinel не пришёл за `--timeout` секунд (default 90). Convention `timeout(1)`. На stderr печатается `last bytes received: "..."` — last 256 байт wire-buffer'а для диагностики где залип (mid-MOTD vs после READY-marker vs mid-command output). |
| 125 | Transport error (serial drop'нулся, host исчез) **или** `--compress` decode failure (host выдал невалидный gzip+base64 payload — на stderr печатается `--compress decode failed: <msg>`) |
| любой | **Ctrl+C на `wd --exec` через IPC mode**: term-процесс умирает мгновенно, но host-side команда продолжает выполняться до собственного завершения (не interrupt'им host shell mid-run — destructive operations safety). GUI handler ждёт sentinel/timeout, потом освобождает single-inflight queue. Следующий `wd --exec` будет ждать пока предыдущая команда не закончится на host'е. Acceptable trade-off для clean-state semantics. |
| любой | Обычный shell exit propagation |

## Гочи

### PS path
- **`$ErrorActionPreference='Stop'`** в обёртке. Любая cmdlet error — terminating, идёт в catch → exit 1. Если хочешь "продолжить после non-terminating error" — оборачивай команду в `try {<cmd>} catch {}` сам, либо `Get-Item -ErrorAction SilentlyContinue ...`.
- **Cmdlets не сетят `$LASTEXITCODE`.** Pre-init = 0 в нашей обёртке. External commands (`& ssh`, `& cmd /c ...`) сетят как обычно. Если pipe'ишь — последний `$LASTEXITCODE` — это последняя external команда.
- **Multi-line PS commands** — pass through `;` или `\``-wrap. Не отправляй буквальные `\n` в command string'е (он парсится shell'ом до того как доходит до wd).

### SSH path
- **`ssh -tt` форсит TTY** на remote. Remote shell интерактивный — Starship/Oh-My-Zsh prompts работают, цвета приходят. `clean_stdout` чистит ANSI и MOTD.
- **Quoting** — `wd --exec --ssh prod 'docker ps --filter status=running'`. Внешний zsh парсит, передаёт `docker ps --filter status=running` в `--exec`. Дальше уезжает целиком в bash на remote через ssh args.
- **Если в команде есть одинарные кавычки** — используй внешние двойные с экранированием: `wd --exec --ssh prod "docker ps --filter \"status=running\""`. Edge case.
- **Persistent SSH** — настраивается через ControlMaster в `~/.ssh/config` **на host'е** (не на Mac'е), и это вне нашего кода:
  ```
  Host prod-mup
      ControlMaster auto
      ControlPath /tmp/ssh-%r@%h:%p
      ControlPersist 10m
  ```

### Sequential calls
- Два `wd --exec` подряд работают (host slot free'ится между ними через ShellClose+Disconnect). Но **между ними ~2 сек handshake'а каждый раз**. Для серии команд лучше — одна команда с `;` или `&&`.

## Чего НЕЛЬЗЯ делать

- **Interactive prompts** (`sudo` без `-S`, `git push` с пасс-фразой ключа, `ssh` с password auth, `vim`, `htop`, `git interactive rebase`) — сломаются. `wd --exec` намеренно pipe-mode (нужно для sentinel detection в clean stdout); для интерактивщины используй просто `wd` без `--exec` — там ConPTY и всё работает. Для скриптов внутри `--exec` используй non-interactive формы:
  - `sudo -n` или `sudo` через настроенный sudoers без password
  - Ssh keys без passphrase либо через ssh-agent
  - `git --no-pager` для команд которые иначе зовут `less`
  - Для git editor'а — `EDITOR=true git ...` или `--no-edit` где есть.
- **Multi-line input** — wd шлёт команду одной строкой. Multiline scripts либо собирай через `;`, либо пиши скрипт в файл и зови `bash script.sh`.
- **stdin** — нет. `wd --exec "cat | grep foo"` без stdin провиснет до timeout.
- **Очень большой output** (>100 KB) — медленно (ограничен 11 KB/s). Лучше grep'ни на remote. Future: `--compress` flag (см. `docs/briefs/wd-exec-compression.md`).

## Encoding (кириллица в SQL-запросах)

`wd --exec` передаёт команду как байты на host'е. PowerShell на Win по умолчанию в **cp1251/cp866** (зависит от региональных настроек), при отправке в `psql` (который ждёт **UTF-8**) кириллица в `WHERE`-clause ломается:
```
ERROR:  invalid byte sequence for encoding "UTF8": 0xa6
```

`chcp 65001` + `[Console]::OutputEncoding = [Text.Encoding]::UTF8` помогает не всегда (зависит от того, как PowerShell конвертирует argv → child process). Workaround на стороне SQL — **Unicode escape** `U&'\NNNN'`:

```bash
# Найти "Стародумов" / "Виктор":
wd --exec --ssh prod-mup "psql ... -c \"select * from official where last_name = U&'\\0421\\0442\\0430\\0440\\043E\\0434\\0443\\043C\\043E\\0432' and first_name = U&'\\0412\\0438\\043A\\0442\\043E\\0440'\""
```

Codepoint каждой буквы: А=0410, Б=0411, ..., Я=042F, а=0430, ..., я=044F, Ё=0401, ё=0451.

**Read из БД работает корректно** — кириллица в результатах приходит в UTF-8 без проблем. Issue только при отправке в `WHERE`/`VALUES`. Альтернатива — поиск по ASCII-полям (UUID, mail, login если в латинице).

## Под капотом (если нужно дебажить)

Sentinel framing: `__WD_DONE_<uuid>__<exit_code>`. UUID per call. PS-only:
```
$LASTEXITCODE=0; $ErrorActionPreference='Stop'; try { <cmd> } catch { $LASTEXITCODE=1 }; "__WD_DONE_<uuid>__$LASTEXITCODE"
```

SSH-mode (bash payload):
```
echo __WD_READY_<uuid>__; <cmd>; echo "__WD_DONE_<uuid>__$?"
```

`__WD_READY_<uuid>__` — нижняя граница для clean_stdout (срезает MOTD/banner). `__WD_DONE_<uuid>__N` — верхняя + exit code.

Trace через `RUST_LOG=debug` показывает каждый recv'ed packet, parse-state, prompt detection.

### `--compress` wire-format

**Exit code в обоих путях передаётся in-band через `__WD_RC__<rc>__` маркер внутри gzipped payload'а** — это POSIX-portable (работает в bash/sh/dash на удалённой стороне) и обходит проблему `${PIPESTATUS[0]}` теряющегося в pipe-subshell'е. Sentinel rc хардкоженый 0 — runner после decode извлекает реальный rc из marker'а через `extract_compressed_rc`.

Bash (через `--ssh`):
```bash
echo __WD_READY_<uuid>__
{ <cmd> 2>&1; printf "__WD_RC__%s__\n" "$?"; } | gzip -c | base64
echo
echo "__WD_DONE_<uuid>__0"
```

PowerShell (host-direct):
```powershell
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Write-Output "__WD_READY_<uuid>__"
$LASTEXITCODE=0; $ErrorActionPreference='Stop'
try { $out = & { <cmd> } 2>&1 | Out-String } catch { $out = $_.ToString(); $LASTEXITCODE=1 }
$rc = $LASTEXITCODE
$ms = New-Object System.IO.MemoryStream
$gz = New-Object System.IO.Compression.GZipStream($ms, [System.IO.Compression.CompressionMode]::Compress)
$bytes = [Text.Encoding]::UTF8.GetBytes($out + "__WD_RC__" + $rc + "__")
$gz.Write($bytes, 0, $bytes.Length); $gz.Close()
Write-Output ([Convert]::ToBase64String($ms.ToArray()))
Write-Output "__WD_DONE_<uuid>__0"
```

Между `__WD_READY_` и `__WD_DONE_` — base64-encoded gzip-payload (multi-line, 76 chars per line для bash; single-line для PS). Runner буферит весь блок до sentinel'а, потом decode + extract_compressed_rc → один callback. Streaming в этом режиме не работает — trade-off opt-in флага.

### Известная limitation: cyrillic в PS source-литералах

`wd --exec --compress 'Write-Output "Привет"'` — кириллица **в тексте PS-скрипта** придёт как mojibake (`╨Я╤А╨╕╨▓╨╡╤В`). Корень проблемы: PowerShell в pipe-mode читает stdin через `[Console]::InputEncoding` = OEM codepage (cp866 на RU Win11). UTF-8 байты от Mac'а интерпретируются как cp866 → строка содержит mojibake-кодпойнты ещё до того как наш wrapper успеет что-то сделать. Без compress'а это работает только потому что output идёт через ту же кривую cp866 в обратную сторону — два errors compensate roundtrip.

**Реальные кейсы (cyrillic в FILE CONTENT, в API responses, в БД-запросах) — работают**, потому что .NET StreamReader / API парсеры читают свои источники с правильным encoding и кладут в `$variable` корректную строку. Через `Out-String` → UTF8.GetBytes → wire — всё ок.

**Workaround если нужен cyrillic literal:** не использовать compress для такой команды (`wd --exec` без `--compress` работает через accidental roundtrip). Или вынести payload в файл и читать через `Get-Content`.

## Memory

Persistent context — в `~/.claude/projects/-Users-pgmac-Data-prjcts-wiredesk/memory/`. Самое полезное: `feedback_serial_terminal_bridge.md`, `project_conpty_followup.md`.
