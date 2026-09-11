# Bluetooth Transports

Two Bluetooth options exist. **Use `transport = "rfcomm"`** (Bluetooth
Classic, this section); the BLE transport further down is kept for
reference and as a fallback where Classic RFCOMM is unavailable.

## Bluetooth Classic (RFCOMM / SPP) — recommended

> **Status 2026-09-11:** implemented on both sides, static checks green on
> macOS and Windows (cross-compiled). **Live-verified only with probes** —
> a Swift IOBluetooth client on the Mac against a Winsock `AF_BTH`
> listener on the Win11 host, i.e. the exact API calls the transport
> makes, but not yet the built `wiredesk-host.exe` / `WireDesk.app`
> pair. First live run: rebuild the host on the Windows machine, flip both
> configs, watch for `opened transport: rfcomm-…` in both logs.

Measured on the reference pair (Mac M4 ↔ Win11 with Intel Wireless-AC 8265,
Bluetooth 4.2), 256 KB each way, pattern-checked:

| Metric                       | RFCOMM (probe)   | BLE (shipped)  | CH340 serial | FT232H @ 3 Mbaud |
|------------------------------|------------------|----------------|--------------|------------------|
| Throughput Win → Mac         | **~122 KB/s**    | ~4–5 KB/s      | ~11 KB/s     | ~300 KB/s        |
| Throughput Mac → Win         | **~124–129 KB/s**| ~4–5 KB/s      | ~11 KB/s     | ~300 KB/s        |
| Round-trip, 16-byte message  | p50 **10 ms**, p90 24 ms | —      | ~1 ms        | ~1 ms            |
| First packet after 1 s idle  | 21 ms            | —              | —            | —                |
| First packet after 3 s idle  | 82 ms (sniff mode) | —            | —            | —                |
| Connect time                 | ~0.4 s           | scan + connect | instant      | instant          |

Why Classic wins on this hardware: the host adapter is Bluetooth 4.2, so
BLE has no 2M PHY and is paced per connection event through two high-level
GATT stacks (WinRT notifications, CoreBluetooth writes), while Classic EDR
streams 2–3 Mbit/s ACL packets with credit-based flow control. The 1 MB
clipboard image that takes ~4 minutes on BLE takes ~9 s here.

### How it works

- **Host (Windows) = server.** `RfcommTransport` (`rfcomm/win.rs`) opens a
  Winsock `AF_BTH` / `BTHPROTO_RFCOMM` socket, binds a channel (OS-picked
  unless `rfcomm.channel` is set), publishes an SDP record under
  `rfcomm.service_uuid` with `WSASetService`, and accepts one client. The
  session thread sees `"recv timeout"` until someone connects — the same
  idle behaviour as an unplugged serial port. A client hanging up surfaces
  as a transport error, which the existing reopen loop handles (re-listen,
  re-publish, backoff).
- **Client (macOS) = client.** `rfcomm/mac.rs` on IOBluetooth: SDP query
  of the host (`rfcomm.peer_address`, or every paired device, computers
  first) → `getServiceRecordForUUID` → RFCOMM channel → async open →
  stream. IOBluetooth delivers every callback on the **main run loop**, so
  the transport signals worker threads through condvars and, if `open()`
  happens to run on the main thread before the app loop is up, pumps
  `CFRunLoopRunInMode` itself. (Sync open from a worker thread fails;
  async open and `writeSync` from workers are fine — probed 2026-09-11.)
- **Client (Windows)** is an ordinary Winsock RFCOMM client: SDP lookup via
  `WSALookupService` (`lpszContext = "(XX:XX:…)"`), `connect` with timeout.
  Unlike BLE, nothing collides with the host's role, so a Windows client
  works over Bluetooth. `rfcomm.peer_address` is required there.
- **Framing** is the serial COBS stream (`crates/wiredesk-transport/src/framing.rs`),
  so the wire is byte-identical to the cable and a lone `0x00` is a legal
  empty frame. The transport sends one every `rfcomm.keepalive_ms` (500) of
  silence to keep the ACL link out of sniff mode — that is what turns the
  82 ms first-packet latency into ~10 ms for mouse and keyboard.
- **Security:** the listener sets `SO_BTH_AUTHENTICATE` + `SO_BTH_ENCRYPT`
  (`rfcomm.require_encryption = true`), so Windows only accepts a paired,
  encrypted peer — the RFCOMM counterpart of the BLE `EncryptionRequired`.

### Setup

1. Pair the two machines once (System Settings → Bluetooth on the Mac,
   Settings → Bluetooth & devices on Win11). Pairing keys live in the OS.
2. Win11: Settings window → Transport → **Bluetooth Classic (RFCOMM)** →
   Save & Restart (or `transport = "rfcomm"` in `%APPDATA%\WireDesk\config.toml`).
   Host log: `RFCOMM: listening on channel N, SDP record … published` and
   `opened transport: rfcomm-server`.
3. Mac: Settings → Transport → **Bluetooth Classic (RFCOMM)**. Optionally
   fill *Host address* (`A0:B1:C2:D3:E4:F5` style — with several paired
   computers this skips the SDP round on each). Save & Restart. Client log:
   `RFCOMM: connected to <host name> [A0:B1:C2:D3:E4:F5], mtu 666`.

```toml
transport = "rfcomm"

[rfcomm]
service_uuid = "3d2df5cf-4f32-40c5-ab30-f1ccd6925b60"  # must match on both peers
peer_address = ""          # client: host BT address, empty = any paired computer
channel = 20               # fixed channel, same on both; 0 = SDP (macOS cannot read it)
connect_timeout_secs = 15
keepalive_ms = 500         # 0 = off
require_encryption = true  # host: SO_BTH_AUTHENTICATE + SO_BTH_ENCRYPT
```

### Live status (2026-09-11)

The first real run (Mac M4 / macOS 26 ↔ Win11, Intel 8265) only linked
with **both sides on `channel = 20`**, which is why 20 is now the default.
SDP does not work from this Mac at all: `performSDPQuery(_:uuids:)` never
calls its delegate back, and the plain `performSDPQuery(_:)` returns in
0 ms from a cache filled at pairing time — twelve stock records (CDP,
A2DP, AVRCP, Device ID), none of them ours, whichever channel the host
binds. So the client never learns the SDP-assigned number and the fixed
channel is the supported path; `channel = 0` stays in the config for a
peer whose SDP server does answer.

**Pairing and encryption hold.** The host runs with the shipped default
`require_encryption = true` (`SO_BTH_AUTHENTICATE` + `SO_BTH_ENCRYPT` on
the listening socket) and the Mac connects through it, so the earlier
connect timeout was the channel, not the secure-link options.

**Measured on the real apps (2026-09-11, after the two fixes below):**

| Path | Time |
|---|---|
| `wd --exec` round trip, trivial command | 0.8 s |
| 48 KB command, Mac to host | 3.1 s |
| 200 KB command, Mac to host | 4.5 s |
| 1 MB of output, host to Mac | 15.9 s |

Two things had to change to get there. `recv` polls every 10 ms rather
than 250, because the host only forwards shell output and clipboard
chunks between `recv` calls, so that interval paced the whole link (24
KB/s at 250 ms). And the Windows reader no longer sets `SO_RCVTIMEO`:
under load the Bluetooth provider answers a timed `recv` with
`ERROR_IO_PENDING` (997) instead of `WSAETIMEDOUT`, which the host took
for a fatal error and dropped the link on, every ~80 s of a large
transfer. A dedicated reader thread now blocks in `recv` with no timeout
and hands bytes over through a condvar.

The remaining cost of a large `wd --exec` is not the radio at all -
see the PowerShell note in `docs/wd-exec-usage.md`.

**The bundled `WireDesk.app` still does not get the Bluetooth prompt.**
It no longer hangs: `request_bluetooth_permission` (called from the
eframe creator callback, i.e. on the main thread with AppKit already
running) asks for the grant, and `ensure_bluetooth_authorized` fails the
open immediately instead of blocking inside
`IOBluetoothCoreBluetoothCoordinator`, so the link falls back to serial
and retries. But macOS shows no prompt for this bundle and writes no row
to `TCC.db`, with or without `tccutil reset BluetoothAlways
dev.kar43lov.wiredesk`, and with or without a `scanForPeripherals` call
to wake CoreBluetooth. Until that is understood, run the client binary
from a terminal:

```bash
target/release/WireDesk.app/Contents/MacOS/wiredesk-client
```

which inherits the terminal's own Bluetooth grant and connects in ~0.6 s.

### Troubleshooting

- **`no SDP record for service …`** — the host is not running with
  `transport = "rfcomm"`, or its SDP publish failed (host log `SDP register:
  WSA error …`). Escape hatch: set the same `channel = 20` on both sides;
  the client then skips SDP. Live 2026-09-11 this was the only way the
  Mac found the host at all — see *Live status* above.
- **`no Bluetooth device with address …`** — the address is not in the
  Mac's paired list. Pair first; leave `peer_address` empty to search.
- **Connect refused with `require_encryption`** — the pairing is stale on
  one side. Remove the device on both machines and pair again.
- **Mouse feels laggy after pauses** — `keepalive_ms` is 0. The default 500
  keeps the link awake.
- **Do not use the OS "Bluetooth serial port" COM ports / `/dev/cu.Bluetooth-Incoming-Port`.**
  Probed 2026-09-11: the Mac's incoming SPP tty receives fine (110 KB/s)
  but never transmits (1 KB buffer fills, then `EIO`), and Windows' legacy
  incoming COM port does not publish an SDP record the Mac can see. The
  transport talks to the RFCOMM APIs directly for exactly this reason.

---

## Bluetooth LE Transport (Plan C) — legacy fallback

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

Realistic positioning: BLE is a **last-resort fallback**. For a no-cable
link use `transport = "rfcomm"` (Bluetooth Classic, ~25× faster on the
same radios — see the top of this document); for the fastest link, FT232H
(Plan A). Root causes on this pair, established 2026-09-11: the host's
Intel 8265 is Bluetooth 4.2 (no 2M PHY), the connection interval is
chosen by macOS and cannot be requested from a WinRT peripheral, and
WinRT `NotifyValueAsync` completes per notification — none of which a
tuning pass on the crate can change.

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


---

## Итог замеров

## Channel speed upgrade

**SHIPPED & VERIFIED LIVE 2026-05-28.** Замена CH340 → **FT232H** на обеих сторонах null-modem'а подняла стабильный baud `115200 → 3_000_000` (×26), clipboard 1MB ~90 сек → ~3 сек. Никаких изменений в коде — только `baud = 3000000` в обоих `config.toml`. Hardware: два CJMCU-FT232H breakout (genuine FTDI, VID 0x0403 PID 0x6014), null-modem `AD0(TX) ↔ AD1(RX)` cross + GND, VCC изолированы. Windows требует **FTDI CDM driver** (https://ftdichip.com/drivers/vcp-drivers/) — без него COM-port не появляется в Ports (COM & LPT); macOS VCP встроен. Полный разбор + закрытые тупики (TCP/UDP режутся WFP-фильтрами Континента; Thunderbolt без TB-header'а на B760M не работает) + lessons learned — в `docs/briefs/ft232h-upgrade.md`. Plan B (Pi Zero 2W WinUSB bridge) остаётся как резерв на будущее **видео** по тому же каналу (USB 2.0 bulk ~30-40 MB/s).

**Деградация платы FT232H (эпизод 2026-06-16).** Если канал «отпадывает» постоянными штормами — частая первопричина не софт, а **деградировавшая плата** (TX-тракт одной из двух CJMCU-FT232H). Диагностика по сигнатуре в `client.log`: `COBS`/`CRC`/`bad magic` = порча битов (сигнал/baud на грани, лечится понижением baud); `Broken pipe`+`No such file` = USB-отвал (питание/контакт; `No such file` для `cu.usbserial-NNN` локализует именно Mac-сторону); `host link lost` при идущих `clipboard.send DONE` = асимметрия, бьётся TX одной стороны/жила провода/GND. Решающий тест «плата vs провод/окружение» — **swap двух плат местами**: если глюк переехал на другую сторону, виновата плата (в эпизоде swap вылечил канал даже на 3 Mbaud). **Рецидив 2026-07-20** (643 `host link lost` + 2066 `dropping bad frame` за день, детерминированный `COBS ... position 11` = систематический clock-skew, не шум): swap снова вылечил, но пользователь **переткнул контакты И swap'нул платы разом** → root-cause не изолирован (плохой контакт vs деградация платы — разные диагнозы). **Урок: при рецидиве менять по ОДНОМУ** (сначала только переткнуть контакт, при повторе — только swap плат), иначе не узнать виновника. И: 3 Mbaud по DuPont-проводам без экрана/согласования — эксплуатация «на грани», нулевой запас по сигналу; надёжный ход «чтобы не всплывало» — понизить baud до 1_000_000 (всё равно ×8–9 к CH340, но запас по джиттеру огромный). Триаж лога — `/pg.wd-log` (личный slash-command).
