# Бриф: Bluetooth LE как альтернативный transport-канал (Plan C)

**Status:** ready for `/planning:make` после прохождения **AC0 (Continent BT verification)**. Branch предложен: `feat/bluetooth-transport`.

**TL;DR:** добавить `BluetoothLeTransport` параллельно с `SerialTransport` через существующий trait `Transport`. Mac = BLE Central (btleplug), Win11 = BLE Peripheral (windows-rs WinRT GATT). Throughput target ≥30 KB/s sustained — ×3+ vs текущих 11 KB/s CH340. **Не замена FT232H Plan A**, а альтернатива «без покупки железа, доступна сегодня».

## Контекст

Решение запрошено пользователем 2026-05-06: «Континент не блокирует BT, оба компьютера его поддерживают, файлы быстрее залетят». Действительно — WFP-фильтры Континента работают на IP/TCP/UDP стеке, BT-радио идёт через отдельный device-driver path и логически не попадает в WFP. Существующее ограничение «non-network only» (`CLAUDE.md`) не противоречит BT, потому что BT classic profiles SPP/RFCOMM/L2CAP и BT LE GATT **не создают сетевых интерфейсов**. (BT PAN — создаёт; режется как любая сеть.)

Параллельные опции channel-upgrade'а:
- **Plan A:** FT232H @ 3 Mbaud — ~300 KB/s, ждёт покупки железа (`docs/briefs/ft232h-upgrade.md`).
- **Plan B:** Pi-gadget WinUSB — ~1+ MB/s, ~2–3 нед work, резерв.
- **Plan C (этот бриф):** BT LE — ~30–100 KB/s, ~5–7 дней work, **без железа**.

Plan C **не вытесняет** A или B — это **третья опция** в `config.toml`. Пользователь выбирает по обстоятельствам.

## Цель

Опциональный BT LE transport, выбираемый через `transport = "bluetooth"` в `config.toml`. Throughput ≥30 KB/s sustained, auto-reconnect ≤5s, latency не деградирует субъективно.

## Выбранный подход — BT LE через btleplug + windows-rs WinRT GATT

```
                                       ┌───────────────────────────┐
WireDesk Mac (Central, btleplug)       │  WireDesk Win11 host      │
                                       │  (Peripheral, WinRT GATT) │
       │                               │                           │
       ├── scan by service UUID ───────────────────────────────────►│
       │                                                            │
       │◄─ Notify char (Win→Mac, packets+heartbeat) ────────────────┤
       │                                                            │
       ├── WriteWithoutResponse char (Mac→Win, input+clipboard) ────►│
       │                                                            │
       └────────── BT 5.0 2M PHY, ATT MTU 247 ──────────────────────┘
```

**Почему BT LE, а не Classic SPP/RFCOMM:**
- **macOS прогрессивно убирает Classic** (`/dev/cu.Bluetooth-*` ненадёжно с macOS 10.15+, deprecated IOBluetooth). BLE — текущий поддерживаемый Apple путь.
- btleplug — зрелый Rust crate, работает на Apple Silicon, активно maintained.
- WinRT GATT Server на Win11 хорошо документирован, реализуется через `windows-rs` (~300–500 строк wrapper).
- Throughput BT 5.0 2M PHY с ATT MTU=247 + WriteWithoutResponse: реальные 50–100 KB/s. ×5–9 vs текущие 11 KB/s.

**Почему не Classic RFCOMM (более быстрый ~150 KB/s):**
- IOBluetooth на Mac deprecated, риск сломаться в следующей macOS.
- ×2 кода (FFI на двух разных native API).
- ROI BLE достаточен; апгрейд до RFCOMM — отдельный follow-up если не хватит.

## Требования

**Функциональные:**
1. `BluetoothLeTransport` в `crates/wiredesk-transport/`, реализует `Transport` trait (existing — не меняется).
2. Mac side (Central): `btleplug` 0.11+ — scan по service-UUID, connect, subscribe Notify, write WriteWithoutResponse.
3. Win11 side (Peripheral): `windows-rs` обёртка над `Windows.Devices.Bluetooth.GenericAttributeProfile.GattServiceProvider` + custom characteristics с notify/write.
4. `config.toml` новые поля:
   ```toml
   transport = "bluetooth"          # "serial" (default) | "bluetooth"
   bt_service_uuid = "..."          # generated UUID, fixed на проект
   bt_peer_name = "WireDeskHost"    # advertised name, для UX-clarity
   ```
   CLI: `--transport bluetooth`.
5. UX: Settings panel — combo выбора transport, status-line «BT: scanning / paired / connected / N KB/s».

**Нефункциональные:**
- Throughput ≥30 KB/s sustained на 1 MB clipboard PNG.
- Latency input-events p95 ≤20ms.
- Auto-reconnect ≤5s после кратковременного disconnect / sleep-wake.
- Coexistence: один процесс открывает один transport (mutex). Smooth fallback на serial если BT init failed (config-flag `transport_fallback = "serial"`).

## Acceptance criteria

1. **AC0 — Continent BT verification (gate, ничего не движется без него):**
   - На Win11 host'е c активным Continent-АП: pair Mac↔Win через стандартный OS BT pair-flow.
   - Любым готовым tool'ом (Win11 «Send a file via Bluetooth» в Mac, или vice versa) — успешно передать тестовый файл 1 KB.
   - Если Continent блокирует pairing/connection — Plan C **закрывается**, переключаемся на Plan A/B.
   - **Если работает — премиса подтверждена, идём дальше.**

2. **AC1 — base connect flow:** обе стороны при `transport = "bluetooth"` paired+connected ≤10s после старта. Tray host показывает «Bluetooth: connected». Mac status-bar — green dot.

3. **AC2 — input parity:** мышь, клавиатура (вкл. кириллица + sidebuttons), modifiers — работают идентично serial (visual + spot-check `wd --exec "echo test"`).

4. **AC3 — clipboard throughput:** PNG 1 MB Mac→Host передаётся ≤30s (текущие ~90s через CH340 — ×3+ ускорение). Замерено через secondhand timing вокруг clipboard-event'а.

5. **AC4 — reconnect:** Mac sleep на 60s → wake → auto-reconnect ≤5s без user-action. (Tied: возможно требует базовый reconnect-loop из `mac-auto-reconnect.md` — bundling sensible.)

6. **AC5 — regression на serial:** `transport = "serial"` (default) → behaviour bit-identical pre-PR. Никаких latency-bumps, никаких change'ей в protocol-handshake.

7. **AC6 — fallback:** если BT init fails (radio off / paired-device not found), при `transport_fallback = "serial"` процесс retries serial. Без crash'а.

8. **AC7 — docs:** README + CLAUDE.md секция «Transport options»: pair flow (один раз), config switching, troubleshoot («не connect'ится» → re-pair / radio reset / check Continent BT-policy).

## Тестирование

- **Unit:** `MockBleAdapter` в `crates/wiredesk-transport/tests/` — in-memory pipe с эмуляцией ATT MTU 247, packet fragmentation/reassembly, notify-event semantics. ~10–15 тестов на `BluetoothLeTransport::send/recv` purity.
- **Integration:** `cargo test -p wiredesk-transport --features bt-mock` — full round-trip handshake + 1MB blob через mock.
- **Live (manual, в release-checklist):**
  - Pair test, connect test, latency spot-check.
  - Throughput bench: `wd --exec --compress "Get-EventLog System -Newest 5000"` — сравнить total time через serial vs bluetooth.
  - Reconnect smoke: tray-quit + relaunch, sleep-wake.

## Что НЕ входит в scope

- **BT Classic RFCOMM/SPP** — отдельный follow-up если BLE throughput окажется недостаточным. Брифа сейчас нет.
- **In-app BT pair UI** — используем OS pair-dialog (standard UX, без custom-кода).
- **Multi-host BT discovery** — single-host setup в спеке. Multi-host (выбор между несколькими WireDesk host'ами на одном Mac) — отдельный follow-up.
- **Замена FT232H Plan A** — BT и FT232H сосуществуют как опции config.
- **BT security custom config** — default LE Secure Connections достаточно (оба host'а под user'овским контролем, BT-сниффер на расстоянии метров — out of threat model).
- **Linux host support** — host у нас Win11-only (Continent specifics).

## Риски

| Риск | Severity | Митигация |
|---|---|---|
| **AC0 не пройдёт** (Continent режет BT через DLP/managed-policy) | **CRITICAL** | Live-test первым делом, до любого кода. Если режет — план закрыт, бриф архивируется. |
| BLE throughput < 30 KB/s | medium | Включить 2M PHY + WriteWithoutResponse + ATT MTU 247. Если всё равно low — апгрейд до RFCOMM (отдельный фолоу). |
| WinRT GATT Peripheral в `windows-rs` малохожено | medium | Прототип Win-side первым (1–2 дня), proof-of-concept до полного scope'а. |
| BLE auto-reconnect нестабилен после sleep-wake | medium | Tied-fix с `mac-auto-reconnect.md` — общий reconnect-loop. |
| Pair-flow UX непривычен | low | Чеклист в README, first-launch hint в Settings panel. |
| BT-радио в host'е выключено / занято другим устройством | low | Status-bar диагностика + fallback на serial если configured. |

## Сложность

**medium** (~5–7 дней разработки + 1–2 дня live-tuning'а).

Распределение:
- AC0 verification: 1 час (только pair-test, без кода).
- WinRT GATT Peripheral wrapper: 1.5–2 дня.
- btleplug Mac Central + handshake: 1 день.
- Packet fragmentation/reassembly до ATT MTU: 0.5 дня.
- Settings panel UI: 0.5 дня.
- Auto-reconnect logic: 0.5–1 день (или bundled с `mac-auto-reconnect`).
- Tests + docs: 1 день.
- Live-tuning, throughput optimisation: 1–2 дня.

## Связанное

- `docs/briefs/ft232h-upgrade.md` — Plan A, parallel option, **не вытесняется**.
- `project_continent_wfp.md` (memory) — Continent блочит ТОЛЬКО network interfaces. BT non-network paths логически free.
- `project_channel_upgrade.md` (memory) — будет дополнено Plan C reference после AC0.
- `docs/briefs/mac-auto-reconnect.md` — reconnect-loop tied-fix; разумно bundle PR если оба идут вместе.
- `crates/wiredesk-transport/src/transport.rs` — trait `Transport` (без изменений, только новый impl).
- `apps/wiredesk-host/src/main.rs`, `apps/wiredesk-client/src/main.rs` — точечные изменения в transport-factory.

## Первые шаги

1. **AC0 first.** На Win11 host (с Continent-АП в типовом режиме) — pair Mac↔Win через OS BT settings, передать любой 1 KB файл. Если работает — green light.
2. **WinRT GATT Peripheral PoC.** Минимальный rust-bin: advertise service-UUID + один dummy characteristic с notify, проверить что Mac (через любой BLE-сканер app — например LightBlue) видит и connect'ится.
3. **btleplug Central PoC.** Минимальный rust-bin на Mac — scan, connect, subscribe notify, write to characteristic. Round-trip echo через PoC-pair.
4. **Throughput baseline.** На PoC-pair'е пушнуть 1 MB blob, измерить wall-time. Если ≥30 KB/s — план виален. Если <15 KB/s — пересмотреть подход.
5. **Integrate into BluetoothLeTransport impl trait.** Wire через factory в host/client main.rs.
6. **Tests + Settings UI + docs.**
7. **Live AC1–AC7.**
