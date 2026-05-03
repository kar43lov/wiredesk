# Бриф: `wd --exec` — single-shot exec через wiredesk

**Цель:** Добавить новый non-interactive режим в существующий `wiredesk-term` через флаг `--exec` (плюс опциональный `--ssh ALIAS`). Запуск выполняет одну команду на host shell или на удалённом ssh-боксе через host'а, печатает clean stdout, exit'ит с тем же кодом что и команда. Идеально для AI-агентов через Bash-tool и для скриптов.

**Выбранный подход:** Расширить `apps/wiredesk-term` флагом `--exec` — отдельная ветка `run_oneshot()` рядом с `bridge_loop()`. Один бинарь, переиспользует handshake + transport split + heartbeat thread. Sentinel-framing (`__WD_DONE_<uuid>__<exit_code>`) + prompt-detection (regex для PS prompt и Starship/bash prompt'а) для надёжного синка с состоянием remote shell'а.

**Почему так, а не отдельный `wd-exec` бинарь:** меньше дубликата (handshake/transport/heartbeat — те же), один artifact для дистрибуции, единая mutual-exclusion модель (один процесс держит serial). Diff в `main.rs` ~200 строк + ~100 строк тестов.

**Почему prompt detection, а не «послал-cmd+sentinel-сразу»:** без detection первый ssh даёт MOTD банер, ssh-handshake messages, login prompt — всё это попадает в наш «clean stdout» вместе с реальным ответом команды. Prompt regex отрезает приходящее ДО prompt'а от приходящего ПОСЛЕ — pure result of `cmd`.

## Требования

**Функциональные**

- **F1.** `wd --exec "Get-ChildItem"` — выполнить на host'ском PowerShell, stdout (без stdin echo, без prompt'а), exit с `$LASTEXITCODE` команды.
- **F2.** `wd --exec --ssh prod-mup "docker ps"` — на host'е сначала `ssh -tt prod-mup`, дождаться remote prompt'а, послать команду+sentinel, exit с `$?` команды на remote bash.
- **F3.** `--timeout SECONDS` (default 30) — exit 124 если sentinel не пришёл за указанное время.
- **F4.** Exit code пробрасывается напрямую: `wd --exec "exit 7"` → exit 7.
- **F5.** Не включать raw_mode, не open stdin. Просто читать ShellOutput → buffer → искать sentinel.
- **F6.** UUID per call. Sentinel: `__WD_DONE_<uuid>__<exit_code>` на отдельной строке.
- **F7.** Stdout cleaning: после prompt detect обрезаем всё что ДО prompt'а (handshake/banner/MOTD), плюс отрезаем echo введённой команды, плюс саму sentinel-строку.
  - **Асимметрия echo:** PS host в pipe-mode БЕЗ PSReadLine **не** echo'ит stdin — поэтому в PS-only режиме (без `--ssh`) echo'ed-line отсутствует, `clean_stdout` его просто не найдёт (no-op). В `--ssh` режиме remote bash через `ssh -tt` **echo'ит** stdin — echo'ed line присутствует и должна стрипаться. `clean_stdout` пишется так чтобы оба случая работали через тот же helper (если echo-line не найдено — пропускаем шаг).
- **F8.** Корректный shutdown: `ShellClose` + `Disconnect` (как `bridge_loop`).
  - **`--ssh` режим**: после поимки sentinel'а мы остаёмся внутри ssh-сессии. ShellClose закрывает host's PS stdin → ssh видит EOF → ssh exit'ит → PS exit'ит. Должно работать, но если конкретная версия OpenSSH не уйдёт по EOF — добавить phantom `exit\r` перед ShellClose как fallback (отправить в host's PS, который форвардит в ssh; bash `exit` закроет remote shell и ssh клиент завершится). Implementation detail на этапе live-теста — если AC3 / AC4 проходят без phantom-exit'а, не добавляем.
- **F9.** Heartbeat thread активен — host's idle timeout не разрывает session между prompt'ом и sentinel'ом для долгих команд.

**Нефункциональные**

- Bridge_loop остаётся как сейчас (default behavior `wd` без `--exec`).
- Не ломать существующие тесты `wiredesk-term`.
- Reuse без копипасты: вынести handshake / transport-split-helper / heartbeat в module-private helpers (если ещё не вынесены).

## Acceptance criteria

- **AC1.** `wd --exec "echo hello"` → stdout: `hello`, exit 0.
- **AC2.** `wd --exec "exit 7"` → exit 7, stdout empty.
- **AC2a.** `wd --exec "Get-Item /nonexistent/path"` (PS terminating error) → exit 1 (через `try/catch` wrapper в `format_command`), **не** timeout 124. Stderr-style error message в stdout.
- **AC3.** `wd --exec --ssh prod-mup "docker ps"` → stdout содержит docker ps таблицу **без** Welcome-banner'а Ubuntu и MOTD, exit 0.
- **AC4.** `wd --exec --ssh prod-mup "docker logs nonexistent-container"` → stderr-style error в stdout (host шлёт всё одним `ShellOutput`'ом, MVP не разделяет), exit ≠ 0.
- **AC5.** `wd --exec --timeout 2 "Start-Sleep 5"` → exit 124, no hang.
- **AC6.** Два последовательных `wd --exec` без open GUI — оба завершаются, host slot free'ится между ними (как уже работает в bridge_loop через `ShellClose + Disconnect`).
- **AC7.** Stdout чистый — ни PS prompt'а, ни echo введённой команды, ни sentinel-строки. Только то, что команда вернула.
- **AC8.** `wd --exec` совместимый с Bash-tool как drop-in replacement: `Bash("wd --exec --ssh prod-mup 'docker ps' | head -20")` работает как нативный shell call.

## Тестирование

**Unit-тесты** (обязательны для каждой task'и плана):

- Pure helpers с MockTransport pair и unit-level table tests:
  - `is_powershell_prompt(line) -> bool` — regex `^PS\s+[A-Z]:.*?>\s*$`. Test cases: `PS C:\> ` true, `PS C:\Users\User> ` true, `bash$ ` false, `> ` false.
  - `is_remote_prompt(line) -> bool` — соответствует Starship `➜ ` и стандартному bash `\$\s*$`/`#\s*$`. Test cases: `➜ ` true, `user@host:~$ ` true, `karlovpg in 🌐 knd02 in ~` false (это пред-строка Starship'а), `➜ /tmp` true (with whitespace tolerance).
  - `format_command(uuid, shell, cmd) -> String`:
    - Для PS (host): `try { <cmd> } catch { $LASTEXITCODE = 1 }; "__WD_DONE_<uuid>__$LASTEXITCODE"\r`. **try/catch wrapper обязателен** — если `<cmd>` роняет PS terminating error без try, sentinel-эхо не выполнится → wd зависнет до timeout(124) вместо корректного exit 1.
    - Для bash post-ssh: `<cmd>; echo "__WD_DONE_<uuid>__$?"\r` (bash continues после non-zero exit, sentinel выполнится в любом случае).
    - Test cases в unit-тестах: «команда роняет PS terminating error → парсится `__WD_DONE_xxx__1`, не timeout 124», «команда выходит non-zero (`exit 7`) → парсится `__WD_DONE_xxx__7`», «успешная команда → `__WD_DONE_xxx__0`».
  - `parse_sentinel(line, uuid) -> Option<i32>` — anchored regex `^__WD_DONE_<uuid>__(\d+)\s*$`. Stdin echo с literal `$LASTEXITCODE` не match'ит. Two/three-digit exit codes работают.
  - `clean_stdout(buffer, prompt_idx, sentinel_idx, echoed_cmd_line) -> String` — отрезает [0..prompt_idx], [echoed_cmd_line], [sentinel_line]. Test: input "MOTD\n...PS C:\>\necho cmd\nactual output\n__WD_DONE_xxx__0\n" → "actual output".
- Integration с MockTransport pair:
  - Симуляция полного potok'а: handshake → ShellOpen → host шлёт PS prompt → wd шлёт command+sentinel → host шлёт echo+output+sentinel → wd выходит.
  - Timeout: peer не шлёт sentinel → wd выходит 124.
  - Disconnect mid-command → graceful exit 125.
  - SSH path: host шлёт PS prompt → wd шлёт `ssh -tt prod`\r → host шлёт SSH-handshake-mock → remote prompt → wd шлёт command → host echoes → output → sentinel.

**Live-тесты** (after build):
- AC1-AC8 на реальном hardware с CH340 + prod-mup.
- Latency: `time wd --exec "Get-Date"` ≤ ~2 сек (handshake + ShellOpen + первый prompt + cmd + sentinel).

## Риски

- **PS prompt regex может промахнуться** на кастомизированных prompt'ах (Oh-My-Posh / Starship на Win). Mitigation: дать `--prompt-regex` flag для override; default — стандартный `PS X:\>`.
- **Long-running команды** — heartbeat-thread активен, host не disconnect'ит. Но если команда идёт >30s default, wd exit'ит 124. User может `--timeout 300`.
- **`ssh -tt` echoes stdin к нам** — означает что после посылки `cmd; sentinel\r` мы увидим сначала echo, потом result, потом sentinel. `clean_stdout` отрезает echo'ed line. Тестовый case с literal `__WD_DONE_<uuid>__$?` (echo) **не** match'ит sentinel regex (`\d+` требует digits, не `$?`).
- **Concurrent sessions** — same as сейчас в `wd`: один процесс на serial. Если GUI открыт — `wd --exec` fail с busy. Acceptable.
- **Output binary blob с встроенной last-byte 0x0A** — sentinel match'ится по строке, binary blobs не line-aligned, не trigger'ят false-positive. UUID per call устраняет даже paranoid case.

## Первые шаги

1. Добавить crate `clap` already есть. Добавить `uuid = "1"` в `wiredesk-term/Cargo.toml`.
2. Pure helpers + unit tests: `is_powershell_prompt`, `is_remote_prompt`, `format_command`, `parse_sentinel`, `clean_stdout`. Каждая = table-driven test.
3. `Args` extension: `#[arg(long)] exec: Option<String>`, `#[arg(long)] ssh: Option<String>`, `#[arg(long, default_value = "30")] timeout: u64`.
4. В `run()` — branch: если `exec.is_some()` → `run_oneshot(args, transport, reader)` без `enable_raw_mode`, иначе текущий `bridge_loop`.
5. `run_oneshot`: handshake → ShellOpen → wait_for_prompt(is_powershell_prompt, timeout) → if `--ssh`: send `ssh -tt ALIAS\r`, wait_for_prompt(is_remote_prompt, timeout) → format_command → send → accumulate output → find sentinel → clean_stdout → print → ShellClose + Disconnect → exit(code).
6. README section: usage examples, ControlMaster setup для prod-mup в `~/.ssh/config` на host'е.

## Что НЕ входит в scope

- **stderr separation.** Всё в stdout. Если нужно — нужны host-side изменения (Message::ShellStderr).
- **Persistent SSH session между разными `wd --exec` вызовами.** Каждый вызов — fresh ssh handshake (~1 сек на prod-mup). Persistent — через **OpenSSH ControlMaster** в `~/.ssh/config` host'а (mulitplexed connection, sub-second). Это вне нашего кода — стандартный ssh feature.
- **Persistent state PS shell** между вызовами (cwd / env). Каждый `wd --exec` — fresh PS process. Нужно — daemon (Variant B follow-up если ControlMaster не покроет).
- **JSON output mode** (`--json`).
- **`--shell` flag** (выбор bash/cmd как target host shell). Default PowerShell — host's default. Если будет нужно cmd — добавим позже.
- **ConPTY refactor** на host'е — отдельный follow-up (см. memory `project_conpty_followup.md`). После него `wd --exec` нужно учить strip'ать ANSI escape codes из ShellOutput'а (поскольку output станет TTY-styled).

## Сложность

**low-medium** — расширение существующего `wiredesk-term`, ~200 строк нового кода + ~100 строк тестов. Никаких новых protocol messages. Никаких изменений в host'е. Главное unknown — корректность prompt regex'а для конкретных пользовательских настроек PS / Starship; mitigation — `--prompt-regex` override.
