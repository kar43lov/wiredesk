# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

WireDesk — утилита для удалённого управления мышью, клавиатурой и clipboard на Windows-машине через serial-соединение (без сети). Видео — отдельно через HDMI capture card.

Контекст: на Host (Windows 11) стоит ПО "Континент", которое блокирует все сетевые интерфейсы. Serial (COM-порт) не блокируется.

## Build & Test

```bash
cargo test --workspace          # все тесты (71)
cargo clippy --workspace -- -D warnings  # линтер
cargo build --workspace         # сборка
```

Host agent компилируется только на Windows (Windows API за `cfg(target_os = "windows")`). На macOS используется MockInjector.

```bash
# запуск host agent (Windows)
cargo run -p wiredesk-host -- --port COM3 --baud 921600

# запуск client (macOS)
cargo run -p wiredesk-client -- --port /dev/tty.usbserial-XXX --baud 921600
```

## Architecture

Rust workspace с 5 crate:

```
crates/
  wiredesk-core       — WireDeskError, типы (Resolution, MouseButton, Modifiers)
  wiredesk-protocol   — бинарный протокол: Packet, Message (13 типов), COBS framing, CRC-16
  wiredesk-transport  — trait Transport, SerialTransport, MockTransport
apps/
  wiredesk-host       — Windows console agent: Session state machine + InputInjector
  wiredesk-client     — macOS egui app: маленькое окно с toggle capture + input mapping
```

### Data flow

```
Client (macOS)                          Host (Windows)
  egui captures input                     Session::tick() loop
  → InputMapper normalizes                  → recv Packet via serial
  → Packet serialized                       → deserialize Message
  → COBS encoded                            → InputInjector::key_down/mouse_move/...
  → SerialTransport::send()                 → Win32 SendInput API
```

### Protocol (wiredesk-protocol)

Packet: `[magic "WD"][type][flags][seq:u16][len:u16][payload][crc16]`, COBS-framed over serial.

18 message types: HELLO/HELLO_ACK (handshake with screen resolution), 5 input types (mouse move/button/scroll, key down/up), 3 clipboard types (offer/chunk/ack), heartbeat/error/disconnect, 5 shell types (open/input/output/close/exit).

Input events are fire-and-forget. Clipboard chunks require ACK. Heartbeat timeout = 3 missed (6 sec). Shell I/O is byte-stream (no framing inside payload).

### Shell-over-serial

Optional terminal channel multiplexed over the same serial connection:
- Client sends `ShellOpen { shell }` — Host spawns subprocess (powershell/cmd by default on Windows, bash/zsh on Unix)
- `ShellInput { data }` carries raw bytes to write to subprocess stdin
- `ShellOutput { data }` carries stdout/stderr bytes back, chunked at 480 bytes
- `ShellClose` drops stdin (subprocess sees EOF); `ShellExit { code }` notifies on process exit
- Line-based MVP — interactive programs needing a real PTY (vim, sudo password prompts) will not work properly. SSH with key-based auth works fine.

### Key design decisions

- **Scancodes, not VK codes** — input is sent as hardware scancodes so it works regardless of Host keyboard layout (including Cyrillic)
- **Extended scancodes** (0xE0xx) need `KEYEVENTF_EXTENDEDKEY` flag in SendInput
- **Cmd → Ctrl** mapping on macOS side (egui_modifiers_to_u8)
- **MockTransport** uses mpsc channels — all protocol tests work without serial hardware
- **Aspect ratio correction** in InputMapper::normalize_mouse — accounts for window vs host screen ratio
- **Partial frame preservation** — SerialTransport keeps read_buf between recv() calls, with timeout limit (50 retries)

## Hardware setup

```
Host HDMI → splitter → monitor + capture card → Mac (QuickTime/VLC for video)
Host USB-Serial ←→ null-modem ←→ Mac USB-Serial (WireDesk for input/clipboard)
```

Baud rate: 921600. Bandwidth budget: ~90 KB/s, actual usage ~1 KB/s (mouse+keyboard).

## Plan

`docs/plans/wiredesk-mvp.md` — full MVP plan with protocol spec, etapes, and risk analysis.
