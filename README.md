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
    wiredesk-host                       wiredesk-client (GUI)
    (console agent)                     wiredesk-term   (terminal-only, e.g. in Ghostty)
```

## What WireDesk does

- Captures keyboard and mouse input on Mac, sends to Windows via serial
- Injects input on Windows via SendInput API (scancodes, works with any keyboard layout, Cyrillic included)
- Syncs clipboard text in both directions automatically (polled every 500ms, UTF-8 only)
- Toggle capture with Ctrl+Alt+G — input goes to Host when active, back to Mac when released
- Special key buttons: Win key, **Lang (Win+Space)** for input-method switching, Ctrl+Alt+Del
- **Terminal-over-serial**: opens a shell on Host (powershell/cmd) and pipes I/O over the same serial link. From there you can run scripts, copy files, or `ssh` to other machines using the Host's internet connection.

> **Note on Ctrl+Alt+Del:** Windows reserves this combo for the kernel SAS handler, so a SendInput-driven press won't reach it without a SYSTEM-level service or `SoftwareSASGeneration` Group Policy. The button is in the UI but won't actually trigger the secure screen. Use Win+L to lock or Ctrl+Shift+Esc for Task Manager instead.

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

### Wiring (CH340 USB-to-TTL)

Standard CH340 cables expose four wires. Connect them as a null-modem (TX/RX crossed, GND straight, VCC NOT connected):

```
Cable A              Cable B
🟢 green (TX) ────►  ⚪ white (RX)
⚪ white (RX) ◄────  🟢 green (TX)
🔵 blue  (GND) ────  🔵 blue  (GND)
🔴 red   (VCC) ╳ ╳   🔴 red   (VCC)   — leave isolated
```

Full step-by-step (wiring + first-time install + first run + troubleshooting): **[docs/setup.md](docs/setup.md)**.

## Build

Requires Rust toolchain.

```bash
cargo build --workspace
cargo test --workspace
```

## Run

> First time? Read **[docs/setup.md](docs/setup.md)** — covers wiring, port discovery, Rust install on Windows (incl. how to do it under "Continent" lockdown), and handshake troubleshooting.

Defaults are baked in for a single-user setup (`COM3`, `/dev/cu.usbserial-120`, 115200 baud, 2560×1440). Override with flags if your hardware differs.

**Host (Windows):**

```bash
wiredesk-host
# or with overrides
wiredesk-host --port COM4 --width 1920 --height 1080
```

**Client (macOS) — full GUI** (mouse, keyboard, clipboard, embedded terminal):

```bash
wiredesk-client
# or with overrides
wiredesk-client --port /dev/cu.usbserial-XXX
```

**Client (macOS) — terminal only** (run inside Ghostty/iTerm/Terminal.app for a real shell experience with history, scrollback, copy/paste):

```bash
wiredesk-term

# Optional: pick a specific shell
wiredesk-term --shell powershell
wiredesk-term --shell cmd
```

Press **Ctrl+]** in `wiredesk-term` to quit and restore the local terminal.

`wiredesk-client` and `wiredesk-term` are mutually exclusive — they share the same serial port. Run one or the other depending on whether you need the GUI or just a shell.

## Protocol

Custom binary protocol over COBS-framed serial:

- Packet: `[magic "WD"][type][flags][seq][len][payload][crc16]`
- 18 message types: handshake, 5 input types, 3 clipboard types, heartbeat/error/disconnect, 5 shell types
- Input events: fire-and-forget (low latency)
- Clipboard: chunked, fire-and-forget (CRC at packet level handles drops; next poll cycle resends)
- Heartbeat: every 2 sec, timeout after 6 sec

Default baud rate 115200 (~11 KB/s). Higher rates (460800, 921600) work in theory but on cheap CH340 cables with Dupont wiring we saw single-bit corruption — 115200 is rock solid and more than enough headroom for mouse+keyboard (~1 KB/s) plus shell I/O.

## Architecture

```
crates/
  wiredesk-core        — error types, shared types
  wiredesk-protocol    — packet format, messages, COBS, CRC-16
  wiredesk-transport   — Transport trait, SerialTransport, MockTransport
apps/
  wiredesk-host        — Windows agent (Session + InputInjector + shell subprocess)
  wiredesk-client      — macOS GUI (egui — input capture, keymap, clipboard, shell panel)
  wiredesk-term        — macOS CLI (raw-mode terminal bridge — runs inside Ghostty/iTerm)
```

## Status

MVP working end-to-end on real hardware: handshake, mouse, keyboard (incl. Cyrillic via scancodes), language toggle, bidirectional clipboard sync, shell-over-serial. 71 tests passing.

## License

MIT
