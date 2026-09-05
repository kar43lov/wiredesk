# WireDesk

[![CI](https://github.com/kar43lov/wiredesk/actions/workflows/ci.yml/badge.svg)](https://github.com/kar43lov/wiredesk/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2021-orange.svg)](https://www.rust-lang.org/)
![Platforms](https://img.shields.io/badge/platforms-macOS%20%7C%20Windows-lightgrey.svg)

Remote keyboard, mouse, and clipboard over serial. No network required.

Drive a locked-down Windows machine from the computer next to it over a
USB-serial null-modem cable — keyboard, mouse, clipboard and a real shell —
when every network path between them is blocked. Video comes separately
through an HDMI capture card.

**Contents:** [Problem](#problem) · [Solution](#solution) ·
[Features](#what-wiredesk-does) · [Security model](#security-model) ·
[Hardware](#hardware) · [Build](#build) · [Run](#run) ·
[Protocol](#protocol) · [Architecture](#architecture) · [Status](#status)

**Documentation:** [setup guide](docs/setup.md) ·
[running & debugging](docs/run.md) · [architecture](docs/architecture.md) ·
[`wd --exec` usage](docs/wd-exec-usage.md) ·
[Bluetooth transport](docs/bluetooth-transport.md) ·
[known limitations](docs/known-limitations.md)

## Problem

You have two computers side by side. One runs Windows with security software ("Continent" / APKSH) that blocks **all** network interfaces — Ethernet, Wi-Fi, virtual adapters, USB Ethernet, everything. You want to control it from your Mac.

## Solution

WireDesk sends keyboard/mouse input and clipboard data over either a **USB-Serial null-modem** (default; ~11 KB/s on CH340 @ 115200, or **up to ~300 KB/s on FT232H @ 3 Mbaud** — verified live, only the `baud` setting changes) or a **Bluetooth LE** link (live-measured ~4-5 KB/s — slower than serial, kept as a no-cable fallback; see [`docs/bluetooth-transport.md`](docs/bluetooth-transport.md)). Video comes separately through an HDMI capture card viewed in QuickTime or VLC.

```
Host (Windows 11)                       Client (macOS or Windows)
    |                                        |
    |-- HDMI --> [splitter] --> capture --> QuickTime/VLC
    |                                        |
    |-- USB-Serial <-- null-modem --> USB-Serial
    |                                        |
    wiredesk-host                       wiredesk-client (GUI)
    (console agent)                     wiredesk-term   (macOS only, terminal-only, e.g. in Ghostty)
```

The client GUI runs on **macOS and Windows** from the same codebase — input
capture, clipboard sync, per-monitor fullscreen and the status-area icon are
implemented against each platform's own API behind one interface. A Windows
client drives a Windows host over the same serial link; `wd` / `wd --exec`
remain macOS-only (see [Known limitations](docs/known-limitations.md)).

## What WireDesk does

### Input

- **Keyboard and mouse** from the client to the host, including X1/X2 side buttons (Back/Forward). Input is injected on Windows via `SendInput` using **scancodes**, so any keyboard layout works — Cyrillic included.
- **OS-level capture**, not just window-level: a CGEventTap on macOS, a `WH_KEYBOARD_LL` hook on Windows. System shortcuts are intercepted before the local OS sees them, so `Cmd+Space` reaches the host as `Win+Space` and `Cmd+C`/`Cmd+V` as `Ctrl+C`/`Ctrl+V`.
- **Toggle capture** with `Cmd+Esc` (`Ctrl+Esc` on a Windows client); input returns to the local machine when released. Capture pauses by itself when the window loses focus, so other apps keep their shortcuts.
- **Fullscreen** with `Cmd+Enter` (`Ctrl+Enter` on Windows) for the "third monitor" workflow: drag WireDesk onto the display fed by the HDMI capture card and it covers exactly that screen. Entering fullscreen engages capture, leaving it releases — one shortcut, not two. On macOS this is a *borderless* fullscreen rather than the system one, so there is no zoom animation and no separate Mission Control tile; the menu bar and Dock do hide. ([Why](docs/known-limitations.md))
- **Karabiner-Elements compensation** for people who remap `left_command ↔ left_option` so one physical keyboard suits both operating systems. A Settings checkbox re-swaps the modifiers on the way out, so `Cmd+V` still arrives as `Ctrl+V`.

### Clipboard

- **Text, PNG images and single files**, both directions, polled every 200 ms. Limits: 256 KB of UTF-8 text, 1 MB encoded image, 20 MB file.
- **Sending files is opt-in** (`Send files` checkbox, off by default), so a stray `Cmd+C` on a file never leaves the machine. Receiving files, text and images sync automatically; six independent checkboxes cover send/receive × text/image plus the two file directions.
- **Cancel** on any transfer in flight — abort a large image without dropping the session.
- **Dictation-tool paste works.** A synthetic `Cmd+V` from Whispr Flow or TextExpander is recognised by its event source and held until the outgoing clipboard sync finishes (4 s cap), so the host pastes what you just dictated rather than the previous clipboard.

### The link, and living with it

- **Self-healing.** Ten consecutive corrupt frames count as a storm — the usual cause is an FT232H whose clock drifts and corrupts both directions — and each side reopens its serial port independently. The client then reconnects with a 1 s → 30 s backoff, showing `Reconnecting…` while it does.
- **Serial-port dropdown with auto-detect** on the host: ports are listed with a chip hint (`COM7 — FT232H`, `COM5 — CH340`), and `Detect` recognises WCH CH340 (VID `0x1A86`) and FTDI FT232H/R/2232/4232 (VID `0x0403`).
- **Save & Restart** on both sides — the Settings button respawns the binary with the new config; the Windows tray has a `Restart` entry too.
- **A shell over the same wire.** `wd` opens a real PTY on the host — vim, htop, `ssh` without `-tt`, PSReadLine history and Tab completion all behave. `wd --exec` runs one command and returns its exit code, which is what makes the link scriptable.

> **Ctrl+Alt+Del cannot be sent.** Windows reserves it for the kernel's secure-attention handler, out of reach of `SendInput` without a SYSTEM-level service. The button exists in the UI but does not reach the secure screen. `Win+L` locks, `Ctrl+Shift+Esc` opens Task Manager.

> **macOS needs Accessibility permission** (System Settings → Privacy & Security → Accessibility → add the `wiredesk-client` binary), otherwise OS-level capture is silently disabled — the app shows an instruction screen on first launch. A Windows client needs no permission at all.

## What WireDesk does NOT do

- Video streaming — use HDMI capture card + QuickTime/VLC
- Bulk file transfer or multi-file selection (single files up to 20 MB sync through the clipboard; for multi-GB transfers a USB flash drive is still faster — serial channel is ~11 KB/s on CH340 / ~300 KB/s on FT232H). Multi-file selection and directories are out of scope for Phase 1.
- Audio

## Security model

Reporting process and supported versions: [`SECURITY.md`](SECURITY.md).

**The link is neither authenticated nor encrypted.** The handshake is a plain `Hello`/`HelloAck` exchange carrying a name and a protocol version — there is no pairing code, no shared secret, no challenge. Whoever can talk to the other end of the channel can inject keyboard and mouse input into the Windows session, open a shell on it, and read or write files through the clipboard. That is the whole point of the tool, so treat access to the channel as equivalent to sitting down at the machine.

What that means per transport:

- **Serial (default).** Trust rests on physical access to the cable. Someone who can reach the null-modem wiring could already reach the keyboard, so this is an acceptable trade — but it does mean an unattended machine with the cable exposed is an unattended machine, full stop.
- **Bluetooth LE (opt-in).** Radio range replaces cable reach, so the missing application-level authentication has to be made up at the link layer: the GATT characteristics are published as `EncryptionRequired`, which makes Windows insist the peers pair before any data flows. Until 2026-09 they were `Plain`, and the RX characteristic — the inbound direction, the one carrying input events and shell input — had no protection level at all, so anything in range that knew the service UUID (a constant in this repository) could drive the host. Set `bluetooth.require_encryption = false` in `config.toml` to get the old behaviour back where pairing is impractical; the host logs a warning when you do. Peer matching is still by service UUID alone (`peer_name` is advisory), so it is the encryption, not the name, that identifies who you are talking to.

Files arriving over the clipboard are written into a cache directory (`~/Library/Caches/WireDesk/`, `%TEMP%\WireDesk\`) with the basename sanitized against path traversal and NTFS device names, so a hostile peer cannot choose where they land — but it can still place arbitrary bytes there, and the receiving side then points the OS clipboard at them.

The Mac IPC socket (`wd-exec.sock`) is chmod 0600, and its directory is narrowed to 0700 before the socket is bound — so the brief moment when the socket still carries umask-derived permissions is not reachable by another local user.

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

**Prebuilt binaries.** Every `v*` tag produces a draft [GitHub Release](https://github.com/kar43lov/wiredesk/releases) with `WireDesk.app` (macOS, Apple Silicon), the `wd` terminal binary and `wiredesk-host.exe` (Windows x64), built by `.github/workflows/release.yml`. They are not signed or notarized: on macOS use right-click → Open on first launch (or `xattr -d com.apple.quarantine WireDesk.app`), on Windows accept the SmartScreen prompt.

On Windows, build the client with `.\scripts\build-win-client.ps1` — it is
the same `cargo build --release -p wiredesk-client`, but building *on*
Windows is what embeds the app icon into the `.exe` (build.rs needs rc.exe
or windres, which a cross-build from the Mac does not have).

Cross-checking the Windows build from macOS needs the target and, for a real
link rather than a type check, mingw-w64:

```bash
rustup target add x86_64-pc-windows-gnu
brew install mingw-w64                       # only needed for `build`
cargo clippy -p wiredesk-client --target x86_64-pc-windows-gnu --all-targets -- -D warnings
cargo build  -p wiredesk-client --target x86_64-pc-windows-gnu
```

## Run

> First time? Read **[docs/setup.md](docs/setup.md)** — covers wiring, port discovery, Rust install on Windows (incl. how to do it under "Continent" lockdown), and handshake troubleshooting.

Hardcoded fallback defaults are baked in for a bare CH340 cable (`COM3`, `/dev/cu.usbserial-120`, 115200 baud, 2560×1440) — override with flags, `config.toml`, or (Mac CLI only) let `wd` auto-detect the serial adapter by USB VID. See [wd port/baud auto-resolve](#wd-portbaud-auto-resolve) below.

### Configuration

Both binaries persist their settings in TOML at the OS config dir:

| Platform | Path |
|----------|------|
| Windows  | `%APPDATA%\WireDesk\config.toml` |
| macOS    | `~/Library/Application Support/WireDesk/config.toml` |

Resolution order (low → high precedence): hardcoded defaults → `config.toml` → CLI flags.

### Host (Windows) — tray agent

Release builds run as a background tray agent — no console window; a monitor-glyph icon sits in the system tray, tinted by link state (grey / yellow / green).

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

Double-click `WireDesk.app` to launch (first time: right-click → Open to bypass Gatekeeper). Grant Accessibility when the app asks — the permission screen disappears on its own once the grant lands, no relaunch. macOS ties that grant to the code signature, and the default ad-hoc signature changes on every rebuild; if you rebuild often, run `./scripts/make-dev-signing-cert.sh` once to create a local "WireDesk Dev" signing identity that `build-mac-app.sh` then uses automatically, so the grant survives rebuilds. The Settings panel in chrome-mode (visible when not in capture/fullscreen) shows port/baud/width/height/client name with a Save button — changes write `~/Library/Application Support/WireDesk/config.toml` and require a restart to apply. The window's position and size are remembered in the same file and restored on the next launch (a window whose saved display has since been unplugged is pulled back onto the primary one).

Or run the binary directly for development:

```bash
wiredesk-client
# or with overrides
wiredesk-client --port /dev/cu.usbserial-XXX
```

Logs roll daily into `~/Library/Application Support/WireDesk/client.log.YYYY-MM-DD` and also stream to stderr. Both `WireDesk.app` and the bare binary write to the same file, so post-mortem after a hang or disconnect is now possible without re-running with `RUST_LOG=debug`. The `RUST_LOG` env-filter still works for raising verbosity (defaults to `info`).

### Client (Windows) — `wiredesk-client.exe`

The same GUI, built for Windows. Use it to drive one Windows machine from
another over the serial link.

```powershell
.\scripts\build-win-client.ps1
.\target\release\wiredesk-client.exe
```

Differences from the Mac client, all of them consequences of the platform:

| | macOS | Windows |
|---|---|---|
| Keyboard hijack | CGEventTap (needs Accessibility permission) | `WH_KEYBOARD_LL` hook (no permission needed) |
| Release capture | `Cmd+Esc` | `Ctrl+Esc` |
| Toggle fullscreen | `Cmd+Enter` | `Ctrl+Enter` |
| Status area | menu bar item, progress shown as text | tray icon, progress shown in the tooltip |
| Files on the clipboard | `NSPasteboard` file URL | `CF_HDROP` |
| Transport | serial or BLE | serial only |
| `wd` / `wd --exec` | yes | not yet |

Settings and logs live in `%APPDATA%\WireDesk\` — same layout as the host,
so a machine running both keeps `config.toml` for the client and
`host.log.*` / `client.log.*` side by side. First launch defaults to `COM3`;
pick the real adapter in Settings → Port and hit **Save & Restart**.

`Ctrl+Esc` is normally the Windows shortcut for the Start menu. While capture
is on, the hook takes it first, which is the point — the keystroke is meant
for the host. Two combos the OS keeps for itself and never delivers to any
hook: **Ctrl+Alt+Del** and **Win+L**.

### Client (macOS) — terminal (`wd`)

A shell on the host, inside your own terminal (Ghostty / iTerm / Terminal.app),
with history, scrollback and copy-paste:

```bash
wiredesk-term

# Optional: pick a specific shell
wiredesk-term --shell powershell
wiredesk-term --shell cmd
```

#### `wd` port/baud auto-resolve

Run with no flags and `wd` picks port and baud in this order: explicit `--port`/`--baud` on the command line → a single unambiguous WCH/FTDI USB-serial adapter detected on the system right now → `port`/`baud` from `config.toml` (the same file the GUI's Settings panel writes) → hardcoded fallback. Auto-detection survives moving the cable to a different physical USB port — macOS reassigns `/dev/cu.usbserial-NNN` by port location, so a config-only lookup would go stale the moment you replug. When resolution didn't come purely from the CLI, `wd` prints where each value came from:

```
wiredesk-term: port via auto-detected adapter, baud via config.toml
wiredesk-term: connecting to /dev/cu.usbserial-140 @ 3000000 baud (Ctrl+] to quit)
```

On launch you'll see a banner with the host name and screen size plus a hotkey cheatsheet:

```
wiredesk-term: connected to 'wiredesk-host' (2560×1440). Press Ctrl+] to quit.

  Hotkeys (handled locally):
    Ctrl+]   exit wiredesk-term and restore your terminal

  Forwarded to host shell:
    Ctrl+C   interrupt the running command on host
    Ctrl+D   send EOF to host stdin (closes the shell)
    others   pass through to host as typed
```

The CLI runs as a **pass-through raw bridge** — the host shell now lives in a real PTY (ConPTY) so PSReadLine, vim, htop, less, ssh-without-`-tt`, `git commit`-editor and similar interactive tools all just work. A 2-second heartbeat keeps idle sessions alive; resizing the local terminal window reflows `htop` / `vim` on the host within ~500 ms.

**Runs alongside the GUI (macOS).** You no longer have to quit `WireDesk.app` to use `wd`. When the GUI is running it holds the serial port; `wd` connects to the GUI's Unix-socket relay and streams the PTY session through it — so you can poke the Host by hand while the GUI keeps an active capture/clipboard session going. The startup banner prints the source ("interactive via GUI IPC" vs "interactive via direct serial"). Ctrl+] exits cleanly either way. If the GUI is closed, `wd` opens the serial port directly, exactly as before. The Host has a single shell slot, so if a `wd --exec` is already in flight (or vice versa) the second one fails fast with a "shell busy" message. Exit codes differ by direction: a refused `wd --exec` exits 125 (transport class); a refused interactive `wd` exits 1 (the "shell busy" text still prints on stderr).

For a shorter command alias, drop this in `~/.zshrc` / `~/.bashrc`:

```bash
alias wd='wiredesk-term'
```

**SSH to remote Linux through `wd`:** plain `ssh dev` works directly — the host shell is a real PTY, so the remote bash gets allocated its own PTY automatically. `.bashrc` loads, prompt and aliases work, `vim`/`htop` over ssh render correctly. The `ssh -tt` workaround is only relevant for `wd --exec --ssh ALIAS …` (the non-interactive path is intentionally pipe-based for sentinel detection — see below).

### Run a single command (`--exec` mode)

For scripts and AI-agents that just need "execute one command, give me stdout, give me the exit code", use `--exec`:

```bash
# On host PowerShell:
wd --exec "Get-ChildItem"
wd --exec "exit 7"     # exits with 7

# Through SSH to a remote box:
wd --exec --ssh prod-box "docker ps"
wd --exec --ssh prod-box "tail -100 /var/log/syslog"

# Compress stdout for large text output (5-10x speedup):
wd --exec --compress --ssh prod-box "docker logs --tail 5000 app.main 2>&1"
```

This skips raw mode and the interactive bridge entirely. The CLI sends the command wrapped in a UUID-tagged sentinel and reads the host's output until that sentinel line is seen, strips the prompt / banner / echoed command, and exits with the same code the command produced.

`--compress` opts into gzip+base64 wrapping of stdout on the host (5–10× speedup on text-heavy output like logs / JSON dumps); `_search` results, `kubectl describe`, `Get-EventLog -Newest N` all benefit. Stdout is byte-for-byte identical to the non-compress path so `| grep`/`| jq` keep working. Both bash (`--ssh`) and PowerShell host-direct paths are supported. Skip for binary output (no ratio gain) or short outputs (~0.5 s overhead). Decode failures surface as exit 125.

The PowerShell wrapper normalises exit codes, which is what makes `--exec` scriptable:

- `$LASTEXITCODE=0` and `$ErrorActionPreference='Stop'` are set before the command runs, so a
  cmdlet that succeeds reports 0 and a non-terminating error becomes a terminating one.
- The command is wrapped in `try { … } catch { $LASTEXITCODE=1 }`, so a thrown error is 1
  rather than a silent success.
- A native `.exe` keeps its own exit code untouched.

Without this, `Get-ChildItem C:\nope` would exit 0 with an error on stderr — the classic way a
script "succeeds" at doing nothing.

**For agent / automation authors:** writing helpers on top of `wd --exec` (binary push, `.ps1` generation, multi-call orchestration)? Read [Host environment quirks](docs/wd-exec-usage.md#host-environment-quirks) — three Win-host gotchas (`AppendAllBytes` missing in .NET 4.x, ru-RU PS parser without UTF-8 BOM, bash `$()` subshell killing serial channel) that look like `wd` bugs but aren't. Each costs hours if you don't know.

For sub-second persistent SSH (so consecutive `--ssh prod-box` calls don't re-handshake every time), set up OpenSSH ControlMaster on the host's `~/.ssh/config`:

```
Host prod-box
    HostName 10.x.x.x
    User <user>
    ControlMaster auto
    ControlPath C:/Users/User/.ssh/cm-%r@%h:%p
    ControlPersist 10m
```

The first `wd --exec --ssh prod-box ...` call creates the multiplexed connection; the next ten minutes of calls re-use it. No daemon required on the WireDesk side.

The same relay carries `wd --exec`, so a scripted call and a live GUI session never fight over the serial port — see [Client (macOS) — terminal](#client-macos--terminal-wd) above.

## Protocol

Custom binary protocol over COBS-framed serial:

- Packet: `[magic "WD"][type][flags][seq][len][payload][crc16]`
- 21 message types: handshake, 5 input types, 4 clipboard types, heartbeat/error/disconnect, 7 shell types (incl. ShellOpenPty + PtyResize)
- Input events: fire-and-forget (low latency)
- Clipboard: chunked, fire-and-forget (CRC at packet level handles drops; next poll cycle resends)
- Heartbeat: every 2 sec, timeout after 6 sec

Default baud rate 115200 (~11 KB/s on CH340) — rock-solid for mouse+keyboard (~1 KB/s) plus shell I/O. On **FTDI FT232H** breakouts (genuine FTDI, VID 0x0403) baud `3_000_000` (~300 KB/s) runs stable on a soldered null-modem — verified live 2026-05-28 — taking the 1 MB clipboard image from ~90 sec down to ~3 sec. No code changes — just `baud = 3000000` in both `config.toml`. Win11 needs the **FTDI CDM driver** (ftdichip.com → VCP) so the device appears as `USB Serial Port (COMx)` in Ports (COM & LPT); macOS VCP is built in. CH340 cables with Dupont wiring are stuck at 115200 — higher rates (460800, 921600) showed single-bit corruption from the CH340's PLL jitter.

## Architecture

```
crates/
  wiredesk-core        — error types, shared types, CF_HDROP file clipboard (shared by host and Windows client)
  wiredesk-protocol    — packet format, messages, COBS, CRC-16
  wiredesk-transport   — Transport trait, SerialTransport, MockTransport, detect (USB VID classification, shared by host's Detect button and `wd`'s auto-resolve)
  wiredesk-exec-core   — shared sentinel-runner + ExecTransport trait for `wd --exec` (used by term and client)
apps/
  wiredesk-host        — Windows agent (Session + InputInjector + shell subprocess)
  wiredesk-client      — GUI for macOS and Windows (egui — input capture, keymap, clipboard); platform code sits behind one facade per concern (keyboard_tap, status_bar, monitor, clipboard_files, mac_window). macOS additionally hosts the IPC relay for parallel `wd`/`wd --exec` over wd-exec.sock (exec handler + interactive streaming relay + shell-channel owner lock)
  wiredesk-term        — macOS CLI (raw-mode terminal bridge — runs inside Ghostty/iTerm)
```

## Status

Working end-to-end on real hardware, in daily use for a single-operator setup.
Everything in the feature list above is implemented and live-tested; the
[known limitations](docs/known-limitations.md) are things that will not be
fixed rather than things that are missing.

| Component | Tests |
|---|--:|
| `wiredesk-client` (GUI, input, clipboard) | 356 |
| `wiredesk-host` (Windows agent) | 151 |
| `wiredesk-exec-core` (shared `wd --exec` runner) | 100 |
| `wiredesk-protocol` (framing, COBS, CRC-16) | 88 |
| `wiredesk-transport` (serial, BLE, port detection) | 51 |
| `wiredesk-term` (`wd` CLI) | 50 |
| `wiredesk-core` (shared types, clipboard files) | 25 |
| **Total** | **821** |

Plus 5 ignored tests that need a live Windows session. On macOS run the suite
with `cargo test --workspace -- --test-threads=1` — the host package has a
pre-existing flake on the parallel runner.

**Maturity by area:**

| Area | State |
|---|---|
| Input, clipboard (text / image / single file), fullscreen | Stable |
| Serial transport, auto-recovery from a frame-error storm | Stable |
| `wd` / `wd --exec` shell over the same link (macOS) | Stable |
| Windows client | New — builds, lints and links; awaiting live use |
| Bluetooth LE transport | Works, but slower than serial; kept as a no-cable fallback |
| Multi-file clipboard, directories, video | Out of scope by design |

## License

MIT
