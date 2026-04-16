# WireDesk

Remote keyboard, mouse, and clipboard over serial. No network required.

## Problem

You have two computers side by side. One runs Windows with security software ("Continent" / APKSH) that blocks **all** network interfaces — Ethernet, Wi-Fi, virtual adapters, USB Ethernet, everything. You want to control it from your Mac.

## Solution

WireDesk sends keyboard/mouse input and clipboard data over a serial connection (USB-to-Serial adapters + null-modem cable). Video comes separately through an HDMI capture card viewed in QuickTime or VLC.

```
Host (Windows 11)                       Client (macOS)
    |                                        |
    |-- HDMI --> [splitter] --> capture --> QuickTime/VLC
    |                                        |
    |-- USB-Serial <-- null-modem --> USB-Serial
    |                                        |
    wiredesk-host                       wiredesk-client
    (console agent)                     (egui control panel)
```

## What WireDesk does

- Captures keyboard and mouse input on Mac, sends to Windows via serial
- Injects input on Windows via SendInput API (scancodes, works with any keyboard layout)
- Syncs clipboard text in both directions
- Toggle capture with Ctrl+Alt+G — input goes to Host when active, back to Mac when released
- Special key buttons: Ctrl+Alt+Del, Win key

## What WireDesk does NOT do

- Video streaming — use HDMI capture card + QuickTime/VLC
- File transfer (serial is ~90 KB/s — use a USB flash drive)
- Audio

## Hardware

| Component | Price | Purpose |
|-----------|-------|---------|
| USB HDMI capture card | $10-15 | Video (outside WireDesk) |
| HDMI splitter 1-to-2 | $5-10 | Monitor + capture |
| 2x USB-to-Serial (CH340/FTDI) | $3-5 each | Serial data channel |
| Null-modem wiring (TX-RX, GND-GND) | $0-3 | Connect serial adapters |

Total: ~$20-30.

## Build

Requires Rust toolchain.

```bash
cargo build --workspace
cargo test --workspace
```

## Run

**Host (Windows):**

```bash
wiredesk-host --port COM3 --baud 921600 --width 1920 --height 1080
```

**Client (macOS):**

```bash
wiredesk-client --port /dev/tty.usbserial-XXX --baud 921600
```

## Protocol

Custom binary protocol over COBS-framed serial:

- Packet: `[magic "WD"][type][flags][seq][len][payload][crc16]`
- 13 message types: handshake, 5 input types, 3 clipboard types, heartbeat/error/disconnect
- Input events: fire-and-forget (low latency)
- Clipboard: chunked with ACK (reliable delivery)
- Heartbeat: every 2 sec, timeout after 6 sec

Baud rate 921600 gives ~90 KB/s. Actual usage: ~1 KB/s for mouse+keyboard.

## Architecture

```
crates/
  wiredesk-core        — error types, shared types
  wiredesk-protocol    — packet format, messages, COBS, CRC-16
  wiredesk-transport   — Transport trait, SerialTransport, MockTransport
apps/
  wiredesk-host        — Windows agent (Session + InputInjector via SendInput)
  wiredesk-client      — macOS app (egui UI + input capture + keymap)
```

## Status

Early prototype (MVP). Core protocol and transport working. 62 tests passing.

## License

MIT
