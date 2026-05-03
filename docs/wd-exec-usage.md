# `wd --exec` — usage guide for AI agents

Drop-in replacement for `Bash(...)` when the target machine is the Win11 host on the other end of WireDesk's serial link. Behaves like a normal shell call: clean stdout, propagated exit code, pipe-friendly.

## TL;DR

```bash
wd --exec "<powershell command>"                  # run on host PS
wd --exec --ssh <alias> "<bash command>"          # run on remote box via host PS → ssh
wd --exec --timeout <secs> "<command>"            # default 30s, exit 124 on timeout
```

`wd` is a zsh alias for `./target/release/wiredesk-term`. The binary itself is at `target/release/wiredesk-term`.

## Что нужно знать ДО запуска

1. **`wd` и `WireDesk.app` взаимоисключающие** — один процесс держит serial-порт. Если открыт GUI — `wd --exec` упадёт с busy. Закрой App или используй один из двух за раз.
2. **Macros в alias не работают с env-prefix.** Для трейса:
   ```bash
   export RUST_LOG=debug
   wd --exec "..." 2>&1 | tee /tmp/wd-trace.log
   unset RUST_LOG
   ```
   Или вызывай бинарь напрямую: `RUST_LOG=debug ./target/release/wiredesk-term --exec "..."`.
3. **Латенси handshake'а** ≈ 1.5–2 сек на каждый `wd --exec` (Hello → ShellOpen → spawn PS). Для batch'а — собирай команды в одну (`cmd1; cmd2; cmd3` через `--exec`).
4. **Wire-channel — 115200 baud (~11 KB/s)** — мелкий output ок, гигабайты не качай.

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

## Exit codes

| Code | Значение |
|---|---|
| 0–253 | Реальный exit code команды (PS `$LASTEXITCODE` или bash `$?`) |
| 1 | PS terminating error (catch'нулось через `try { } catch { }`) — например `Get-Item /nonexistent` |
| 124 | Sentinel не пришёл за `--timeout` секунд (default 30). Convention `timeout(1)`. |
| 125 | Transport error (serial drop'нулся, host исчез) |
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

- **Interactive prompts** (`sudo` без `-S`, `git push` с пасс-фразой ключа, `ssh` с password auth, `vim`, `htop`, `git interactive rebase`) — сломаются. Никакого ConPTY. Используй non-interactive формы:
  - `sudo -n` или `sudo` через настроенный sudoers без password
  - Ssh keys без passphrase либо через ssh-agent
  - `git --no-pager` для команд которые иначе зовут `less`
  - Для git editor'а — `EDITOR=true git ...` или `--no-edit` где есть.
- **Multi-line input** — wd шлёт команду одной строкой. Multiline scripts либо собирай через `;`, либо пиши скрипт в файл и зови `bash script.sh`.
- **stdin** — нет. `wd --exec "cat | grep foo"` без stdin провиснет до timeout.
- **Очень большой output** (>100 KB) — медленно (ограничен 11 KB/s). Лучше grep'ни на remote.

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

## Memory

Persistent context — в `~/.claude/projects/-Users-pgmac-Data-prjcts-wiredesk/memory/`. Самое полезное: `feedback_serial_terminal_bridge.md`, `project_conpty_followup.md`.
