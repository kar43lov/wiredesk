# Bluetooth LE Transport (Plan C)

WireDesk supports an alternative to the default USB-Serial channel:
**Bluetooth Low Energy** between the Mac client (Central) and the Win11
host (Peripheral). Throughput is ~5-9× faster than the default
CH340 @ 115200 baud (~30-100 KB/s vs ~11 KB/s) without buying any
hardware.

## When to use

| Channel       | Speed        | Hardware              | Setup time    |
|---------------|--------------|-----------------------|---------------|
| USB-Serial    | ~11 KB/s     | already have it       | already done  |
| Bluetooth LE  | ~30-100 KB/s | already have BT radio | one-time pair |
| FT232H @ 3M   | ~300 KB/s    | $20-30, must order    | wait + plug   |

Bluetooth wins on time-to-deliver (no shipping wait) and convenience
(no cable). FT232H wins on absolute speed. The two coexist — flip via
`config.toml`, restart, done.

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

## Performance expectations

- **Throughput:** sustained ~30-100 KB/s. BT 5.0 2M PHY peers (most
  Apple Silicon Macs, Win11 BT 5.x adapters) hit the upper end; older
  BT 4.x peers settle around 30-50 KB/s.
- **Clipboard:** a 1 MB PNG that takes ~90 s over CH340 @ 115200 takes
  ~10-30 s over BLE (×3-9 speedup).
- **Latency:** input events typically 5-15 ms one-way (vs ~3-5 ms over
  serial). Subjectively imperceptible for mouse/keyboard.
- **Auto-reconnect:** the `reconnect.rs` backoff helper is in place
  (Task 10) but the runtime hookup in `mac.rs` / `win.rs` is a
  follow-up — currently a sleep-wake cycle requires manually
  re-launching the app on Mac. Tracked in
  `docs/plans/20260506-bluetooth-le-transport.md` Post-Completion.

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
