# WireDesk MVP — Plan

## Цель

Утилита для управления мышью, клавиатурой и clipboard на Windows-машине (Host, с ПО "Континент") с macOS (Client) через serial-соединение. Видео — отдельно через HDMI capture card + QuickTime/VLC.

## Выбранный подход

**WireDesk Lite: Serial-утилита для ввода и clipboard**

Видео не входит в scope — пользователь смотрит экран Host через HDMI capture card в QuickTime/VLC. WireDesk занимается только:
- Перехват мыши/клавиатуры на Mac → передача на Host через serial
- Clipboard sync в обе стороны
- Toggle подключения по горячей клавише

```
[Host Windows 11]                       [Client macOS]
    |                                        |
    |--- HDMI out --→ [Capture] --→ QuickTime/VLC (видео, вне WireDesk)
    |                                        |
    |--- USB-Serial ←-- null-modem --→ USB-Serial
    |                                        |
    Host Agent (console):               WireDesk Client (egui):
    - SendInput() мышь/клав             - [Подключено] [Отключиться]
    - clipboard → serial               - перехват ввода при активации
    - разрешение экрана                 - clipboard preview
```

## Hardware

| Компонент | Цена | Назначение |
|-----------|------|-----------|
| USB HDMI capture card | $10-15 | Видео (вне WireDesk) |
| HDMI сплиттер 1→2 | $5-10 | Монитор Host + capture |
| 2x USB-to-Serial (CH340/FTDI) | $3-5 за штуку | Serial канал WireDesk |
| Null-modem перемычка | $0-3 | TX↔RX, GND↔GND |

## Bandwidth budget (921600 baud = ~90 KB/s)

| Канал | Трафик | Запас |
|-------|--------|-------|
| Мышь (60 evt/s * 12 B) | ~720 B/s | 125x |
| Клавиатура (20 evt/s * 10 B) | ~200 B/s | 450x |
| Heartbeat (0.5/s * 8 B) | ~4 B/s | — |
| Clipboard (chunked, до 64 KB) | burst ~0.7 s | OK |

---

## Протокол (wiredesk-protocol)

### Формат пакета

```
[MAGIC: 0x57 0x44]  2 bytes   "WD"
[TYPE:  u8]         1 byte    тип сообщения
[FLAGS: u8]         1 byte    ACK_REQUIRED, CHUNKED
[SEQ:   u16 LE]     2 bytes   sequence number
[LEN:   u16 LE]     2 bytes   длина payload
[PAYLOAD: N bytes]  0-512     данные
[CRC16: u16 LE]     2 bytes   CRC-16/CCITT
```

Framing: COBS, delimiter 0x00.

### Типы сообщений

```
// Handshake
0x01 HELLO           → {version: u8, client_name: [u8; 32]}
0x02 HELLO_ACK       → {version: u8, host_name: [u8; 32], screen_w: u16, screen_h: u16}

// Input (fire-and-forget)
0x10 MOUSE_MOVE      → {x: u16, y: u16}           // 0..65535 normalized
0x11 MOUSE_BUTTON    → {button: u8, pressed: u8}
0x12 MOUSE_SCROLL    → {delta_x: i16, delta_y: i16}
0x13 KEY_DOWN        → {scancode: u16, modifiers: u8}
0x14 KEY_UP          → {scancode: u16, modifiers: u8}

// Clipboard (с ACK)
0x20 CLIP_OFFER      → {format: u8, total_len: u32}
0x21 CLIP_CHUNK      → {index: u16, data: [u8; <=512]}
0x22 CLIP_ACK        → {index: u16}

// System
0x30 HEARTBEAT       → {}
0x31 ERROR           → {code: u16, msg: [u8; <=256]}
0x32 DISCONNECT      → {}
```

---

## Этапы

### Этап 1: Протокол + транспорт (1-2 дня)

**Задачи:**

1. **Cargo workspace** — `Cargo.toml`
   - members: `crates/wiredesk-core`, `crates/wiredesk-protocol`, `crates/wiredesk-transport`, `apps/wiredesk-host`, `apps/wiredesk-client`

2. **wiredesk-core** — `crates/wiredesk-core/src/`
   - `lib.rs`, `error.rs` (thiserror), `types.rs` (Resolution, Modifiers)

3. **wiredesk-protocol** — `crates/wiredesk-protocol/src/`
   - `packet.rs`: Packet struct, serialize/deserialize
   - `message.rs`: MessageType enum, payload structs
   - `cobs.rs`: COBS encode/decode
   - `crc.rs`: CRC-16/CCITT

4. **wiredesk-transport** — `crates/wiredesk-transport/src/`
   - `transport.rs`: trait Transport
   - `serial.rs`: SerialTransport (serialport crate, COBS framing)
   - `mock.rs`: MockTransport (mpsc channel)

**Тесты:**
- Round-trip сериализация каждого типа сообщения
- COBS round-trip с 0x00 в payload
- CRC16 тестовые вектора
- MockTransport: send → recv
- Bad CRC → ошибка
- Truncated packet → ошибка

**Критерии готовности:**
- `cargo test --workspace` проходит
- `cargo clippy --workspace -- -D warnings` чисто

---

### Этап 2: Host Agent + Client App (2-3 дня)

**Задачи:**

1. **wiredesk-host** — `apps/wiredesk-host/src/` (Windows console app)
   - `main.rs`: clap CLI, serial event loop
   - `injector.rs`: trait InputInjector + WindowsInjector
     - mouse_move(x, y) → SendInput MOUSEEVENTF_ABSOLUTE
     - mouse_button(btn, pressed) → SendInput
     - mouse_scroll(dx, dy) → SendInput MOUSEEVENTF_WHEEL
     - key_down/up(scancode) → SendInput KEYEVENTF_SCANCODE
   - `clipboard.rs`: poll Windows clipboard 500ms, отправка CLIP_OFFER при изменении
   - `session.rs`: handshake, heartbeat, disconnect/reconnect

2. **wiredesk-client** — `apps/wiredesk-client/src/` (macOS egui app)
   - `main.rs`: clap CLI, запуск eframe
   - `app.rs`: WireDeskApp (eframe::App) — маленькое окно:
     - Кнопка "Подключиться" / "Отключиться"
     - Статус: Connected/Disconnected, serial port name
     - Clipboard preview (последний скопированный текст с Host)
     - Кнопка "Ctrl+Alt+Del" (отправить спецкомбинацию)
   - `input/keymap.rs`: таблица macOS Key → Windows scancode
     - A-Z, 0-9, F1-F12, стрелки, Enter, Esc, Tab, Backspace, Delete
     - Модификаторы: Cmd→Ctrl, Option→Alt, Shift→Shift
     - Кириллица через scancodes (работает с любой раскладкой Host)
   - `input/mapper.rs`: InputMapper
     - Координаты мыши: нормализация с учётом aspect ratio Host (из HELLO_ACK)
     - Mouse debounce (макс 60 events/sec)
   - `input/grabber.rs`: перехват глобального ввода при активации
     - При нажатии "Подключиться" или Ctrl+Alt+G: захват мыши/клавиатуры
     - Все события мыши/клавиатуры → serial → Host
     - Повторный Ctrl+Alt+G: отпустить захват, вернуть управление Mac
   - `clipboard.rs`: мониторинг Mac clipboard + приём от Host

**Тесты:**
- Keymap: A-Z, модификаторы, Cmd+C→Ctrl+C, кириллица
- InputMapper + MockTransport: нажатие → корректный пакет
- Координаты мыши: нормализация при разных aspect ratio
- Session: HELLO → HELLO_ACK → Connected
- Disconnect: 3 пропущенных heartbeat → Disconnected
- Reconnect: новый HELLO → восстановление

**Критерии готовности:**
- Client: маленькое окно с кнопкой, статус, clipboard
- Host: console app принимает команды, двигает мышь, печатает
- Ctrl+Alt+G: toggle захвата ввода
- Clipboard sync работает в обе стороны
- Отключение serial → статус Disconnected → автоматический reconnect

---

## Структура проекта

```
wiredesk/
├── Cargo.toml
├── wiredesk.toml.example
├── README.md
├── docs/plans/wiredesk-mvp.md
├── crates/
│   ├── wiredesk-core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── error.rs
│   │       └── types.rs
│   ├── wiredesk-protocol/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── packet.rs
│   │       ├── message.rs
│   │       ├── cobs.rs
│   │       └── crc.rs
│   └── wiredesk-transport/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           ├── transport.rs
│           ├── serial.rs
│           └── mock.rs
├── apps/
│   ├── wiredesk-host/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs
│   │       ├── injector.rs
│   │       ├── clipboard.rs
│   │       └── session.rs
│   └── wiredesk-client/
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── app.rs
│           ├── clipboard.rs
│           └── input/
│               ├── mod.rs
│               ├── keymap.rs
│               ├── mapper.rs
│               └── grabber.rs
└── tests/
    └── integration.rs
```

## Зависимости (Cargo.toml)

| Crate | Назначение |
|-------|-----------|
| `serialport` 4.x | Serial I/O |
| `eframe` / `egui` 0.31 | UI клиента (маленькое окно) |
| `windows` 0.58 | Win32 API (SendInput, clipboard) |
| `clap` 4.x | CLI |
| `serde` + `toml` | Config |
| `thiserror` | Errors |
| `log` + `env_logger` | Logging |
| `crc` | CRC-16 |

Убрано: `nokhwa`, `image`, `ffmpeg` — видео вне scope.

## Риски и mitigation

| Риск | Mitigation |
|------|-----------|
| Континент блокирует USB-Serial | Проверить до разработки; fallback: встроенный COM-порт |
| Глобальный перехват ввода на macOS | CGEventTap требует Accessibility permission; документировать |
| Кириллица | Scancodes вместо VK, работает с любой раскладкой |
| Латентность | 921600 baud, events < 20 bytes = < 1ms |

## Отложено

- Видео в окне WireDesk (сейчас — QuickTime/VLC)
- File transfer
- Multi-monitor
- Audio
- Encryption
- Windows client

## Оценка

- Этап 1: 1-2 дня
- Этап 2: 2-3 дня
- **Итого: 3-5 рабочих дней**
