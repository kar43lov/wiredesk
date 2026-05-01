# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

WireDesk — утилита для удалённого управления мышью, клавиатурой и clipboard на Windows-машине через serial-соединение (без сети). Видео — отдельно через HDMI capture card.

Контекст: на Host (Windows 11) стоит ПО "Континент", которое блокирует все сетевые интерфейсы. Serial (COM-порт) не блокируется.

**Статус:** MVP работает end-to-end. Соединение, мышь, клавиатура (включая кириллицу), переключение языка (Win+Space), двунаправленный буфер обмена — проверено живьём. 71 тест проходит.

## Build & Test

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo build --release --workspace
```

Host компилируется и на macOS (с MockInjector), и на Windows (`WindowsInjector` за `cfg(target_os = "windows")` через crate `windows`). На macOS реальный SendInput не вызывается — для dev-цикла без Windows это нормально.

## Run

Дефолты подобраны под solo-сетап (single user): COM3 на Windows, `/dev/cu.usbserial-120` на Mac, baud 115200, разрешение 2560×1440. Запуск без аргументов:

```bash
# Host (Windows)
.\target\release\wiredesk-host.exe

# Client (macOS) — GUI с capture-окном, кнопками Ctrl+Alt+Del / Win key / Lang (Win+Space)
./target/release/wiredesk-client

# Terminal-only клиент (raw-mode CLI bridge для Ghostty/iTerm), Ctrl+] для выхода
./target/release/wiredesk-term
```

Все флаги переопределяемы (`--port`, `--baud`, `--width`, `--height`, `--shell`).

`wiredesk-client` и `wiredesk-term` взаимоисключающие — оба открывают serial-порт.

## Architecture

Rust workspace с 6 crate:

```
crates/
  wiredesk-core       — WireDeskError, типы (Resolution, MouseButton, Modifiers)
  wiredesk-protocol   — бинарный протокол: Packet, Message (18 типов), COBS framing, CRC-16
  wiredesk-transport  — trait Transport, SerialTransport, MockTransport
apps/
  wiredesk-host       — Windows console agent: Session + InputInjector + ShellProcess + ClipboardSync
  wiredesk-client     — macOS egui app: capture-окно + InputMapper + clipboard poll thread
  wiredesk-term       — macOS CLI: raw-mode terminal bridge для Ghostty/iTerm (только shell)
```

### Threading (client)

Клиент делит serial-порт на два независимых хэндла через `Transport::try_clone()`:

- **writer_thread** — единственный отправитель. Блокируется на `outgoing_rx.recv_timeout(2s)`. Пакет → отправляет немедленно. Таймаут → шлёт Heartbeat. UI кладёт пакеты в канал и не ждёт.
- **reader_thread** — единственный получатель. recv() в цикле, диспатчит на `events_tx` для UI. Также держит `IncomingClipboard` для сборки входящих ClipChunks.
- **clipboard poll thread** — раз в 500мс читает Mac clipboard, при изменении отправляет ClipOffer + ClipChunks через тот же `outgoing_tx`.

Латенси UI→провод ~µs (только время записи в UART, ~100µs).

### Data flow

```
Client (macOS)                          Host (Windows)
  egui captures input                     Session::tick() loop
  → InputMapper.send_*(outgoing_tx)         → recv Packet via serial
  → outgoing_tx (mpsc channel)              → handle_packet
  → writer_thread.send()                    → InputInjector::key_down/mouse_move/...
  → SerialTransport::send()                 → Win32 SendInput API
```

### Protocol (wiredesk-protocol)

Packet: `[magic "WD"][type][flags][seq:u16][len:u16][payload][crc16]`, COBS-framed over serial.

18 message types: HELLO/HELLO_ACK (handshake with screen resolution), 5 input types (mouse move/button/scroll, key down/up), 3 clipboard types (offer/chunk/ack), heartbeat/error/disconnect, 5 shell types (open/input/output/close/exit).

Ввод — fire-and-forget. Clipboard — fire-and-forget chunks (256 байт), reassembly по `index`. ACK-сообщения определены в протоколе, но в текущей реализации не используются (CRC на пакетном уровне даёт достаточную защиту для MVP). Heartbeat: каждые 2 сек, timeout 6 сек (3 пропущенных).

### Clipboard sync

Симметрично на обеих сторонах:
- Polling раз в 500мс через `arboard::Clipboard::get_text()`.
- Хэш последнего известного содержимого — защита от петли (когда мы сами записали входящий текст в локальный clipboard).
- ClipOffer { format=0 UTF-8 text, total_len } + N×ClipChunk { index, data ≤ 256B }, лимит 256 KB на буфер.
- Сборка через `BTreeMap<u16, Vec<u8>>` — устойчиво к out-of-order (хотя serial доставляет по порядку).
- Mac side: `apps/wiredesk-client/src/clipboard.rs`. Host side: `apps/wiredesk-host/src/clipboard.rs`. Не вынесено в общий crate — duplication приемлема.

### Shell-over-serial

Опциональный канал терминала на том же serial:
- Client → `ShellOpen { shell }` — Host спавнит подпроцесс (powershell/cmd на Windows, bash/zsh на Unix).
- `ShellInput { data }` — байты в stdin подпроцесса.
- `ShellOutput { data }` — байты из stdout/stderr, чанки по 480 байт.
- `ShellClose` — закрывает stdin (EOF); `ShellExit { code }` — на выходе процесса.
- Line-based MVP без PTY: vim, sudo с паролем не работают. SSH с key-based auth работает.

### Key design decisions

- **Scancodes, not VK codes** — ввод как hardware scancodes, работает независимо от раскладки Host (включая кириллицу).
- **Extended scancodes** (0xE0xx) — в SendInput требуют `KEYEVENTF_EXTENDEDKEY` flag.
- **Cmd → Ctrl** mapping в `egui_modifiers_to_u8`. Win-key combos (Win+Space, Win+L) — через явные кнопки в UI, потому что Cmd занят под Ctrl.
- **115200 baud, не 921600** — на дешёвых CH340 с Dupont-проводами 921600 даёт single-bit corruption (видели "bad magic" с XOR 0x80). 115200 надёжно. Bandwidth budget ~11 KB/s, реально ~1 KB/s для ввода + редкие всплески под clipboard/shell.
- **Leading 0x00 + drain on open** — серьёзный фикс для startup transient: при открытии порта CH340 выпускает мусорный байт, который иначе склеивается с первым кадром. Решено в `SerialTransport::send` (ведущий 0x00) + `SerialTransport::open` (drain OS буфера).
- **MockTransport** — mpsc, протокол-тесты без железа. `try_clone` для Mock возвращает ошибку (не нужно в тестах).
- **Aspect ratio correction** в `InputMapper::normalize_mouse` — letterbox/pillarbox для разной геометрии окна и Host.

### Известные ограничения

- **Ctrl+Alt+Del** через SendInput не сработает на Windows (защищено ядром, нужен SAS API в SYSTEM-сервисе или Group Policy `SoftwareSASGeneration`). Кнопка в UI есть, но ничего не делает реально. Альтернативы — Win+L (lock), Ctrl+Shift+Esc (Task Manager).
- **Картинки/файлы в clipboard** — не передаются, только текст.
- **Видео** — никогда. Ставь HDMI capture card отдельно.

## Hardware setup

```
Host HDMI → splitter → monitor + capture card → Mac (QuickTime/VLC for video)
Host USB-Serial ←→ null-modem (TX-RX crossed, GND-GND, VCC isolated) ←→ Mac USB-Serial
```

CH340 USB-to-TTL кабели: красный=VCC (изолировать), синий=GND, зелёный=TX, белый=RX. Полная инструкция: `docs/setup.md`.

## Plan

`docs/plans/wiredesk-mvp.md` — full MVP plan with protocol spec, etapes, and risk analysis.
