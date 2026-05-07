# Bluetooth LE Transport (Plan C)

> **Status 2026-05-07:** infrastructure shipped end-to-end, but **the
> performance goal was not met on the tested hardware**. Live testing
> on Mac M4 + Win11 BT 5.x measured **~4-5 KB/s symmetric** — *slower*
> than the CH340 serial baseline (~11 KB/s). Use BLE only when a
> cable is genuinely unavailable; default to serial otherwise.
> Faster real channel-upgrade is **Plan A (FT232H @ 3 Mbaud, ~300 KB/s)**
> — see `docs/briefs/ft232h-upgrade.md`.

WireDesk supports a Bluetooth Low Energy alternative to the default
USB-Serial channel between the Mac client (Central) and the Win11 host
(Peripheral). The infrastructure is correct (custom GATT service,
fragmentation, reconnect helper, factory-based switching); only the
real-world wire throughput on this hardware combo turned out lower
than the original brief's estimate.

## When to use

| Channel       | Live measured speed | Hardware              | Setup time    |
|---------------|--------------------|-----------------------|---------------|
| USB-Serial    | ~11 KB/s           | already have it       | already done  |
| Bluetooth LE  | **~4-5 KB/s**      | already have BT radio | one-time pair |
| FT232H @ 3M   | ~300 KB/s (planned) | $20-30, must order   | wait + plug   |

**BLE measured slower than serial** (4-5 KB/s vs 11 KB/s) on the
Mac M4 + Win11 reference setup, contrary to the brief's
~30-100 KB/s estimate. Likely causes:

- macOS CoreBluetooth's WriteWithoutResponse drops silently when the
  internal queue overflows; we have to interleave WriteWithResponse
  for backpressure, and each ATT-ack roundtrip eats throughput.
- WinRT's `NotifyValueAsync.get()` blocks per-notification until the
  BLE link layer delivers, capping Win→Mac at the connection-event
  rate (≈30 ms intervals).
- ATT MTU isn't verified to actually negotiate up to 247 — could be
  much lower on this hardware combo.

Realistic positioning: BLE is a **no-cable fallback** when serial is
unavailable. For day-to-day use, keep `transport = "serial"`. For a
real speed-up, wait on FT232H (Plan A) or a future tuning pass on
the BLE crate (try `bluest` instead of `btleplug`, manual Connection
Parameter Update Request, ATT MTU instrumentation).

## One-time pairing

Before flipping `transport = "bluetooth"` in config.toml, pair the two
machines via the OS Bluetooth UI **once**. Pair-keys live in the OS
keychain; WireDesk reuses them on every launch.

1. **Win11**: Settings → Bluetooth & devices → Add device → Bluetooth.
   Confirm Win11 BT radio is **on** and `Discoverable as "DESKTOP-…"`.
2. **Mac**: System Settings → Bluetooth (toggle on if needed). The Mac
   should see the Win11 host in `Nearby Devices`.
3. Click the Win11 device on the Mac → confirm the PIN on both sides
   → both should show `Connected` / paired in their respective panels.

Continent-АП on the Win11 host **does not block BLE** (verified live
2026-05-06): WFP filters operate on the IP/TCP/UDP stack; BT-radio runs
through a separate device-driver path that WFP doesn't see. As long as
your Continent endpoint policy permits BT (most do — BT mice/keyboards
work), the transport works.

## Switching transport

### Mac (WireDesk.app Settings panel)

1. Open WireDesk.app → Settings.
2. Connection group → **Transport** combo → pick `Bluetooth LE`.
3. Optional: edit `Peer name` (defaults to `WireDeskHost`) and
   `Connect timeout (s)` (default 30).
4. Click **Save & Restart**. WireDesk.app re-launches with the new
   transport. Status-bar log shows `opened transport: bluetooth-le-central`
   when scan + connect succeeds.

### Win11 (config.toml directly — UI deferred)

The Win nwg Settings panel doesn't yet expose the transport picker
(see `docs/plans/20260506-bluetooth-le-transport.md` Task 12 — deferred
follow-up `feat/bluetooth-host-ui`). For now, edit
`%APPDATA%\WireDesk\config.toml` manually:

```toml
transport = "bluetooth"

[bluetooth]
service_uuid = "cc7d466c-21f3-41ba-a711-991adf9f218e"
peer_name = "WireDeskHost"
mtu = 247
connect_timeout_secs = 30
reconnect_max_attempts = 0
```

Then restart `wiredesk-host.exe` from the tray menu. Host-log will show
`opened transport: bluetooth-le-peripheral` when advertising starts.

### Both sides

`service_uuid` and `peer_name` **must match** on Mac and Win11 — the
Mac scans for that exact UUID and filters by that exact name. The
defaults in `BluetoothConfig::default()` (`wiredesk-core`) are a single
source of truth so they don't drift; only edit them if you have two
WireDesk pairs in earshot of each other and need to disambiguate.

## Performance expectations (measured 2026-05-07)

| Workload              | BLE (measured)     | Serial baseline | Verdict          |
|-----------------------|--------------------|------------------|------------------|
| Mouse / keyboard      | usable             | smooth           | OK on BLE        |
| Small clipboard text  | ~50-200 ms / KB    | comparable       | OK on BLE        |
| 100 KB PNG image      | ~20-25 s           | ~10 s            | BLE 2× slower    |
| 500 KB PNG image      | ~100-110 s         | ~50 s            | BLE 2× slower    |
| 1+ MB PNG image       | unstable / timeout | ~90 s            | BLE not usable   |

- **Throughput:** ~4-5 KB/s sustained, both directions. *Slower than
  serial.*
- **Stability:** under sustained bidirectional load, btleplug 0.11
  occasionally tears down the CoreBluetooth event loop. UI surfaces
  this as "Disconnected: BLE send timeout" — relaunch needed.
- **Latency:** input events still feel close to serial after tuning
  (1/64 events pays an ATT-RTT for backpressure pacing — barely
  perceptible).
- **Auto-reconnect:** the `reconnect.rs` backoff helper is in place
  (Task 10) but the runtime hookup in `mac.rs` / `win.rs` is a
  follow-up — currently any disconnect (timeout, sleep-wake)
  requires manually relaunching the app. Tracked in
  `docs/plans/completed/20260506-bluetooth-le-transport.md`
  Post-Completion.

## Troubleshooting

### "BLE: no peer named 'WireDeskHost' advertising service ..."

The Mac scanned for the configured `peer_name` + `service_uuid` and
didn't find a matching peer within `connect_timeout_secs`. Check:
1. Win11 host running with `transport = "bluetooth"`? Tray menu →
   "Show Settings" — verify mode.
2. Win11 BT radio on? Settings → Bluetooth & devices → toggle.
3. Mac and Win paired? System Settings → Bluetooth → both should show
   `Connected` / paired.
4. Custom service UUID matches on both ends? Compare
   `bluetooth.service_uuid` in both config.toml files.

### Mac scan empty (no peripherals at all)

Usually a permission issue, not Continent. macOS requires Bluetooth
permission per app. Open **System Settings → Privacy & Security →
Bluetooth** and ensure WireDesk.app has the toggle on. If it's not
listed, the system permission prompt was missed at first launch — full
Quit (Cmd+Q) the app and re-launch; the prompt will reappear.

`Info.plist` has `NSBluetoothAlwaysUsageDescription` set so the prompt
shows on first launch.

### "BLE write timeout" on send

Either the link broke between scan and write, or Mac's BT radio is
saturated (sharing with another peer). Save & Restart on the Mac side
forces a fresh scan + connect.

### Continent endpoint policy blocks BT entirely

If your Continent installation has a DLP policy that disables BT (some
enterprise setups do), Plan C won't work at all — falls back to
serial. Verify by checking if any BT device works on the Win11 host.
If a BT mouse / keyboard pairs and works, BT-radio path is open.

## Architecture pointers

- Transport trait: `crates/wiredesk-transport/src/transport.rs` — sync
  `send/recv/is_connected/name/try_clone`. Both `SerialTransport` and
  `BluetoothLeTransport` implement it.
- Factory: `crates/wiredesk-transport/src/factory.rs::open_transport`
  picks impl by `cfg.transport`.
- Mac BLE Central: `crates/wiredesk-transport/src/bluetooth/mac.rs`
  via btleplug 0.11. Embedded tokio runtime (2 worker threads).
- Win BLE Peripheral: `crates/wiredesk-transport/src/bluetooth/win.rs`
  via windows-rs WinRT GATT. Same runtime pattern.
- Fragmentation: `crates/wiredesk-transport/src/bluetooth/fragment.rs`
  — 4-byte ChunkHeader (packet_id u16-le, chunk_idx u8, total_chunks
  u8). 240 bytes payload per chunk @ ATT MTU 247 (3-byte ATT header,
  4-byte ChunkHeader). Reassembler with per-packet_id bitmap and 5-s
  stale-sweep timeout.
- Reconnect helper: `crates/wiredesk-transport/src/bluetooth/reconnect.rs`
  — `next_backoff(attempt)` returns 0s → 2s → 4s → 8s → 16s → 30s.

## Related docs

- `docs/briefs/bluetooth-transport.md` — original brief.
- `docs/plans/20260506-bluetooth-le-transport.md` — implementation plan.
- `docs/briefs/ft232h-upgrade.md` — Plan A (parallel option).
- `docs/briefs/mac-auto-reconnect.md` — orthogonal process-level
  reconnect.
