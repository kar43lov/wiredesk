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
- **OS-level keyboard capture on macOS** via CGEventTap — system shortcuts like Cmd+Space (input-method switch) and Cmd+C/Cmd+V are intercepted before macOS gets them and forwarded to Host as Win+Space / Ctrl+C / Ctrl+V
- Syncs clipboard **text and PNG images** in both directions automatically (polled every 500ms; UTF-8 text up to 256 KB, images up to 1 MB encoded). Settings → Clipboard offers four independent toggles (send/receive × text/image) — handy when an app like Whispr Flow keeps writing transcribed text into the clipboard. Mac shows a visual progress bar in the window and inside the capture banner; menu bar shows "↑43%" / "↓67%". Windows surfaces oversize warnings as a tray balloon notification.
- Toggle capture with `Cmd+Esc` — input goes to Host when active, back to Mac when released
- Toggle fullscreen with `Cmd+Enter` — for "third monitor" workflow when WireDesk is dragged onto a display fed by the HDMI-capture. **Per-monitor selection** on macOS — pick a target display in Settings and `Cmd+Enter` lands fullscreen on that exact screen. Entering fullscreen auto-engages capture, leaving it auto-releases — no second shortcut needed.
- Auto-pauses capture when the WireDesk window loses focus — click any other Mac app and Cmd-shortcuts work locally again
- **Auto-detect CH340 cable** on Windows — `Detect` button in the Settings window scans serial ports for VID 0x1A86 and fills in the COM port automatically
- **Save & Restart** on Windows — apply settings without leaving the tray; the host respawns itself with the new config (single-instance retry-loop covers the handover)
- **Terminal-over-serial**: opens a shell on Host (powershell/cmd) and pipes I/O over the same serial link. From there you can run scripts, copy files, or `ssh` to other machines using the Host's internet connection.

> **Note on Ctrl+Alt+Del:** Windows reserves this combo for the kernel SAS handler, so a SendInput-driven press won't reach it without a SYSTEM-level service or `SoftwareSASGeneration` Group Policy. The button is in the UI but won't actually trigger the secure screen. Use Win+L to lock or Ctrl+Shift+Esc for Task Manager instead.

> **macOS permission required.** WireDesk needs Accessibility permission (System Settings → Privacy & Security → Accessibility → add the `wiredesk-client` binary). Without it the OS-level keyboard capture is silently disabled. The app shows an instruction screen on first launch.

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

Defaults are baked in for a single-user setup (`COM3`, `/dev/cu.usbserial-120`, 115200 baud, 2560×1440). Override with flags or via the settings UI / `config.toml`.

### Configuration

Both binaries persist their settings in TOML at the OS config dir:

| Platform | Path |
|----------|------|
| Windows  | `%APPDATA%\WireDesk\config.toml` |
| macOS    | `~/Library/Application Support/WireDesk/config.toml` |

Resolution order (low → high precedence): hardcoded defaults → `config.toml` → CLI flags.

### Host (Windows) — tray agent

Release builds run as a background tray agent — no console window, icon `W` in the system tray.

```bash
# Release build runs hidden as a tray app
.\target\release\wiredesk-host.exe

# Right-click the tray icon for: Show Settings… / Open Logs / Quit
# Settings window persists changes to %APPDATA%\WireDesk\config.toml
# "Run on startup" toggle writes HKCU\Software\Microsoft\Windows\CurrentVersion\Run
```

Logs roll daily into `%APPDATA%\WireDesk\host.log.YYYY-MM-DD`. Panics and `log::*` macros across the host crate are captured into the same file via `tracing-log`.

CLI overrides still work for one-off runs:

```bash
wiredesk-host --port COM4 --width 1920 --height 1080
```

### Client (macOS) — `WireDesk.app` bundle

Build the `.app` bundle once:

```bash
./scripts/build-mac-app.sh
# → target/release/WireDesk.app
```

Double-click `WireDesk.app` to launch (first time: right-click → Open to bypass Gatekeeper). The Settings panel in chrome-mode (visible when not in capture/fullscreen) shows port/baud/width/height/client name with a Save button — changes write `~/Library/Application Support/WireDesk/config.toml` and require a restart to apply.

Or run the binary directly for development:

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

MVP working end-to-end on real hardware: handshake, mouse, keyboard (incl. Cyrillic via scancodes), language toggle via Cmd+Space, bidirectional clipboard sync via Cmd+C/Cmd+V (text + PNG images up to 1 MB encoded; LRU text-history dedup tolerates Whispr Flow-style "save→inject→restore" patterns; modifier-only hotkeys like Ctrl+Option pass through to macOS even in capture mode so dictation tools keep working), OS-level keyboard hijack on macOS, fullscreen toggle (per-monitor on macOS) with auto-engage/release of capture, shell-over-serial. Mac UI: visual progress bars in the chrome panel and inside the capture banner (visible in fullscreen), `NSStatusItem` in the menu bar (W / ↑% / ↓%), Settings → Clipboard with four send/receive × text/image toggles. Win host: tray agent (nwg) with auto-detect CH340, Save & Restart, balloon notification on oversize image, double-click on tray icon opens Settings. TOML-backed settings on both sides, file logging on Windows, autostart toggle, single-instance lock. ~280 tests passing.

## License

MIT
