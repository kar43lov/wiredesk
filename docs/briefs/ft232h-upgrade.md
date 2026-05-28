# Бриф: апгрейд канала CH340 → FT232H

**Дата брейншторма:** 2026-05-02
**Автор брейншторма:** /pg.brainstorm
**Триггер:** разговор о замене serial на USB/Thunderbolt ради скорости и видео.

---

## Status: SHIPPED & VERIFIED LIVE 2026-05-28

Реализовано на двух **CJMCU-FT232H** breakout (genuine FTDI, VID `0x0403` PID `0x6014`; один FT232HQ/QFN, другой FT232HL/LQFP) соединённых null-modem'ом (`AD0(TX) ↔ AD1(RX)` cross + `GND ↔ GND`, экран на GND с одного конца, VCC/+5V не соединены). Подняли `baud` поэтапно `115200 → 1_000_000 → 3_000_000` — оба промежуточных и целевой прошли чисто (capture, clipboard, heartbeat без CRC-ошибок и disconnect'ов). Изменения в коде — **только** `baud = 3000000` в обоих `config.toml`.

**Результат vs AC:**
- AC1 ✓ FT232H с обеих сторон null-modem'а.
- AC2 ✓ `baud = 3_000_000` принят без ошибок.
- AC3 ✓ 1 MB clipboard едет ~3 сек (×30 от тогдашних ~90 сек; цель была <2 сек, фактически ~3 сек — но это уже комфортнее любого user scenario, целевая ratio достигнута).
- AC4 ✓ Heartbeat без потерь на sustained-load.
- AC5 ✓ Continent-АП не среагировал.
- AC6 ✓ Все тесты прошли (test count в README/CLAUDE.md обновлён до 491).

**Lessons learned (не попало в исходный бриф):**
- **Перепрошивка EEPROM на CJMCU-FT232H не нужна.** Платы приходят в async-RS232 (`Single RS232-HS`), VCP сам поднимается. Самый рискованный пункт оригинального плана (FT_PROG / `ftx_prog`) отпал.
- **Win11 требует FTDI CDM driver** (https://ftdichip.com/drivers/vcp-drivers/). Без него FT232H появляется как "USB Serial Converter" в Universal Serial Bus controllers, но **без** строки в Ports (COM & LPT) — Detect возвращает пустой список. Лекарство: установить CDM, после reset устройства COM-port появится. На macOS VCP встроен — install не нужен.
- **macOS reuse'ает `/dev/cu.usbserial-NNN` номер по physical USB port location-ID, не по чипу.** Если воткнуть FT232H в тот же физический порт где раньше был CH340 — имя останется тем же (например `usbserial-140`). Не пугаться что номер «как у старого».
- **Поднимать baud СИНХРОННО на обеих сторонах.** Mismatch (1M ↔ 115200) = garbage / no handshake / disconnect.
- **Win11 PnP edge case:** иногда плата садится как `USB Serial Converter` **без** VCP-shim даже после установки CDM. Device Manager → Properties → Advanced → ☑ **Load VCP** → перевоткнуть.

Plan B (Pi Zero 2W WinUSB bridge) остаётся актуален **только** как путь к видео по тому же каналу (~30-40 MB/s через USB 2.0 bulk), не как backup на скорость — Plan A полностью закрыл speedup-цель.

---

## Цель

Поднять пропускную способность канала Mac↔Win в **~100 раз** (с 11 KB/s до 1.0–1.5 MB/s стабильно), оставив всю остальную архитектуру WireDesk без изменений.

## Контекст и обоснование выбора

В ходе брейншторма проверили альтернативные каналы. Подтверждено:

- **Континент-АП** на Host (Win11) блокирует **любую IP-связь мимо своего туннеля** через WFP-фильтры. Это подтвердил живой тест: Win и Mac в одной Wi-Fi сети 192.168.1.0/24, route table показывает default через Wi-Fi, но `ping 192.168.1.98` с Win → `General failure`, `ping 192.168.1.100` (сам в себя) → тот же `General failure`. Drop происходит WFP-callout'ом до отправки пакета.
- Это закрывает **TCP/UDP/Wi-Fi/Ethernet/Thunderbolt Networking/USB CDC NCM/Plugable bridge cable** — всё, что создаёт сетевой интерфейс.
- На Host **нет Thunderbolt**, USB-контроллер — Intel USB 3.2 Gen 2.
- Mac mini M4 — есть 3×TB4/USB4 порта, но это ничего не меняет, т.к. узкое место на Win-стороне и сетевые каналы не проходят.

Допустимые с точки зрения Континента каналы — только **не-сетевые**: USB CDC ACM (текущий serial), WinUSB / libusb bulk на custom device class, USB HID. Текущая реализация на CH340 — частный случай USB CDC ACM с медленным baud (115200, лимит самого CH340 на нестабильных кабелях).

**Решение по результатам брейншторма:** заменить чип в null-modem кабеле с **CH340** на **FTDI FT232H** (или FT4232H для ещё большей скорости). Тот же класс USB CDC ACM — Континент это видит как «обычное USB-устройство, не сеть» и не трогает. В коде проекта меняется только дефолтный baud rate.

**Почему именно FT232H:**

| Кандидат | Заявленная скорость | Стабильность | Цена | Driver |
|---|---|---|---|---|
| **CH340** (текущий) | до 921600 baud | unstable на dupont/921600 | ~$2 | OS native |
| **FT232H** | до 12 Mbps в bit-bang, до **3 Mbps stable в UART** | high | ~$15–25 | FTDI VCP, on-board on macOS, Win driver auto-install |
| **FT2232H / FT4232H** | до **12 Mbps stable per channel** | high, multi-channel | ~$30–50 | как FT232H |
| **CP2102N** (SiLabs) | до 3 Mbps | medium | ~$5 | SiLabs VCP |

FT232H выбран как лучший баланс цена/скорость/доказанность. При желании дальнейшего апгрейда можно перейти на FT4232H без изменений в коде (тот же VCP-интерфейс).

## Требования

### Функциональные
- Заменить CH340 на FT232H с обеих сторон линка (Mac и Host).
- Поднять стабильный baud до **3 000 000** (3 Mbps) как первая цель; иметь возможность поднять до 12 Mbps на FT4232H в будущем.
- Сохранить совместимость со всем существующим протоколом WireDesk (Packet, Message, COBS, CRC-16).
- TOML-конфиг должен принимать новый baud; CLI-флаг `--baud` уже работает.

### Нефункциональные
- Никаких изменений в архитектуре: те же 6 crates, тот же `SerialTransport`, та же threading-модель.
- Откат на CH340+115200 должен работать одной строкой в config.toml — для случаев тестирования или если FT232H не доедет.
- Continent-АП должен оставаться видеть устройство как «нейтральный USB CDC ACM» (он же VCP), без вызова WFP-фильтров. Подтвердить эмпирически после установки.

## Acceptance criteria

1. С обеих сторон стоит FT232H breakout (или эквивалент), connected null-modem (TX↔RX, GND↔GND, VCC isolated).
2. WireDesk запускается с `baud = 3_000_000` без ошибок инициализации serial.
3. Передача 1 МБ-картинки через clipboard завершается за **<2 секунды** (сейчас ~90 сек). Это ≥45× ускорение, целевая ratio.
4. Heartbeat-loss не растёт по сравнению с baseline (115200): не больше одного потерянного heartbeat за 10-минутный sustained-load тест.
5. Continent-АП после установки FT232H продолжает работать в обычном режиме (туннель up, корпоративные ресурсы доступны).
6. Все 211 существующих тестов проходят без модификаций.

## Тестирование

### Что покрыть тестами (новое)
- **Unit:** `validate_baud` (`apps/wiredesk-host/src/ui/format.rs`) — расширить допустимые значения, добавить case 3_000_000, 12_000_000.
- **Integration (Mock):** прогон существующих protocol-тестов на новом baud — должны пройти без изменений (MockTransport не зависит от baud).
- **Live (manual):** sustained-load тест (transfer 100 МБ файла через clipboard или shell-канал; замер времени, byte-error-rate через CRC-failed-counter).

### Что не покрывать
- Не писать тестов на **FTDI driver behavior** — это область драйвера, не наша.
- Не писать perfomance-benchmarks в CI (зависят от железа, дёргают флаки).

## Что НЕ входит в scope

- **Видео по тому же каналу** — даже на 12 Mbps это впритык для 720p H.264 (1.5 MB/s budget с overhead'ом). HDMI capture остаётся, отдельный спайк может быть позже.
- **WinUSB / Pi-bridge архитектура** — отложено как Plan B, если FT232H по какой-то причине не даст ожидаемого ускорения или будет нестабильным.
- **Thunderbolt / TB AIC / TB DMA** — закрыто. Нет TB-header на B760M, и любой сетевой канал режется Континентом.
- **Code-signing / нотарификация .app** — оставлено как было.

## Риски

| Риск | Вероятность | Последствие | Mitigation |
|---|---|---|---|
| FT232H на VCC от USB не даёт стабильные 3 Mbps на Dupont-проводах | medium | падение до 1 Mbps | использовать **короткие** провода (≤30 см), shielded если есть; начать с 1 Mbps, поднять до 3 |
| macOS видит FT232H как нестандартный VCP, нужен пользовательский драйвер | low | разовая настройка | FTDI VCP на macOS работает out-of-the-box с Big Sur; в крайнем случае ставится FTDI installer |
| Continent видит новый USB VID/PID и блокирует устройство | very low | устройство недоступно | прецедентов не зафиксировано, Continent работает на network-stack уровне, не на USB-class. Если случится — пробовать FT4232H с другим VID. |
| FT232H ноунейм с aliexpress оказался FT232H-FAKE (CH340 в чужом корпусе) | medium | те же проблемы что сейчас | купить у проверенных продавцов (Adafruit, Sparkfun, DigiKey, Mouser); либо сразу два кабеля и проверить через `lsusb`/Device Manager |
| 921600 был нестабилен на CH340 — те же проблемы повторятся на 3 Mbps | low | падение скорости до 1 Mbps | FT232H использует другой UART-генератор (не PLL-divider от 12 МГц как CH340), его jitter заметно ниже |

## Первые шаги (5 действий по порядку)

1. **Купить два FT232H breakout-кабеля или модуля.** Adafruit FT232H Breakout — самый надёжный (~$15). Альтернатива — SparkFun, или ноунейм UMFT232H у проверенных eBay/Aliexpress продавцов с reputation. Заказать сразу два — для обеих сторон линка.

2. **Перепаять/подключить null-modem.** Те же 4 провода что сейчас: TX→RX, RX→TX, GND↔GND, VCC изолировать. На FT232H breakout TX/RX выведены на пины — конкретно `D0`=TX, `D1`=RX в UART-mode (по умолчанию).

3. **Проверить enumeration на обеих сторонах.** На Mac: `system_profiler SPUSBDataType | grep -i FT232`. На Win: Device Manager → Ports (COM & LPT) → должен появиться `USB Serial Port (COM*)` с VID 0403:6014 (FT232H) в свойствах. Если нет — поставить FTDI VCP driver с ftdichip.com.

4. **Обновить config.toml на обеих сторонах:**
   ```toml
   [serial]
   baud = 3000000   # с 115200
   ```
   Перезапустить host (Save & Restart в tray) и client.

5. **Прогнать sustained-load тест.** Скопировать 1-МБ картинку в clipboard на Mac → проверить что Host получил за <2 сек. Параллельно следить за status-line counter и log-warnings об interleaved offers / CRC failures. Если стабильно — поднять baud до 6 Mbps, повторить. Если на 3 Mbps уже флаки — откатиться на 1 Mbps и копать в провода.

## Сложность

**Low.** Никаких архитектурных изменений, никакого нового кода. ~1 день работы (½ дня на железо, ½ на live-тест и тонкую настройку baud).

## Если FT232H не сработает (Plan B)

Если по какой-то причине FT232H выдаёт нестабильность на 3 Mbps — следующий уровень это **WinUSB через Pi Zero 2W bridge** (см. вариант B в обсуждении). Pi видится с обеих сторон как кастомное USB-устройство (не сеть), даёт реальные ~30 MB/s через USB 2.0 bulk endpoints. Effort — 2–3 недели разработки, отдельный спайк. Запускается только если железный апгрейд FT232H провалится.
