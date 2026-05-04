# WireDesk Architecture

Внутренние детали реализации. CLAUDE.md содержит обзор и ссылку сюда — здесь полный технический разбор: module maps, threading, протокол, clipboard sync, keyboard hijack, shell-over-serial, дизайн-решения.

---

## Module maps

### Host (`apps/wiredesk-host/src/`)

```
main.rs                — entry, single-instance, init_logging, config merge,
                          run_windows() / run_dev_loop() split
config.rs              — HostConfig, load/save TOML, merge_args via ArgMatches
logging.rs             — tracing-appender rolling daily + LogTracer bridge,
                          install_panic_hook() (separate from init_logging
                          to avoid leaking global hook in tests)
session.rs             — Session<T,I> state machine, current_state(), client_name()
session_thread.rs      — spawn() generic over injector cfg, SessionStatus enum,
                          derive_status() pure helper
ui/
  mod.rs               — module routing, dead_code allows for non-Windows
  format.rs            — pure validators (validate_baud/port/dimension),
                          format_mac_launch_command, status_color,
                          detect_ch340_port + DetectResult enum (VID 0x1A86)
  icons.rs             — shared embedded PNG bytes (ICON_GREEN/YELLOW/GRAY_BYTES)
                          + app-icon.ico bytes — used by tray + settings status
  settings_window.rs   — #[cfg(windows)] nwg builder UI grouped into Frame
                          blocks (Connection / Display / System), bottom
                          button-bar (Save & Restart + Save primary), Detect
                          button, status ImageFrame, runtime icon load via
                          nwg::Icon::builder().source_bin (no PE-resource
                          path due to mingw fallback)
  tray.rs              — #[cfg(windows)] TrayUi using icons.rs constants, popup menu
  autostart.rs         — HKCU\...\Run via windows::Win32::System::Registry
                          (own implementation, no auto-launch crate)
  single_instance.rs   — CreateMutexW("WireDeskHostSingleton") + drop=close,
                          try_acquire_with_retry (5×100ms) для Save & Restart race
  status_bridge.rs     — session_thread → nwg::Notice via Arc<Mutex<SessionStatus>>
```

### Client (`apps/wiredesk-client/src/`)

Дополнительно к `keyboard_tap.rs` / `keymap.rs` / `clipboard.rs`:

```
monitor.rs             — NSScreen FFI wrapper (objc2-app-kit) под cfg(macos),
                          MonitorInfo { index, name, frame, size },
                          list_monitors(), resolve_target_monitor(preferred, &monitors)
                          (pure helper для fullscreen orchestration)
```

---

## Threading (client)

Клиент делит serial-порт на два независимых хэндла через `Transport::try_clone()`:

- **writer_thread** — единственный отправитель. Блокируется на `outgoing_rx.recv_timeout(2s)`. Пакет → отправляет немедленно. Таймаут → шлёт Heartbeat. UI кладёт пакеты в канал и не ждёт.
- **reader_thread** — единственный получатель. recv() в цикле, диспатчит на `events_tx` для UI. Также держит `IncomingClipboard` для сборки входящих ClipChunks.
- **clipboard poll thread** — раз в 500мс читает Mac clipboard, при изменении отправляет ClipOffer + ClipChunks через тот же `outgoing_tx`.
- **keyboard tap thread** (только macOS) — отдельный CFRunLoop, владеет CGEventTap. Подробнее в секции «Keyboard hijack».

Латенси UI→провод ~µs (только время записи в UART, ~100µs).

## Data flow

```
Client (macOS)                          Host (Windows)
  egui captures input                     Session::tick() loop
  → InputMapper.send_*(outgoing_tx)         → recv Packet via serial
  → outgoing_tx (mpsc channel)              → handle_packet
  → writer_thread.send()                    → InputInjector::key_down/mouse_move/...
  → SerialTransport::send()                 → Win32 SendInput API
```

---

## Protocol (`wiredesk-protocol`)

Packet: `[magic "WD"][type][flags][seq:u16][len:u16][payload][crc16]`, COBS-framed over serial.

20 message types: HELLO/HELLO_ACK (handshake with screen resolution), 5 input types (mouse move/button/scroll, key down/up), 3 clipboard types (offer/chunk/ack) + ClipDecline (0x23), heartbeat/error/disconnect, 7 shell types (open/input/output/close/exit + ShellOpenPty + PtyResize).

Ввод — fire-and-forget. Clipboard — fire-and-forget chunks (256 байт), reassembly по `index`. ACK-сообщения определены в протоколе, но в текущей реализации не используются (CRC на пакетном уровне даёт достаточную защиту для MVP). Heartbeat: каждые 2 сек, timeout 6 сек idle / 30 сек busy (3 пропущенных).

**MAX_PAYLOAD = 4096** (bumped 512→4096 в `feat/wd-exec-fixes` для типичных ES `_search` через `wd --exec`). Matched bump SerialTransport frame limit `MAX_FRAME_SIZE = 8192` в `crates/wiredesk-transport/src/serial.rs` — иначе 4 KB packet silently дискар'дился receiver'ом.

---

## Clipboard sync

Симметрично на обеих сторонах:
- Polling раз в 500мс (Mac — 200ms после Whispr-fix). **Probing text + image в одном tick'е** — OS clipboard может содержать оба формата (typical Cmd+C на rich content). Каждый dedup'ится в свой slot независимо.
- **Два формата:** `FORMAT_TEXT_UTF8 = 0` (UTF-8 строка, лимит 256 KB), `FORMAT_PNG_IMAGE = 1` (PNG-encoded RGBA, лимит `MAX_IMAGE_BYTES = 1 MB` после encode). Константы — в `wiredesk-protocol::message`.
- ClipOffer { format, total_len } + N×ClipChunk { index, data ≤ 256B }. Сборка через `BTreeMap<u16, Vec<u8>>` — устойчиво к out-of-order.
- **Loop avoidance — `LastSeen` struct** с раздельными слотами per-type: `text_history: VecDeque<u64>` (LRU, последние 4 hash'а), `image: Option<u64>`, `oversize_image: Option<u64>`. Mac `Arc<Mutex<LastSeen>>`, Host plain field. Single-slot enum (старый `LastKind`) ломался: (a) text-write erased image hash → loop, (b) Whispr-style inject pattern (save→write→paste→restore) re-shлёт `prev` после каждого цикла. LRU text-history покрывает Whispr `prev → new → prev → newer → prev` без resend'ов. Хэш для image считается **от RGBA bytes**, не от encoded PNG: round-trip arboard PNG↔RGBA нестабилен (NSPasteboard TIFF round-trip меняет байты).
- **Pre-stamp on startup.** При создании poll thread (Mac) и `ClipboardSync::with_counters` (Host) текущий clipboard читается, hash стампится в `LastSeen` БЕЗ отправки. Без этого каждый restart re-uploads то что юзер оставил в clipboard от прошлой сессии.
- **Image encode/decode:** `image 0.25` (`default-features=false, features=["png"]`), helpers `encode_rgba_to_png` / `decode_png_to_rgba` дублируются на обеих сторонах. Encode в poll thread (~50–150 ms). Decode имеет `Limits::max_alloc = 64 MB` + post-decode проверка `(w*h*4) ≤ 64 MB` (PNG-bomb защита: палеточный 8K×8K decode'ит в 256 MB RGBA).
- **Settings → Clipboard panel** (Mac UI): 4 независимых runtime-toggle через `Arc<AtomicBool>` — `send_images`, `receive_images`, `send_text`, `receive_text`. Без рестарта. Полезно для apps вроде Whispr Flow / Maccy которые часто пишут в clipboard.
- **Status UI на Mac:** (1) `format_progress("Sending clipboard", cur, total)` рендерится как `egui::ProgressBar` с inline текстом — в chrome panel И в capture banner (для fullscreen где menu bar скрыт macOS). (2) `NSStatusItem` справа от часов через `objc2-app-kit::NSStatusBar::systemStatusBar` + `dispatch_async_f` на main queue. Idle: «W», active: «↑43%» / «↓67%». Click handler — TODO (custom NSObject subclass через `objc2::declare_class!` нужен).
- **Tray balloon notification (Win)** при oversize: `SessionStatus::Notification(String)` slot в `StatusState` — отдельно от persistent `Connected/Waiting/Disconnected`, не overwrites tray icon color и settings status row. Surface через `nwg::TrayNotification::show(msg, title, WARNING_ICON|LARGE_ICON, None)`.
- **Tray double-click → Settings (Win):** nwg 1.0.13 не имеет нативного double-click event для tray. Workaround: `OnMousePress(MousePressLeftUp)` + `Cell<Option<Instant>>` tracking previous up — два up в окне 500ms = double-click.
- **Pass-through modifier-only events в capture mode.** `CGEventType::FlagsChanged` callback теперь возвращает `Keep` (не `Drop`) после forward'а scancodes на Host. Это позволяет Whispr Flow / push-to-talk dictation apps trigger'нуться на Ctrl+Option, при этом letter keys всё ещё intercept'ятся через `KeyDown` → Drop.
- **Edge case: interleaved offers.** Новый ClipOffer пришёл во время незавершённой reassembly → `log::warn!("incoming offer aborted previous reassembly")` + `received.clear()` + reset counters.
- **Edge case: peer disconnect.** При `TransportEvent::Disconnected` (Mac) или потере связи (Host) — `IncomingClipboard::reset()` обнуляет expected_len / format / received / counters. Sender'ская `last_kind` сохраняется (после reconnect не нужно повторно слать тот же контент).
- **Edge case: oversized peer offer.** `on_offer` отвергает `total_len > MAX_*` ДО reassembly arming — без этого peer мог запросить 4GB allocation в `Vec::with_capacity` через корраптный/враждебный offer.
- **Edge case: non-contiguous chunks / length mismatch.** `commit()` проверяет (a) chunk indices contiguous 0..N, (b) reassembled `buf.len() == expected_len`. Иначе log warn + reset. Защита от silent corruption.
- **TransferOverlay (Win) отключён** в main.rs. Topmost popup-window даже invisible забирал z-order у других окон (Total Commander не активировался кликом). Прогресс на host'е — только в логе + balloon notification на oversize.
- Mac side: `apps/wiredesk-client/src/clipboard.rs`. Host side: `apps/wiredesk-host/src/clipboard.rs`. Не вынесено в общий crate — duplication приемлема.

---

## Keyboard hijack (macOS)

Чтобы перехватывать системные шорткаты типа `Cmd+Space` (которые macOS интерпретирует раньше уровня приложения) — используется `CGEventTap` на сессионном уровне. Без этого `egui` видит только клавиши, которые macOS не успел обработать.

**Permission gate.** Tap требует Accessibility permission (System Settings → Privacy & Security → Accessibility). Без неё tap создаётся, но молча не срабатывает. На старте `keyboard_tap::start()` вызывает `AXIsProcessTrustedWithOptions({prompt: false})` и, если permission нет, возвращает no-op handle. UI показывает экран с инструкцией. После grant'а **обязательно перезапустить процесс** — tap-поток создаётся один раз при `start()`.

**Threading.**
- Отдельный thread `wiredesk-keyboard-tap` с CFRunLoop.
- `CGEventTapCreate` с маской `KeyDown | KeyUp | FlagsChanged | TapDisabledByTimeout | TapDisabledByUserInput`.
- Callback быстрый: проверяет `enabled: Arc<AtomicBool>` + `passive: Arc<AtomicBool>`. Три состояния: ACTIVE (intercept всё, Drop на Host) / PASSIVE (только Cmd+Esc и Cmd+Enter — отправляем `TapEvent::EngageCapture`/`ToggleFullscreen` в UI и Drop, остальное Keep) / IDLE (`enabled=false`, всё Keep — macOS видит ввод как обычно).
- `CGEventFlags` приходят как полный bitmask текущего состояния модификаторов (не diff). `prev_flags: Arc<AtomicU64>` хранит прошлый bitmask, `cg_flag_change_to_scancodes(cur, prev)` выдаёт список (scancode, pressed) для отправки.
- При `kCGEventTapDisabled*` — re-enable через сохранённый `CFMachPortRef` (как `usize` через `Arc<AtomicUsize>`, потому что raw pointer нужно протащить в callback closure).
- Shutdown: `CFRunLoop` сохранён в `Arc<Mutex<Option<CFRunLoop>>>`. `Drop for TapHandle` вызывает `rl.stop()`, join потока с timeout 1с.

**Focus-gating + passive mode.** `WireDeskApp::sync_tap_to_focus()` бежит в каждом `update()`, читает `ctx.input(|i| i.viewport().focused)` и синхронизирует через 3-стейт machine:
- `(focused=true, capturing=true)` → `enable()` (ACTIVE) — всё на Host;
- `(focused=true, capturing=false)` → `enable_passive()` (PASSIVE) — ловим только Cmd+Esc для engage и Cmd+Enter для fullscreen, остальное Mac обрабатывает сам. Без passive AppKit перехватывает Cmd+Esc как Cancel-button accelerator до того как egui увидит keypress, и engage capture хоткеем не работает.
- `(focused=false, _)` → `disable()` (IDLE) — sticky-cleanup, отправляются KeyUp для нажатых модификаторов, чтобы Host не остался с залипшим Ctrl.

**Cmd → Ctrl mapping.** Cmd OR Ctrl на Mac → Ctrl на Windows. Bit 20 (Command) или bit 18 (Control) в CGEventFlags — оба маппятся на единственный Win scancode 0x1D. Если оба нажаты одновременно — не дублируем press/release (см. тесты в `keymap::cg_flag_change_to_scancodes`).

**Локальные хоткеи.**
| Combo | Mac VK | Действие |
|-------|--------|----------|
| `Cmd+Esc` | `0x35` | Toggle capture (вкл/выкл) |
| `Cmd+Enter` | `0x24` | Toggle fullscreen |

Оба перехватываются tap-callback'ом (через `is_release_capture` и `is_cmd_enter`) и не форвардятся на Host. Также детектятся в egui-input для случая когда tap не запущен (out-of-capture, пользователь нажал Cmd+Enter ещё до Capture).

**Файлы.** `apps/wiredesk-client/src/keyboard_tap.rs` (~440 строк), `keymap.rs::cgkeycode_to_scancode`, `keymap.rs::cg_flag_change_to_scancodes`. Deps: `core-graphics 0.25`, `core-foundation 0.10`, `accessibility-sys 0.1` под `cfg(target_os = "macos")`.

---

## Shell-over-serial

Опциональный канал терминала на том же serial. **Per-session pipe-vs-PTY выбор** через opcode-discriminator (бинарный protocol, не serde — расширение payload'а 0x40 байтом флага молча корраптило бы парсеров старых hosts'ов, поэтому новый opcode чище):

- `ShellOpen = 0x40 { shell }` (pipe-mode) — Host спавнит подпроцесс с `Stdio::piped()`. На Win добавлен `CREATE_NO_WINDOW` чтобы child не показывал console window. Используется `wd --exec` (sentinel detection требует чистого stdout) и GUI shell-panel (egui без ANSI parser'а). Все платформы.
- `ShellOpenPty = 0x45 { shell, cols, rows }` (PTY-mode) — Host'овский `ShellProcess::spawn(_, Some((cols, rows)))` через `portable-pty` (ConPTY на Win11 1809+; gated `cfg(target_os = "windows")` потому что forkpty конфликтует с parallel cargo test runner на macOS dev). Используется interactive `wd`. Vim/htop/ssh без `-tt`/PSReadLine стрелки + Tab autocomplete работают как в нативном ssh. Нативный CRLF-emit (no LF→CRLF translation на client'е).
- `PtyResize = 0x46 { cols, rows }` — runtime resize. wiredesk-term'овский `resize_poll_thread` каждые 500 ms через `crossterm::terminal::size()` (cadence — fast enough для vim mid-resize, slow enough не саратурировать 11 KB/s wire). Pure helper `compute_resize_packet(prev, cur)` — emit only on diff (или first tick).
- `ShellInput { data }`, `ShellOutput { data }`, `ShellClose`, `ShellExit { code }` — общие для обоих режимов.
- Re-Hello host kill'ает leftover shell-slot (ShellOpenPty не bounce'ает на "shell already open"). Term шлёт `Disconnect` после `ShellClose` чтобы host не ждал heartbeat timeout.

`ShellProcess` в host'е — `enum Backend { Pipe { child: Child }, #[cfg(target_os = "windows")] Pty { child: Box<dyn portable_pty::Child + Send + Sync>, master: Box<dyn MasterPty + Send> } }`. На Mac dev все pty-связанные tests pаботают cfg(not(windows)) и проверяют возврат `Err("PTY-mode shell is only supported on Windows host")`. AC1-AC4 (vim, ssh без -tt, PSReadLine, git editor) проверяются live на Win11 — Mac unit tests pipe-mode regression тогда есть.

**Два frontend'а к этому каналу:**

- **GUI shell-panel** в `wiredesk-client` (Settings collapsing → Terminal). Командная строка с `id_salt("shell_input")`. После Enter автоматический `request_focus()` чтобы поле не теряло фокус (без этого — `lost_focus()`-pattern, и пользователь должен кликать перед каждой следующей командой). При первом open `shell_just_opened` flag запрашивает focus на следующем frame'е.

- **`wiredesk-term`** CLI (отдельный бинарь). Raw-mode bridge для Ghostty/iTerm. Serial-port расщеплён через `Transport::try_clone()` на independent reader/writer handles — reader thread больше не держит mutex на blocking recv'е, stdin keystrokes доходят без latency. **Четыре потока** в interactive mode: reader (owns reader handle), main (stdin → writer.lock), heartbeat (`Heartbeat` каждые 2s), **`resize_poll_thread`** (каждые 500ms `crossterm::terminal::size()` → `PtyResize` on diff). Все держат общий `stop: AtomicBool`.
  - **Pass-through raw на client'е** (PTY-mode на host'овой стороне): host'овский ConPTY echoes/edits/colors сам через PSReadLine, поэтому client — dumb pipe. Stdin chunks forward'ятся byte-for-byte как `ShellInput`. Единственный intercept — `ESCAPE_BYTE = 0x1D` (Ctrl+]) для local quit. Никакого local echo, line buffering, BS-erase, CRLF-translate'а — host TTY делает всё сам.
  - **Pure helper `build_shell_open_message(shell, exec, cols, rows)`** выбирает `ShellOpen` (exec=true) или `ShellOpenPty` (exec=false). Тестируется без spinning serial.
  - Banner после handshake — `format_connected_banner(host_name, w, h)` (pure helper, тесты).
  - Hotkey cheatsheet печатается после banner'а: `Ctrl+]` exit (telnet/nc convention — позволяет forward'ить Ctrl+C/Ctrl+D на host'а).
  - **`--exec COMMAND` non-interactive mode** (`run_oneshot` рядом с `bridge_loop`, feat/wd-exec / PR #9). `--exec` — boolean flag, COMMAND — positional argument (`wd --exec "Get-ChildItem"` / `wd --exec --ssh prod-mup "docker ps"`). Опциональный `--ssh ALIAS` для chain'а через `ssh -tt`, опциональный `--timeout N` (default 30, exit 124 на timeout как `timeout(1)`). Не enable raw mode, не открывает stdin. Линейный protocol с минимальной state machine `OneShotState::{AwaitingRemotePrompt, AwaitingSentinel}` — PS-only сразу в AwaitingSentinel (PS не emit'ит prompt в pipe mode); SSH идёт AwaitingRemotePrompt → AwaitingSentinel (обязательно ждём prompt remote shell'а перед посылкой payload, иначе PS .NET StreamReader read-ahead запирает payload в PS-памяти и до ssh subprocess'а он не доходит). Pure helpers: `format_command` (PS: `$LASTEXITCODE=0; $ErrorActionPreference='Stop'; try { <cmd> } catch { $LASTEXITCODE=1 }; "__WD_DONE_<uuid>__$LASTEXITCODE"\n` — pre-init `$LASTEXITCODE` для cmdlet-success cases, EAP=Stop ловит non-terminating errors. Bash post-ssh: `echo __WD_READY_<uuid>__; <cmd>; echo "__WD_DONE_<uuid>__$?"\n` — READY-marker как нижняя граница для `clean_stdout`'s slice'а MOTD/banner), `parse_sentinel` (anchored prefix-strip + `parse::<i32>` отсеивает stdin-echo с literal `$LASTEXITCODE`/`$?`), `parse_ready` (matches только expanded form, не `echo __WD_READY_…`), `strip_ansi` (char-aware; CSI/OSC/simple escape; нужен для match'а Starship-prompt'ов вида `➜ \x1b[K\x1b[?2004h`), `clean_stdout` (lower=READY-line+1 если есть, иначе last prompt+1; upper=sentinel; внутри slice'а filter'ует unexpanded sentinel + READY echo line'ы). Heartbeat thread активен — host's idle timeout не разорвёт connection во время slow-running команды. Persistent SSH — через OpenSSH ControlMaster в `~/.ssh/config` host'а (вне нашего кода). На timeout (exit 124) `run_oneshot` печатает в stderr `last bytes received: "..."` (last 256 байт wire-buffer'а через pure-helper `format_timeout_diagnostic`) — диагностика где залип (mid-MOTD / после READY-marker / mid-command output) без phase-based machinery. Подробный usage guide для AI-агентов: `docs/wd-exec-usage.md`. Поведенческие гочи PS pipe-mode — отдельная memory `feedback_ps_pipe_exec_quirks.md`.

- GUI и CLI **взаимоисключающие** — оба открывают serial-порт. Multiplex-daemon вне scope MVP.

---

## Key design decisions

- **Scancodes, not VK codes** — ввод как hardware scancodes, работает независимо от раскладки Host (включая кириллицу).
- **Extended scancodes** (0xE0xx) — в SendInput требуют `KEYEVENTF_EXTENDEDKEY` flag.
- **Cmd → Ctrl** mapping в `egui_modifiers_to_u8` и `cg_flag_change_to_scancodes`. Win-key combos (Win+Space, Win+L) — через CGEventTap они теперь работают напрямую (Cmd+Space на Mac → Win+Space на Host), но кнопки в UI оставлены как fallback для случая когда permission ещё не granted.
- **Synthetic vs physical events.** Tap callback читает `EventField::EVENT_SOURCE_STATE_ID` (1=HIDSystemState→physical, 0=CombinedSessionState→synthetic). Karabiner-Elements remap'ит на HID-уровне → physical events приходят post-Karabiner. Synthetic CGEventPost (Whispr Flow, TextExpander) bypass'ает Karabiner и несёт литеральный modifier intent. Swap toggle применяется только к physical; synthetic forward'ится со standard mapping'ом + ad-hoc modifier wrap (synthetic'е приходят без preceding FlagsChanged, иначе Host видит orphan letter scancodes).
- **Karabiner ⌥/⌘ swap toggle.** `swap_option_command: bool` config поднимает `cg_flag_change_to_scancodes_swapped` для FlagsChanged forward'а и `disable()` cleanup'а — Cmd flag → Alt scancode, Option flag → Ctrl scancode. Hotkey detection (`is_cmd_enter`/`is_release_capture`) при swap=true принимает либо `CG_FLAG_COMMAND`, либо `CG_FLAG_ALT` (но не оба) — Cmd+Esc/Cmd+Enter работают на той же физической кнопке независимо от того remap'нута ли клавиатура.
- **Synthetic Cmd+V dispatcher** (`apps/wiredesk-client/src/main.rs`). Whispr Flow's Cmd+V опережает Mac→Host clipboard sync — Host paste'ит prev. Решение: tap не emit'ит synthetic Cmd+V напрямую — упаковывает в `SyntheticCombo` (`Vec<Packet>` из modifier-press + key-press) и push'ит в `synth_tx`. Dispatcher thread ждёт пока poll thread сбросит `outgoing_text_in_flight=false` (max 4s), плюс grace 400ms (для Host commit), потом emit'ит. Дополнительно tap kicks poll через `poll_kick_tx` mpsc channel — poll wakes immediately и читает clipboard, не дожидаясь sleep'а. CLIP_POLL_INTERVAL 200ms.
- **Adaptive heartbeat timeout (host)**. `Session::heartbeat_timeout()` возвращает 30s когда `clipboard.transfer_in_flight()` (есть active reassembly или непустой `pending_outbox`), иначе 6s. CH340 bidirectional saturation топит heartbeats peer'а во время image transfer'а и строгий 6s давал false-positive disconnect'ы.
- **115200 baud, не 921600** — на дешёвых CH340 с Dupont-проводами 921600 даёт single-bit corruption (видели "bad magic" с XOR 0x80). 115200 надёжно. Bandwidth budget ~11 KB/s, реально ~1 KB/s для ввода + редкие всплески под clipboard/shell.
- **Leading 0x00 + drain on open** — серьёзный фикс для startup transient: при открытии порта CH340 выпускает мусорный байт, который иначе склеивается с первым кадром. Решено в `SerialTransport::send` (ведущий 0x00) + `SerialTransport::open` (drain OS буфера).
- **MockTransport** — mpsc, протокол-тесты без железа. `try_clone` для Mock возвращает ошибку (не нужно в тестах).
- **Aspect ratio correction** в `InputMapper::normalize_mouse` — letterbox/pillarbox для разной геометрии окна и Host.
- **Cancel-кнопки на progress bars** (UI helper `render_progress_row`, Mac). `outgoing_cancel`/`incoming_cancel: Arc<AtomicBool>` shared с writer/reader threads. Writer drop'ает queued ClipOffer/ClipChunk без записи на провод и self-arms flag после non-clip packet или timeout (queue empty). Reader на первом ClipChunk при cancel=true делает `incoming_clip.reset()` + drop, flag clear'ится на следующем ClipOffer. Лог компактный — один summary INFO в start, один в end с counter'ом (per-chunk даёт 700+ строк за 180 KB image cancel). Без protocol message: Host видит partial offer, self-correct'ится на следующем ClipOffer'е.
- **Self-relaunch helper** `apps/wiredesk-client/src/restart.rs`. На macOS spawn'ит `open -n WireDesk.app` если бинарь внутри bundle, иначе spawn'ит `current_exe`. Затем `std::process::exit(0)`. Используется Save & Restart кнопкой в Settings (Mac). Аналогичный pattern на Win — Restart entry в tray menu делает `Command::new(current_exe).spawn() + nwg::stop_thread_dispatch()`.
- **Second-instance показывает Settings** существующего host'а (вместо MessageBox + exit). Win32 named auto-reset event `WireDeskHostShowSettings` (см. `ui::single_instance::create_show_settings_event` / `signal_show_settings`). Первый процесс CreateEvent + spawn wait-thread, второй — OpenEvent + SetEvent + exit. Wait-thread поднимает `show_settings_pending: AtomicBool` и **piggybacks** на existing status-bridge `nwg::Notice` через `notice.sender().notice()` — nwg 1.0.13 panic'ит на втором Notice anywhere в дереве (см. memory `feedback_nwg_gotchas.md` #4). OnNotice handler в начале arm'а делает `swap → false` на pending flag и поднимает Settings.
- **`Message::ClipDecline { format }` (proto type 0x23)** — receiver просит sender'а abandon transfer. `IncomingClipboard::on_offer` возвращает `Some(Message::ClipDecline)` когда rejectит из-за settings toggle (`receive_text=false` / `receive_images=false`); reader thread форвардит decline через outgoing_tx. Sender (host: `ClipboardSync::cancel_outgoing()` дренит `pending_outbox`; client: set `outgoing_cancel: AtomicBool` который writer thread наблюдает) — drop'ает все pending chunks. Без этого toggle-off вызывал ~75 sec wire-saturation на 1 MB image (host всё равно шлёт chunks), что starved'ил TX (mouse / heartbeats) → false-positive heartbeat timeout disconnect.
- **CREATE_NO_WINDOW для shell child** в `apps/wiredesk-host/src/shell.rs`. Win-host с `windows_subsystem = "windows"` — без console; default child создаёт свою console window. Flag (0x0800_0000) на `Command::creation_flags` подавляет это так что ShellOpen не показывает PowerShell window на host'ской HDMI-capture.
- **Embed app icon в .exe** через `winresource` build-dependency. `apps/wiredesk-host/build.rs` gating через `HOST` env triple (only Windows host has `rc.exe`/`windres`) — Mac dev cross-checks compile clean но без icon resource section. Производственная сборка на Win показывает WireDesk-иконку в taskbar/Alt+Tab/Explorer.
