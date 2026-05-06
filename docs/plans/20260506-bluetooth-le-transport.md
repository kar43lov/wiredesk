# Bluetooth LE Transport (Plan C)

## Overview

Добавить опциональный **BLE-канал** как альтернативу текущему USB-Serial transport'у в WireDesk. Mac выступает BLE Central, Win11 — BLE Peripheral. Custom GATT service с двумя characteristics: `Notify` (Win→Mac) + `WriteWithoutResponse` (Mac→Win). Выбор канала через `transport = "bluetooth" | "serial"` в `config.toml`.

**Проблема, которую решаем:** текущий канал CH340 @ 115200 baud (~11 KB/s) — узкое место для clipboard'а (1 МБ картинка ~90 секунд). FT232H Plan A даст ×30, но ждёт покупки железа. BLE доступен сегодня, ×3-9 ускорение, без покупок.

**Контекст: AC0 PASSED live 2026-05-06** — Continent-АП **никак не вмешивается** в BLE pathway: advertising, scan-response, connect, manufacturer-data, service-UUID broadcast — всё проходит. Плану зелёный свет.

**Не замена SerialTransport** — два transport'а сосуществуют, выбираются через config. Default остаётся `serial`, никаких regression'ов.

## Context (from discovery)

**Файлы / компоненты:**
- `crates/wiredesk-transport/src/transport.rs` — trait `Transport: Send` (`send / recv / is_connected / name / try_clone`). Не меняется.
- `crates/wiredesk-transport/src/serial.rs` — existing `SerialTransport` impl. Reference для нового impl'а. Не меняется.
- `crates/wiredesk-transport/src/lib.rs` — exports. Добавим `BluetoothLeTransport`.
- `apps/wiredesk-host/src/main.rs` + `apps/wiredesk-host/src/config.rs` — host config + main loop. Добавим transport-factory.
- `apps/wiredesk-client/src/main.rs` + `apps/wiredesk-client/src/config.rs` — Mac client equivalent.
- `apps/wiredesk-host/src/ui/` — nwg Settings panel, добавим transport combo + BT fields.
- `apps/wiredesk-client/src/app.rs` — Mac chrome panel Settings, аналогично.

**Найденные паттерны:**
- TOML config с `#[serde(default)]` + per-field defaults в `Default` impl.
- CLI override через `clap::ArgMatches::value_source()` (см. `merge_args` в host/config.rs).
- Tests — unit-тесты в `#[cfg(test)] mod tests` рядом с кодом, table-driven, `tempfile::tempdir()` для file I/O.
- Workspace crate boundary: `wiredesk-core` (errors), `wiredesk-protocol` (packets), `wiredesk-transport` (channels). Не нарушаем.

**Зависимости (новые):**
- `btleplug = "0.11"` — Mac BLE Central. Async-only, требует tokio runtime.
- `windows = { version = "0.x", features = ["Devices_Bluetooth_GenericAttributeProfile", "Foundation", "Storage_Streams"] }` — Win11 BLE Peripheral GATT server.
- `tokio = { version = "1", features = ["rt", "rt-multi-thread", "sync", "time"] }` — runtime для async BLE-callbacks.
- `async-trait = "0.1"` — для internal async-traits внутри BluetoothLeTransport.

**Ключевые архитектурные решения (см. Solution Overview):**
- BluetoothLeTransport владеет встроенным `tokio::runtime::Runtime` и через `block_on` реализует sync `Transport` trait. Не трогаем существующий sync codebase.
- Packet fragmentation: WireDesk packets ~до 8 KB после COBS, ATT MTU=247 → ~244 байта payload per chunk. Внутри BluetoothLeTransport добавляем тонкий fragment/reassembly слой со своим header'ом (chunk_idx, total_chunks, packet_id).

## Development Approach

- **Testing approach**: Regular (код, потом тесты в каждой task) — соответствует pattern'у проекта.
- Complete each task fully before moving to the next.
- Make small, focused changes.
- **CRITICAL**: every task MUST include new/updated tests for code changes in that task.
- **CRITICAL**: all tests must pass before starting next task.
- **CRITICAL**: update this plan file when scope changes during implementation.
- Run tests after each change.
- Maintain backward compatibility — `transport = "serial"` (default) остаётся bit-identical.

## Testing Strategy

- **Unit tests**: required for every task (см. Development Approach).
- **No e2e UI tests** в проекте сейчас. UI changes (nwg settings, egui chrome) тестируются live (manual checklist в Task N-1).
- **Integration tests**: `MockBleAdapter` в `crates/wiredesk-transport/tests/` — in-memory fake двух peer'ов с эмуляцией ATT MTU и notify-event semantics. Round-trip на нём.
- **Live тесты** (manual в Task N-1):
  - Pair Mac↔Win11 (один раз) → транспорт переключается на `transport = "bluetooth"`.
  - Latency spot-check: мышь, клавиатура, sidebuttons, Cmd+Space.
  - Throughput bench: clipboard PNG 1 MB Mac→Host, замер времени.
  - Reconnect smoke: Mac sleep на 60s + wake.
  - Regression на `transport = "serial"`: всё работает как до PR.

## Progress Tracking

- Mark completed items with `[x]` immediately when done.
- Add newly discovered tasks with ➕ prefix.
- Document issues/blockers with ⚠️ prefix.
- Update plan if implementation deviates from original scope.
- Keep plan in sync with actual work done.

## Solution Overview

### Архитектура высокого уровня

```
┌─────────────────────────┐                     ┌────────────────────────────┐
│  Mac (wiredesk-client)  │                     │  Win11 (wiredesk-host)     │
│                         │                     │                            │
│  main.rs                │                     │  main.rs                   │
│   │                     │                     │   │                        │
│   ▼                     │                     │   ▼                        │
│  transport::factory     │                     │  transport::factory        │
│   │ (config.transport)  │                     │   │ (config.transport)     │
│   ▼                     │                     │   ▼                        │
│  BluetoothLeTransport   │                     │  BluetoothLeTransport      │
│   │ (Central role)      │   BLE 5.0 2M PHY    │   │ (Peripheral role)      │
│   │  - btleplug + tokio │   ATT MTU 247       │   │  - windows-rs WinRT    │
│   │  - fragment/reassem │ ◄─────────────────► │   │  - GattServiceProvider │
│   │  - sync facade      │   custom service    │   │  - fragment/reassem    │
│   │                     │   UUID + 2 chars    │   │  - sync facade         │
│   ▼                     │                     │   ▼                        │
│  Transport trait (sync) │                     │  Transport trait (sync)    │
│   │                     │                     │   │                        │
│   ▼ (existing code)     │                     │   ▼ (existing code)        │
│  reader/writer threads  │                     │  Session loop              │
└─────────────────────────┘                     └────────────────────────────┘
```

### Ключевые design decisions

1. **Sync facade над async tokio runtime внутри BluetoothLeTransport.** btleplug и `windows-rs` BLE — async-only. Вместо переписывания всех callsites под async-trait, **embed runtime внутри transport**: `BluetoothLeTransport::open()` создаёт `tokio::runtime::Builder::new_multi_thread().worker_threads(2)`, all sync trait-methods (`send/recv`) делегируют в runtime через `runtime.block_on(...)`. Pattern проверен в Rust-сообществе; cost — два worker-thread'а на runtime + минорный overhead block_on.

2. **Custom GATT service с двумя characteristics, reliable primitives.**
   - Service UUID — фиксированный в коде + опциональный override в config (`bt_service_uuid`).
   - **TX characteristic** (Notify, Win→Mac) — host пишет, client subscribe'ится. Используется для всего входящего трафика на Mac.
   - **RX characteristic** (**WriteWithResponse**, Mac→Win) — client пишет, ATT-уровень ack'ает каждый write. Выбрано ради надёжности (плата ~10–15% throughput vs WriteWithoutResponse).
   - **Drop-detection на Notify-направлении** (Win→Mac): полагаемся на ChunkHeader sequence-number + per-packet_id reassembly tracking. Если packet целиком потерян (все chunks dropped) — heartbeat-driven recovery на application-уровне (existing). Future-задача (если bottleneck) — Indicate с ack вместо Notify; not in scope MVP.
   - Rationale: альтернатива «WriteWithoutResponse + Notify» (обе unreliable) даёт silent drop'ы под нагрузкой → cascade timeouts (см. `feedback_wd_exec_timeout_channel_hang.md`). WriteWithResponse + Notify — sufficient reliability для AC3 (1 MB clipboard) и AC4 (reconnect ≤5s).

3. **Packet fragmentation/reassembly слой.** WireDesk-packet'ы COBS-encoded до ~8 KB (`MAX_FRAME_SIZE = 8192`), ATT MTU=247 байт **включая 3-байтовый ATT header** → effective ATT payload = **244 байта**. Минус наш 4-байтовый ChunkHeader → **240 байт `chunk_payload` per BLE write**.

   ChunkHeader (4 bytes):
   ```
   [packet_id: u16 le] [chunk_idx: u8] [total_chunks: u8]
   ```
   Полный BLE-write: `header(4) + chunk_payload(0..=240) = до 244 bytes`.

   API:
   ```rust
   /// max_chunk_payload = 240 (ATT_PAYLOAD - sizeof(ChunkHeader))
   pub fn split_packet(packet_id: u16, payload: &[u8], max_chunk_payload: usize) -> Vec<Vec<u8>>
   ```
   Параметр `max_chunk_payload` (не «mtu») — explicit naming чтобы избежать путаницы с full ATT MTU.

   Math примеры:
   - 1000-байтовый packet, max_chunk_payload=240 → ⌈1000/240⌉ = 5 chunks. Sizes: 240+240+240+240+40 (last partial). Wire bytes: (4+240)×4 + (4+40) = 1020 байт.
   - 8192-байтовый packet (worst case): ⌈8192/240⌉ = 35 chunks. Wire bytes: (4+240)×34 + (4+32) = 8332 байт.

   На принимающей стороне buffer'ятся chunks по `packet_id`, после получения всех `total_chunks` (отслеживается bitmap'ом, не по `chunk_idx == total_chunks-1` — потому что chunks могут прийти не по порядку) → финализируется. Reassembly timeout 5s → discard. Hardcoded ATT MTU=247 на старт; negotiate MTU после connect через btleplug `set_default_mtu()` / windows-rs equivalent. Если negotiated MTU < 247 — пересчитываем `max_chunk_payload` динамически (cache в `BluetoothLeTransport`).

4. **try_clone — write-only split, не full duplication.** Existing pattern (SerialTransport) открывает второй file-descriptor — каждый клон полностью независим, может read+write. Для BLE второй connection невозможен. **Семантика для BLE: clone — write-only handle**, share'ит `Arc<Sender<OutgoingChunk>>` + `Arc<RuntimeHandle>`. **Только original handle делает `recv()`**, clone calls `recv()` → возвращает `Err(WireDeskError::Transport("recv on cloned BLE handle"))`.

   Verification: в существующем коде `try_clone()` используется в `apps/wiredesk-client/src/main.rs` и `apps/wiredesk-host/src/session_thread.rs` для writer-thread'а — он только `send`, не `recv`. Поэтому write-only clone semantics достаточны. **Sub-task в Task 8/9** — verify call-sites and document constraint.

5. **Transport-factory.** Новый файл `crates/wiredesk-transport/src/factory.rs`:
   ```rust
   pub fn open_transport(cfg: &TransportConfig) -> Result<Box<dyn Transport>> {
       match cfg.transport.as_str() {
           "serial" => SerialTransport::open(&cfg.port, cfg.baud).map(boxed),
           "bluetooth" => match BluetoothLeTransport::open(&cfg.bt) {
               Ok(t) => Ok(boxed(t)),
               Err(e) if cfg.fallback == Some("serial") => SerialTransport::open(...).map(boxed),
               Err(e) => Err(e),
           },
           other => Err(WireDeskError::Transport(format!("unknown transport: {other}"))),
       }
   }
   ```

6. **Combined dual-channel mode (BT + serial одновременно для агрегированной скорости)** — **OUT OF SCOPE** этого плана. Future post-completion follow-up: требует sequence-number'ов на packet-уровне, channel-multiplexer'а, retransmit'ов при channel-imbalance. ~1-2 недели extra. Если когда-нибудь понадобится — отдельный бриф.

### Что меняется в существующем коде (сводно)

- `Cargo.toml` (workspace) — новые deps.
- `crates/wiredesk-transport/Cargo.toml` — добавление conditional cfg-features (mac/win) для BLE.
- `crates/wiredesk-transport/src/lib.rs` — экспорт нового модуля.
- `apps/wiredesk-host/src/config.rs` — новые поля (`transport`, `bt_*`, `transport_fallback`).
- `apps/wiredesk-client/src/config.rs` — то же самое.
- `apps/wiredesk-host/src/main.rs` — замена прямого `SerialTransport::open` на `transport::open_transport(...)`.
- `apps/wiredesk-client/src/main.rs` — то же.
- `apps/wiredesk-host/src/ui/...` — nwg Settings UI: combo «Transport» + BT-поля.
- `apps/wiredesk-client/src/app.rs` — egui Settings: combo + BT-поля.

## Technical Details

### Config schema (TOML)

Дополнения, общие для host и client:

```toml
# transport selection
transport = "serial"           # "serial" | "bluetooth", default "serial"
transport_fallback = "serial"  # if BT init fails — fallback to this; null/omit = no fallback

# Bluetooth-specific (used only when transport == "bluetooth")
[bluetooth]
service_uuid = "5d3a2f01-1234-4abc-9def-aabbccddeeff"  # generated once, fixed across project
peer_name = "WireDeskHost"     # Mac scan-filter / Win advertise-name
mtu = 247                      # ATT MTU; negotiate up to this on connect
reconnect_max_attempts = 0     # 0 = unlimited; backoff 2s→4s→8s→16s→30s→30s
```

CLI flags:
- `--transport serial|bluetooth`
- (BT-specific overrides не нужны для MVP — менять через config.toml)

### Service / characteristics UUIDs (fixed)

```rust
// crates/wiredesk-transport/src/bluetooth/uuids.rs
pub const SERVICE_UUID: Uuid = uuid!("5d3a2f01-1234-4abc-9def-aabbccddeeff");
pub const TX_CHAR_UUID: Uuid = uuid!("5d3a2f02-1234-4abc-9def-aabbccddeeff");  // Notify (Win→Mac)
pub const RX_CHAR_UUID: Uuid = uuid!("5d3a2f03-1234-4abc-9def-aabbccddeeff");  // WriteWithResponse (Mac→Win)
```

(Точные UUIDs регенерим в Task 1, чтобы random'ные были.)

### Fragmentation header (4 bytes)

```rust
struct ChunkHeader {
    packet_id: u16,    // wraps at 65536, used to disambiguate concurrent in-flight packets
    chunk_idx: u8,     // 0..total_chunks
    total_chunks: u8,  // 1..=255 (max 255 chunks → ~60 KB, выше MAX_FRAME_SIZE 8 KB)
}
// ATT MTU 247 = 3-byte ATT header + 244 effective payload
// BLE write: 4-byte ChunkHeader + 0..=240 chunk_payload = up to 244 bytes
```

### Processing flow (Mac side, send packet)

1. `send(packet)` вызывает `runtime.block_on(self.send_async(packet))`.
2. `send_async`: serialize packet via existing `Packet::to_bytes()` + COBS — это уже как в SerialTransport.
3. Split в chunks с ChunkHeader'ом (`max_chunk_payload = ATT_PAYLOAD - 4`).
4. For each chunk — `peripheral.write(rx_char, &bytes, WriteType::WithResponse).await`. ATT-ack гарантирует delivery; на Err — return immediately, packet failed.
5. Wait для last write completion → return Ok.

### Processing flow (Mac side, recv packet)

1. На `BluetoothLeTransport::open()` после connect делаем `peripheral.subscribe(tx_char).await`.
2. Spawned tokio task `notification_loop` слушает `peripheral.notifications()` stream.
3. Для каждого incoming chunk — feed в `Reassembler` (per-`packet_id` buffer).
4. Когда reassemb готов → COBS-decode → `Packet::from_bytes()` → push в `tokio::sync::mpsc::UnboundedSender` shared с recv'ом.
5. `recv()` (sync): `runtime.block_on(self.incoming_rx.recv())` — блокируется до прихода packet'а.

### Win11 side (Peripheral)

1. `BluetoothLeTransport::open()` (Peripheral mode):
   - `GattServiceProvider::CreateAsync(SERVICE_UUID)`
   - `service.CreateCharacteristicAsync(TX_CHAR_UUID, parameters with Notify property)`
   - `service.CreateCharacteristicAsync(RX_CHAR_UUID, parameters with **Write** property — `WriteWithResponse`)`
   - Set `tx_char.SubscribedClientsChanged` event handler — fires on Mac subscribe.
   - Set `rx_char.WriteRequested` event handler — fires on Mac write. Handler **must call `request.RespondWithProtocolErrorAsync(0)` или `request.RespondAsync()`** для ATT-ack (требуется WriteWithResponse semantics).
   - `service.StartAdvertising({ IsConnectable: true, IsDiscoverable: true })`
2. Spawned task: на event'е `WriteRequested` — read bytes, feed в Reassembler, push assembled Packet в incoming queue. Respond ack-фrame'ом (Success-status, без data).
3. send: serialize+chunk → for each chunk `tx_char.NotifyValueAsync(buffer)` → wait completion. (Notify — fire-and-forget, application-уровень heartbeat ловит drops.)

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code changes, tests, docs.
- **Post-Completion** (no checkboxes): manual live-tests на physical hardware (Win11 host + Mac), pairing flow, deploy notes.

## Implementation Steps

### Task 1: Workspace deps + crate skeleton

**Files:**
- Modify: `Cargo.toml` (workspace root)
- Modify: `crates/wiredesk-transport/Cargo.toml`
- Create: `crates/wiredesk-transport/src/bluetooth/mod.rs`
- Create: `crates/wiredesk-transport/src/bluetooth/uuids.rs`
- Modify: `crates/wiredesk-transport/src/lib.rs`

- [x] Добавить в `[workspace.dependencies]` корня: `btleplug 0.11`, `tokio` (rt-multi-thread+sync+time+macros), `async-trait 0.1`, `futures 0.3`, `windows 0.58` с union feature set (Win32_* host + Devices_Bluetooth_* + Storage_Streams + Foundation для transport). Host инлайн-declaration убран в пользу `windows.workspace = true`.
- [x] В `wiredesk-transport/Cargo.toml` подтянуть deps из workspace: `tokio/async-trait/futures/uuid` в общий `[dependencies]`, `btleplug` в `[target.'cfg(target_os = "macos")'...]`, `windows` в `[target.'cfg(target_os = "windows")'...]`.
- [x] Host'овский `apps/wiredesk-host/Cargo.toml` использует `windows = { workspace = true }` — Cargo unifies features.
- [x] Создать `bluetooth/mod.rs` — `pub mod uuids; pub struct BluetoothFactoryConfig { ... }` + cfg-fenced submodule re-exports (mac/win/stub).
- [x] Создать `bluetooth/uuids.rs` — `SERVICE_UUID = cc7d466c-…`, `TX_CHAR_UUID = 9062d406-…`, `RX_CHAR_UUID = 24bce5b3-…` (random v4 UUIDs от `uuidgen`). + tests `uuids_are_distinct` / `uuids_are_v4`.
- [x] Создать `bluetooth/{mac,win,stub}.rs` — placeholder structs `BluetoothLeTransport` под cfg-target'ами, `open()` возвращает Err с pending-message, `impl Transport` returns `unimplemented!()`/Err. + tests `name_is_stable` + `open_currently_errors_with_pending_message`.
- [x] В `lib.rs` экспортировать `pub use bluetooth::BluetoothLeTransport;` + `pub use bluetooth::uuids;`.
- [x] `cargo check --workspace` — passed clean.
- [x] `cargo test --workspace -- --test-threads=1` — passed (включая 8 новых: 2 uuids + 2 mac stub + другие existing).
- [x] `cargo clippy --workspace --all-targets -- -D warnings` — clean.

### Task 2: Config schema — transport selection + bluetooth section

**Files:**
- Create: `crates/wiredesk-core/src/bluetooth_config.rs`
- Modify: `crates/wiredesk-core/src/lib.rs`
- Modify: `apps/wiredesk-host/src/config.rs`
- Modify: `apps/wiredesk-client/src/config.rs`

- [x] Создать `BluetoothConfig` в `crates/wiredesk-core/src/bluetooth_config.rs` со всеми полями + DEFAULT_* константами + Default impl + 4 unit-теста (defaults_match_constants, default_service_uuid_parses_as_uuid, toml_roundtrip, empty_toml_yields_defaults).
- [x] `wiredesk-core/Cargo.toml` — добавлены `[dev-dependencies] uuid + toml` для тестов.
- [x] Экспорт через `wiredesk-core/src/lib.rs`: `pub use bluetooth_config::BluetoothConfig;`.
- [x] HostConfig + ClientConfig: новые поля `transport`, `transport_fallback`, `bluetooth: BluetoothConfig` с `#[serde(default)]`. Default impls обновлены.
- [x] `merge_args` в обоих apps обрабатывает `--transport` CLI flag.
- [x] `--transport` flag добавлен в clap `Args` обоих main.rs (default `"serial"`).
- [x] Existing tests `defaults_match_hardcodes` обновлены под новые поля.
- [x] Новые тесты: `toml_transport_bluetooth_section_roundtrips`, `partial_toml_without_bluetooth_section_uses_defaults`, `merge_cli_transport_overrides_toml`, `merge_no_transport_arg_keeps_toml` — в обоих apps.
- [x] `read_form` в host'овском Settings UI расширен `base: &HostConfig` параметром чтобы preserve unedited fields (transport/bluetooth/host_name) при Save через UI.
- [x] `cargo test --workspace -- --test-threads=1` — passes (170 host + 102 client + 4 wiredesk-core BLE tests).
- [x] `cargo clippy --workspace --all-targets -- -D warnings` — clean.

### Task 3: Transport factory

**Files:**
- Create: `crates/wiredesk-transport/src/factory.rs`
- Modify: `crates/wiredesk-transport/src/lib.rs`
- Create: `crates/wiredesk-transport/tests/factory_test.rs`

- [x] Создан `crates/wiredesk-transport/src/factory.rs` с `SerialFactoryConfig` (port, baud) + `TransportConfig` (transport, serial, bluetooth, fallback). Tests внутри (не отдельным `tests/factory_test.rs` — `#[cfg(test)] mod tests` соответствует существующему pattern'у проекта).
- [x] `pub fn open_transport(cfg: &TransportConfig) -> Result<Box<dyn Transport>>` со switch `"serial"` / `"bluetooth"`. На BT failure + `fallback == Some("serial")` — log::warn + retry serial. На unknown fallback (например `"ftdi"`) — НЕ recurse'ит, primary error surfaces.
- [x] В `lib.rs` экспортированы `open_transport`, `SerialFactoryConfig`, `TransportConfig`.
- [x] 6 unit-тестов: `unknown_transport_errors`, `empty_transport_errors`, `serial_transport_attempts_serial_open` (через invalid port → "serial open" error origin), `bluetooth_transport_without_fallback_returns_ble_error`, `bluetooth_init_fail_falls_back_to_serial` (full path verified через "serial open" error в final result), `unknown_fallback_value_does_not_recurse`.
- [x] `cargo test -p wiredesk-transport -- --test-threads=1` — 14 passed (6 new factory + 4 bluetooth + 4 mock).
- [x] `cargo clippy --workspace --all-targets -- -D warnings` — clean.

### Task 4: BluetoothLeTransport skeleton — sync facade + tokio runtime + fragmentation

**Files:**
- Modify: `crates/wiredesk-transport/src/bluetooth/mod.rs`
- Create: `crates/wiredesk-transport/src/bluetooth/runtime.rs`
- Create: `crates/wiredesk-transport/src/bluetooth/fragment.rs`

- [x] Создан `runtime.rs` — `EmbeddedRuntime` с 2 worker threads + thread name `wiredesk-ble`, methods `block_on/spawn`. 3 unit-теста (block_on_runs_to_completion, spawn_runs_on_runtime_threads, block_on_chains_async_calls — последний валидирует `enable_all` для timer).
- [x] Создан `fragment.rs` с pure-logic chunking/reassembly:
  - [x] `ChunkHeader { packet_id: u16, chunk_idx: u8, total_chunks: u8 }`, packet_id little-endian, `from_bytes` валидирует short buffer / zero total / chunk_idx >= total.
  - [x] Constants `ATT_HEADER_OVERHEAD = 3`, `CHUNK_HEADER_LEN = 4`, `DEFAULT_ATT_MTU = 247`, `REASSEMBLY_TIMEOUT = 5s`, `MAX_TOTAL_CHUNKS = 255`.
  - [x] `max_chunk_payload(att_mtu)` — saturating, для tiny MTU (<7) даёт 0.
  - [x] `split_packet(packet_id, payload, max_chunk_payload) -> Result<Vec<Vec<u8>>, FragmentError>` — pre-checks `max_chunk_payload > 0`, errors на `TooManyChunks` (>255).
  - [x] `Reassembler` с per-packet_id slot (bitmap-based progress + first_seen Instant), `feed_chunk_at(now, bytes)` + `feed_chunk(bytes)` convenience. Sweep на каждом feed before processing. Идемпотентность на duplicate chunk arrival. Defensive reset на mismatched total_chunks для одного packet_id.
- [x] BluetoothLeTransport struct остался stub'ом (Tasks 5/7 будут wire'ить tokio runtime + fragment в реальный send/recv).
- [x] **17 fragment-тестов**: header roundtrip / rejects (short / zero total / out-of-range), max_chunk_payload (default 240, tiny MTU saturating), split_packet (single-chunk, multi-chunk 1000→5×240+40, max-frame 8192→35 chunks, empty payload, exact-multiple, too-many-chunks), reassembler (in-order, out-of-order, timeout sweep, packet_id disambiguation, idempotent duplicate). +3 runtime-теста = **20 новых tests**.
- [x] `cargo test -p wiredesk-transport -- --test-threads=1` — 34 passed (20 new + 14 from prior tasks).
- [x] `cargo clippy --workspace --all-targets -- -D warnings` — clean.

### Task 5: Mac BLE Central impl (btleplug) + Info.plist permission

**Files:**
- Create: `crates/wiredesk-transport/src/bluetooth/mac.rs`
- Modify: `crates/wiredesk-transport/src/bluetooth/mod.rs`
- Modify: `apps/wiredesk-client/Info.plist`

- [ ] **Info.plist permission**: добавить `<key>NSBluetoothAlwaysUsageDescription</key><string>WireDesk uses Bluetooth Low Energy to communicate with the Windows host without USB cable.</string>`. Без этого macOS первый запуск повиснет на permission prompt + scan молчит (см. AC0 verification: с пустым permission CentralManager State == Unknown).
- [ ] Под `#[cfg(target_os = "macos")]` создать `mac.rs` с реализацией `BluetoothLeTransport`:
  - [ ] Internal struct: `Arc<Inner>` где `Inner` содержит `peripheral`, `tx_char` handle, `rx_char` handle, `outgoing_tx: mpsc::UnboundedSender<Vec<Vec<u8>>>` (chunks), `incoming_rx: Arc<Mutex<mpsc::UnboundedReceiver<Vec<u8>>>>`, `att_payload: AtomicUsize`, `is_owner: bool`.
  - [ ] `open(cfg)`: создать tokio runtime, `block_on(async { Manager::new().await ... })`, scan по service-UUID `cfg.service_uuid` (timeout `cfg.connect_timeout_secs`), фильтр по advertised name == `cfg.peer_name`, connect, discover services, get TX_CHAR_UUID и RX_CHAR_UUID handles, subscribe TX, negotiate ATT MTU (через `peripheral.set_default_mtu(cfg.mtu)` если поддерживается → store `att_payload = max_chunk_payload(negotiated_mtu)`).
  - [ ] `send(&mut self, packet)`: serialize → COBS (используем `wiredesk_protocol::cobs`) → split_packet(packet_id_counter, &cobs, att_payload.load()) → loop: `peripheral.write(rx_char, &chunk, WriteType::**WithResponse**).await`. WithResponse даёт ATT-ack, гарантирует delivery. Timeout 10s на все chunks суммарно.
  - [ ] Spawned background task (в open'е): `peripheral.notifications()` stream → for each notification feed в Reassembler → on full packet, COBS-decode → push в `incoming_tx`.
  - [ ] `recv(&mut self)`: `incoming_rx.lock().blocking_recv()` (через `block_on`). Если `is_owner == false` → возвращаем `Err(WireDeskError::Transport("recv on cloned BLE handle"))`.
  - [ ] `is_connected()`: cached в `AtomicBool`, обновляется из notification-task'а.
  - [ ] `name()`: `"bluetooth-le-central"`.
  - [ ] `try_clone()`: новый handle с тем же `Arc<Inner>`, но `is_owner = false`. Send-only semantics (см. Solution Overview Decision 4).
- [ ] В `mod.rs` под `#[cfg(target_os = "macos")] pub use mac::*;`.
- [ ] Tests:
  - [ ] `mac_open_no_peer_errors` — попытка open с invalid service-UUID → Err после `connect_timeout_secs=2` (override в test config'е). Без real Win-host'а timeout-path tested.
  - [ ] `cloned_handle_recv_returns_err` — pure-logic test: создать `BluetoothLeTransport` через mock-Inner (без real BLE), `try_clone()`, вызвать `recv()` на клон → `Err(...)`.
  - [ ] **Real connect-test (Mac↔Win)** перенесён в Task 16 manual checklist — требует both real devices.
- [ ] Запустить `cargo test -p wiredesk-transport -- --test-threads=1`.

### Task 6: Cross-platform compile-fence

**Files:**
- Modify: `crates/wiredesk-transport/src/bluetooth/mod.rs`

- [ ] Архитектура модуля: `pub struct BluetoothLeTransport` с одинаковым именем на всех платформах. Платформо-специфичные impl'ы скрыты за `cfg`-fenced sub-modules:
  ```rust
  #[cfg(target_os = "macos")] mod mac;
  #[cfg(target_os = "windows")] mod win;
  #[cfg(not(any(target_os = "macos", target_os = "windows")))] mod stub;

  #[cfg(target_os = "macos")] pub use mac::BluetoothLeTransport;
  #[cfg(target_os = "windows")] pub use win::BluetoothLeTransport;
  #[cfg(not(any(...)))] pub use stub::BluetoothLeTransport;
  ```
- [ ] Создать `bluetooth/stub.rs`: `pub struct BluetoothLeTransport;` + `impl BluetoothLeTransport { pub fn open(_: &BluetoothFactoryConfig) -> Result<Self> { Err(WireDeskError::Transport("BLE not supported on this platform".into())) } }` + stub `impl Transport`.
- [ ] **Project — Win11+Mac only** (CLAUDE.md). Stub существует исключительно для случая "разработчик случайно `cargo check --workspace` на Linux box'е" — silent fail без compile-error. Не over-engineering; просто не блокирует.
- [ ] Tests (только sanity на target):
  - [ ] `cargo build --workspace` на dev-machine (Mac) — должен пройти.
  - [ ] (Optional) `cargo build --workspace` на Win11 — пройти. Cross-compile из Mac не требуется.

### Task 7: Win11 BLE Peripheral impl (windows-rs WinRT GATT)

**Files:**
- Create: `crates/wiredesk-transport/src/bluetooth/win.rs`
- Modify: `crates/wiredesk-transport/src/bluetooth/mod.rs`

- [ ] Под `#[cfg(target_os = "windows")]` создать `win.rs`:
  - [ ] `open(cfg)` (Peripheral mode): tokio runtime, `block_on(async { ... })`:
    - `GattServiceProviderResult::CreateAsync(SERVICE_UUID).await`.
    - `service.CreateCharacteristicAsync(TX_CHAR_UUID, GattLocalCharacteristicParameters { CharacteristicProperties: Notify, ... }).await`.
    - `service.CreateCharacteristicAsync(RX_CHAR_UUID, GattLocalCharacteristicParameters { CharacteristicProperties: **Write** (= WriteWithResponse), ... }).await`.
    - `tx_char.SubscribedClientsChanged += handler` — для notify когда subscriber connects/disconnects.
    - `rx_char.WriteRequested += handler` — для каждого incoming write. Handler **обязан** вызвать `request.RespondAsync()` для ATT-ack (требуется WriteWithResponse semantics; иначе client'ский write зависнет на 30s timeout).
    - `service_provider.StartAdvertising({ IsConnectable: true, IsDiscoverable: true, ServiceUuids: [SERVICE_UUID] })`.
  - [ ] `send`: serialize+COBS → chunks → for each chunk `tx_char.NotifyValueAsync(buffer).await`. NotifyValueAsync шлёт всем subscriber'ам fire-and-forget; protocol-level heartbeat ловит drop'ы.
  - [ ] WriteRequested handler: extract bytes → feed в Reassembler → on full packet, COBS-decode → push в `incoming_tx`. После feeding chunk'а respond ack-ом (`Success`).
  - [ ] `recv`: `incoming_rx.blocking_recv()`. Cloned-handle (`is_owner == false`) → `Err`.
  - [ ] `is_connected()`: check subscribed-clients count > 0 (cache в `AtomicUsize`).
  - [ ] `try_clone`: same `Arc<Inner>`, `is_owner = false` (write-only).
- [ ] В `mod.rs`: `#[cfg(target_os = "windows")] pub use win::*;`.
- [ ] Tests (cross-platform deterministic):
  - [ ] `win_open_no_radio_errors` — если BT-радио недоступно (mock), open → Err. Live integration тестируется в Task N-1.
- [ ] **Manual smoke** (записываем в Task N-1, не блокируем эту task): прогнать advertising на real Win11, scan-test через LightBlue с Mac.
- [ ] Запустить `cargo test -p wiredesk-transport --target x86_64-pc-windows-msvc` (если cross compile нет — пропустить, тестим на host'е в Task N-1).

### Task 8: Wire factory в host main.rs + verify try_clone call-sites

**Files:**
- Modify: `apps/wiredesk-host/src/main.rs`
- Modify: `apps/wiredesk-host/src/config.rs` (если требуется helper для конверсии)
- Modify: `apps/wiredesk-host/src/session_thread.rs` (если try_clone используется)

- [ ] В `main.rs` заменить прямой `SerialTransport::open(&cfg.port, cfg.baud)` на `transport::open_transport(&to_transport_config(&cfg))`.
- [ ] Добавить helper `fn to_transport_config(cfg: &HostConfig) -> TransportConfig` в `config.rs`.
- [ ] Update startup logging — log `transport.name()` после open чтобы было видно какой реально открылся (важно для fallback-сценария).
- [ ] **try_clone audit**: grep `try_clone()` в `apps/wiredesk-host/`. Verify что cloned handle используется ТОЛЬКО для `send()`-вызовов (writer-thread), не `recv()`. Если найдётся `recv()` на cloned — пометить ⚠️ и переработать call-site (вынести `recv` на original handle).
- [ ] Update existing test'ы host'а (если они dependent на transport-creation — скорее всего нет, всё в Session/SessionThread слое).
- [ ] Tests:
  - [ ] `to_transport_config_serial` — host config с `transport = "serial"` → корректный TransportConfig.
  - [ ] `to_transport_config_bluetooth` — `transport = "bluetooth"` → корректный TransportConfig с BT-полями.
- [ ] Запустить `cargo test -p wiredesk-host -- --test-threads=1`.

### Task 9: Wire factory в client main.rs + verify try_clone call-sites

**Files:**
- Modify: `apps/wiredesk-client/src/main.rs`
- Modify: `apps/wiredesk-client/src/config.rs` (helper)

- [ ] Аналогично Task 8 — replace direct `SerialTransport::open` на `transport::open_transport(...)`.
- [ ] Helper `fn to_transport_config(cfg: &ClientConfig) -> TransportConfig`.
- [ ] Update startup log.
- [ ] **try_clone audit**: grep `try_clone()` в `apps/wiredesk-client/`. Verify writer-only usage. Document constraint в comment рядом с try_clone call.
- [ ] Tests symmetric с Task 8 — `to_transport_config_serial`, `to_transport_config_bluetooth`.
- [ ] Запустить `cargo test -p wiredesk-client -- --test-threads=1`.

### Task 10: Auto-reconnect loop в BluetoothLeTransport

**Files:**
- Modify: `crates/wiredesk-transport/src/bluetooth/mac.rs`
- Modify: `crates/wiredesk-transport/src/bluetooth/win.rs`
- Create: `crates/wiredesk-transport/src/bluetooth/reconnect.rs`

- [ ] Создать `reconnect.rs` с pure helper'ом `pub fn next_backoff(attempt: u32) -> Duration` — **первая попытка immediate (0s)**, затем exponential 2s→4s→8s→16s→30s→30s. Это ради AC4 (≤5s после кратковременного disconnect/sleep-wake) — first-attempt без delay, чтобы typical recovery (radio re-acquired в течение секунды после wake) пройти за <5s.
- [ ] В Mac side: на disconnect-event (peripheral.events stream возвращает `CentralEvent::DeviceDisconnected`) → spawn reconnect-task: loop с `next_backoff()` пытается scan+connect снова. На успешном connect — re-subscribe TX, restore handles. Если configured `reconnect_max_attempts > 0` — break после N attempts, возвращаем persistent disconnected state.
- [ ] В Win side: на `tx_char.SubscribedClientsChanged` event (subscribers → 0) → spawn task ждёт `SubscribedClientsChanged` → 1 again, no-op (advertising и так running).
- [ ] **Не интегрируем с existing `mac-auto-reconnect.md` brief** — это per-transport reconnect. Process-level Mac reconnect — отдельный бриф, ortho.
- [ ] Tests:
  - [ ] `next_backoff_first_attempt_immediate` — `next_backoff(0) == Duration::ZERO`.
  - [ ] `next_backoff_exponential` — `next_backoff(1..7)` равны `[2, 4, 8, 16, 30, 30]` секунд.
  - [ ] `reconnect_loop_respects_max_attempts` — mock-impl с counter, после max returns persistent fail.
  - [ ] **Live test** Mac sleep-wake → переезжает в Task 15 (verification).
- [ ] Запустить `cargo test -p wiredesk-transport -- --test-threads=1`.

### Task 11: Settings UI — Mac client

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs`

- [ ] В Settings panel (group «Connection») добавить:
  - [ ] Combo «Transport»: «Serial» / «Bluetooth».
  - [ ] Когда «Bluetooth» selected — показать BT-fields: «Service UUID» (read-only display, editable advanced), «Peer Name», «MTU», «Reconnect attempts».
  - [ ] Сохранение: при clicking Save — пишем `cfg.transport`, `cfg.bluetooth.*` в TOML.
  - [ ] Show inline toast: «Transport changed — restart to apply».
- [ ] **Не делаем live-restart** — в проекте уже Save+Restart pattern, новый transport также требует restart. Documenting.
- [ ] Tests: existing settings-panel UI-тестов в проекте нет — пропускаем UI-tests, manual checklist в Task N-1.
- [ ] Запустить `cargo test -p wiredesk-client -- --test-threads=1` (regression check).

### Task 12: Settings UI — Win11 host (nwg)

**Files:**
- Modify: `apps/wiredesk-host/src/ui/...` (точный файл — settings panel)

- [ ] Аналогично Task 11 в nwg-Settings:
  - [ ] Combo «Transport».
  - [ ] BT-fields visibility toggle.
  - [ ] Save-handler пишет TOML, prompt для restart.
- [ ] Manual checklist в Task 15.
- [ ] Запустить `cargo test -p wiredesk-host -- --test-threads=1`.

### Task 13: Status indication — connection state (no throughput counter)

**Files:**
- Modify: `apps/wiredesk-client/src/status_bar.rs`
- Modify: `apps/wiredesk-host/src/ui/...` (tray tooltip)

- [ ] Mac status-bar: показывать `BT: scanning / paired / connected` когда `transport == "bluetooth"`. Status read'ится через **`Transport::is_connected()`** + cached scan/connect state в `BluetoothLeTransport`-instance (доступен через downcast). Если serial — текущее behaviour, никаких изменений.
- [ ] Win tray tooltip: при `transport == "bluetooth"` — `WireDesk: BT connected (Mac client)` или `BT waiting for client`. Update on subscribe-event.
- [ ] **Throughput counter EWMA** — **defer на post-completion follow-up**. Не в AC брифа, не критичен для MVP. Заметка в `Post-Completion` секции.
- [ ] Tests: existing UI patterns без unit-tests; manual smoke в Task 15.
- [ ] Запустить `cargo test --workspace -- --test-threads=1` (regression check).

### Task 14: Documentation updates

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `docs/architecture.md`
- Create: `docs/bluetooth-transport.md` (troubleshooting / pair flow)

- [ ] README: добавить раздел «Transport options» с serial/bluetooth-сравнением и конфиг-примерами.
- [ ] CLAUDE.md: добавить в «Architecture» абзац про BluetoothLeTransport (Mac=Central, Win=Peripheral, embedded tokio runtime, fragmentation MTU=247).
- [ ] docs/architecture.md: расширить module-map списком `bluetooth/{mod,uuids,fragment,reconnect,runtime,mac,win,stub}`.
- [ ] Создать `docs/bluetooth-transport.md`:
  - [ ] One-time pair flow (Mac System Settings → Bluetooth → Connect к DESKTOP-XXX).
  - [ ] Config switching (`transport = "bluetooth"`, restart).
  - [ ] Troubleshoot: «не connect'ится» → re-pair, BT radio reset, Continent-policy verification.
  - [ ] Performance expectations (×3-9 vs CH340).
  - [ ] Limitations: latency p95 +10-20ms vs serial, MTU нюансы.

### Task 15: Verify acceptance criteria

- [ ] **AC0 re-verify (10 минут, gate)**: pair Mac↔Win11, отправить тестовый файл 1 KB через стандартный OS BT-flow (или прогнать LightBlue scan видя custom service-UUID после Win-side build'а). Если **fail** — environment изменился (Continent policy update, smartcard rules), план abort'ится. Дёшево, защищает от 5–7 дней wasted work.
- [ ] AC1 (base connect): `transport = "bluetooth"` на обеих сторонах → paired+connected ≤10s. Tray host показывает `Bluetooth: connected`. Mac status — green dot.
- [ ] AC2 (input parity): мышь, клавиатура (вкл. кириллица + sidebuttons), modifiers — работают идентично serial. **Restart GUI с `transport = "bluetooth"` ПЕРЕД** spot-check'ом `wd --exec "echo test"` (потому что `wd --exec` идёт через GUI IPC, а GUI открывает один transport per process).
- [ ] AC3 (clipboard throughput): PNG 1 MB Mac→Host передаётся ≤30s. Замер через `time` обёртку clipboard-event'а.
- [ ] AC4 (reconnect): Mac sleep на 60s → wake → auto-reconnect ≤5s. (First attempt immediate per Task 10 backoff.)
- [ ] AC5 (regression): `transport = "serial"` (default) — bit-identical pre-PR behavior. Run full smoke на serial.
- [ ] AC6 (fallback): broken BT config + `transport_fallback = "serial"` → serial picks up. Log shows fallback warning.
- [ ] AC7 (docs): README + CLAUDE.md секции updated, docs/bluetooth-transport.md created.
- [ ] Run full test suite: `cargo test --workspace -- --test-threads=1`
- [ ] Run clippy: `cargo clippy --workspace -- -D warnings`
- [ ] Verify test coverage не упало (manual reveal via running tests for transport crate).

### Task 16: [Final] Move plan to completed

- [ ] Update CLAUDE.md / docs где упоминается «Plan C — open» — пометить SHIPPED.
- [ ] Update memory `project_bluetooth_transport.md` → SHIPPED status с PR-pointer'ом.
- [ ] Update memory `project_channel_upgrade.md` — Plan C SHIPPED, Plan A pending hardware.
- [ ] Move `docs/plans/20260506-bluetooth-le-transport.md` → `docs/plans/completed/20260506-bluetooth-le-transport.md`. Создать дир если нужно: `mkdir -p docs/plans/completed`.

## Post-Completion

*Items requiring manual intervention or external systems — no checkboxes, informational only*

**Manual verification (mandatory before merge):**

1. **One-time pair Mac↔Win11** (System Settings → Bluetooth, как в AC0 verification). Pair-keys live in OS keychain, не trogenet by app.
2. **Live AC1–AC7 на real Win11+Mac** с активным Continent-АП:
   - Pair flow — clean reproduce.
   - Connect/reconnect cycles — no leak'ов tokio-task'ов, no zombie BLE-handles.
   - Throughput bench: `wd --exec --compress "Get-EventLog System -Newest 5000"` через BT vs Serial — measure relative.
   - Sleep-wake reconnect: Mac закрыть lid на 60s, открыть, проверить что transport recover'ился без user-action'а.
   - Settings flip serial→bluetooth→serial с restart'ом — все режимы работают.
   - Fallback live-test: указать invalid `service_uuid`, `transport_fallback = "serial"` — должен fall back, log warn.
3. **Continent re-verify** — после внесения custom service в код, прогнать LightBlue scan на Mac → подтвердить что Win11 виден с правильным advertised service UUID. Если на этом этапе wfp фильтр режет custom UUID — неожиданное blocking, требует investigation.
4. **Performance regression check на serial.** Запустить full WireDesk session с CH340 после deploy'а — ничего не потеряли в response-time, latency, throughput.

**Future follow-ups (not in this plan, separately scheduled if needed):**

- **Combined dual-channel mode** (BT + serial одновременно для агрегированной скорости) — requires sequence-number'ов, channel-multiplexer'а, retransmit'ов. ~1–2 нед extra. Bullet в memory follow-up.
- **BT Classic RFCOMM upgrade** — если BLE throughput недостаточен (<30 KB/s sustained). Native FFI на Mac (IOBluetooth) + Win (windows-rs Rfcomm). Riskier из-за deprecated IOBluetooth.
- **In-app pairing UI** — replacement для OS pair-dialog. Ergonomics nice-to-have, не критично.
- **Multi-host BLE discovery** — Mac выбирает между несколькими WireDesk hosts. Single-host scope для MVP.
- **EWMA throughput counter в status-bar/tray** — отображение текущей скорости BT-канала (KB/s). Стояло в первоначальном Task 14, но не покрывается AC брифа и не критично для MVP. Defer до момента когда понадобится визуальный bench-tool.
- **Indicate (с ack) вместо Notify для Win→Mac** — если Notify-drop'ы под нагрузкой ломают AC3/AC4 (пока полагаемся на heartbeat-driven recovery). Migration тривиальна — поменять CharacteristicProperties на Indicate, остальное btleplug автоматом подхватит.
- **Drop `transport_fallback`** — runtime fallback на serial при BT failure добавляет complexity (логирование, два code-path'а в factory, два testing scenarios). Альтернатива: при BT init failure print error, exit, user правит конфиг (соответствует Save+Restart pattern проекта). Если AC6 окажется источником багов — упростить до error-exit. В этом плане оставляем, т.к. в брифе AC6 фиксирован.

**Связанные active briefs:**

- `docs/briefs/ft232h-upgrade.md` (Plan A) — parallel option, не вытесняется.
- `docs/briefs/mac-auto-reconnect.md` — process-level reconnect, ortho к BT auto-reconnect (transport-level).
- `docs/briefs/wd-exec-followup-quickwins.md` — independent.
