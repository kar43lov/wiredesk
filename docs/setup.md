# WireDesk — Setup Guide

Пошаговая инструкция по физическому подключению двух машин (Windows Host + macOS Client) и первому запуску WireDesk.

Контекст: на Host (Windows 11) стоит "Континент" / АПКШ, который при включении блокирует все сетевые интерфейсы. Поэтому всё, что требует сети (установка Rust, `git clone`, `cargo build` за зависимостями) — делается при выключенном Континенте. Сам `wiredesk-host.exe` после сборки сеть не использует и работает при включённом Континенте.

---

## Шаг 0. Соединение проводов (null-modem)

Два варианта по железу:
- **Вариант A — CH340 USB-to-TTL кабели** (~$3-5 каждый, baud до 115200 стабильно).
- **Вариант B — FT232H breakout** (~$15-25 каждый, baud до 3 Mbaud стабильно, ×26 скорости — рекомендуется).

### Вариант A — CH340 USB-to-TTL

Стандартная распиновка — четыре провода:

```
🔴 Красный  = VCC (+5V)   ← НЕ ТРОГАТЬ, изолировать
🔵 Синий    = GND          ← к синему второго кабеля (прямо)
🟢 Зелёный  = TXD (выход)  ← к БЕЛОМУ второго кабеля (крест)
⚪ Белый    = RXD (вход)   ← к ЗЕЛЁНОМУ второго кабеля (крест)
```

Ключевая идея — **зелёный и белый меняются местами** между кабелями: то, что один кабель передаёт (TX, зелёный), другой должен принимать (RX, белый). Это и называется null-modem.

```
Кабель A              Кабель B
🟢 зелёный  ────►  ⚪ белый      (TX → RX)
⚪ белый    ◄────  🟢 зелёный    (RX ← TX)
🔵 синий    ────   🔵 синий      (GND ↔ GND, общая земля)
🔴 красный  ╳   ╳  🔴 красный    (НЕ соединять)
```

### Вариант B — FT232H breakout (CJMCU / Adafruit)

На плате выведен silkscreen `AD0` (TX), `AD1` (RX), `GND`. Распиновка такая же по сути (TX↔RX крест + общая земля), отличаются только обозначения:

```
Плата A              Плата B
AD0 (TX)  ────►  AD1 (RX)      (TX → RX)
AD1 (RX)  ◄────  AD0 (TX)      (RX ← TX)
GND       ────   GND            (общая земля)
+5V / +3.3V       ╳ ╳            (НЕ соединять — каждая плата питается от своего USB)
```

Если провод экранированный (типа UL2547) — экран припаивается к **GND с одного конца** (антенна на оба конца создаёт ground loop). RTS/CTS не разводим — protocol без hardware flow control.

### Общие правила (для обоих вариантов)

1. **VCC/+5V не соединять** — соединение +5V с двух USB-портов может сжечь порт. Изолируй или просто не подключай.
2. **GND обязательно** — без общей земли уровни сигнала плавают, связь не встанет либо будет с CRC errors.
3. **TX↔RX крест-накрест** — то что один передаёт, второй принимает.

Соединять Dupont-джамперами female-female, скруткой с изолентой, либо через макетную плату. На FT232H + 3 Mbaud болтающиеся контакты гарантированы ошибки — паять надёжно, провода короткие (≤30 см).

> ⚠️ Если у CH340-кабелей цвета отличаются от типичной раскладки, проверь мультиметром: между предполагаемым VCC и GND при подключённом USB должно быть ~5V. Если 0V или странное — смотри фото на странице товара или прозванивай. У FT232H breakout всё подписано silkscreen'ом — ошибиться сложно.

---

## Шаг 1. Подключи кабели и определи порты

**На Mac:**

```bash
ls /dev/cu.* | grep -iE 'usb|wch|ch34'
```

Должно появиться что-то вроде `/dev/cu.usbserial-120` (CH340-драйвер от Apple), `/dev/cu.wchusbserial-XXX` (официальный WCH-драйвер) или `/dev/cu.usbserial-1120` (FT232H через встроенный FTDI VCP). Запиши.

> macOS даёт `/dev/cu.usbserial-NNN` номер по physical USB-port location-ID, **не по чипу**. Если переткнёшь адаптер в тот же порт после смены чипа — имя останется тем же. В соседний порт — будет новое имя.

> На macOS каждое serial-устройство имеет два узла: `/dev/tty.*` и `/dev/cu.*`. Для исходящих соединений всегда используй `cu.*` — `tty.*` блокируется в ожидании DCD-сигнала, которого USB-UART кабели обычно не выдают.

**На Windows:**

- Win+X → Диспетчер устройств → "Порты (COM и LPT)"
- Для **CH340**: ищи `USB-SERIAL CH340 (COM3)` или похожее. Жёлтый треугольник = нет драйвера → поставь `CH341SER` с сайта WCH.
- Для **FT232H**: ищи `USB Serial Port (COMx)`. Если запись появилась только в **Universal Serial Bus controllers** как `USB Serial Converter` **без** строки в Ports — поставь **FTDI CDM driver** с https://ftdichip.com/drivers/vcp-drivers/ (раздел Windows → setup executable). После установки перевоткнуть USB, COM-port появится.
  - Edge case на Win11: после установки CDM иногда плата всё ещё сидит без VCP. Лекарство — на `USB Serial Converter` правый клик → Properties → Advanced → ☑ **Load VCP** → OK → перевоткнуть.
- Запиши номер COM.

---

## Шаг 2. Установи Rust на Windows (Континент выключен)

PowerShell от админа:

```powershell
winget install Rustlang.Rustup
```

Перезапусти PowerShell, проверь `cargo --version`. Если `winget` недоступен — скачай `rustup-init.exe` с rust-lang.org.

---

## Шаг 3. Клонируй и собери на Windows (Континент выключен)

```powershell
cd C:\
git clone https://github.com/kar43lov/wiredesk.git
cd wiredesk
cargo build -p wiredesk-host --release
```

Первая сборка 5-10 минут (тянет зависимости с crates.io). После сборки `target\release\wiredesk-host.exe` сеть не требует — работает и при включённом Континенте.

---

## Шаг 4. Собери клиент на Mac

```bash
cd ~/Data/prjcts/wiredesk    # или твой путь к репо
cargo build --release --workspace
```

Чтобы получить полноценный `.app` bundle с иконкой в доке (двойной клик из Finder / Spotlight):

```bash
./scripts/build-mac-app.sh
# → target/release/WireDesk.app
```

Внутри bundle: `Contents/MacOS/wiredesk-client`, `Contents/Info.plist` с `CFBundleIdentifier=dev.kar43lov.wiredesk`, `Contents/Resources/AppIcon.icns`. При первом запуске Gatekeeper заблокирует — правый клик по `.app` → Open → подтвердить.

Source-иконку можно перерисовать через `swift scripts/generate-icon.swift` (Swift+AppKit, ImageMagick не нужен).

---

## Шаг 5. Узнай разрешение Host-экрана

Windows: правый клик по рабочему столу → "Параметры экрана" → "Разрешение дисплея". Например, `1920x1080`.

---

## Шаг 6. Первый запуск — handshake

Можно включать Континент — рабочий режим, в котором WireDesk и нужен.

**Сначала Host (Windows):**

```powershell
cd C:\wiredesk
.\target\release\wiredesk-host.exe --port COM3 --baud 115200 --width 1920 --height 1080
```

> На CH340 — `--baud 115200` (стабильно). На FT232H — можно сразу `--baud 3000000` (~×26 скорости, верифицировано). Главное — **тот же baud на обеих сторонах**.

Release-сборка стартует фоновым tray-приложением — консольного окна не будет. Иконка `W` в трее (серый цвет до handshake).

Логи: `%APPDATA%\WireDesk\host.log.YYYY-MM-DD` (rolling daily). Чтобы быстро открыть папку — правый клик в трее → **Open Logs**.

Чтобы изменить порт/разрешение через UI вместо CLI: правый клик в трее → **Show Settings…** → меняй поля → **Save** → перезапусти процесс через **Quit** + autorelaunch (если включён startup) или повторный запуск из Explorer. Settings пишутся в `%APPDATA%\WireDesk\config.toml`.

Чтобы host автоматически стартовал при логине Windows: в Settings включи **Run on startup** → Save. WireDesk пропишет себя в `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` (без admin-прав, только для текущего юзера).

**Потом Client (Mac):**

```bash
cd ~/Data/prjcts/wiredesk
./target/release/wiredesk-client --port /dev/cu.usbserial-XXX --baud 115200
```

Откроется маленькое окошко WireDesk. На Хосте появится `HelloAck sent`, в окне клиента — статус `Connected`.

> Baud **должен совпадать с host'ом**. Если host запущен `--baud 3000000` — client тоже `--baud 3000000`. Mismatch = no handshake / garbage / instant disconnect.

### Если зависло на handshake

1. Оба видят порты? (`ls /dev/cu.*` на Mac, Диспетчер на Windows)
2. Одинаковый baud с обеих сторон?
3. Поменяй TX/RX (зелёный ↔ белый) на одном из кабелей — самая частая ошибка распиновки
4. Проверь GND — без общей земли handshake не пройдёт
5. Включи debug-логи:
   - Windows: `$env:RUST_LOG="debug"; .\target\release\wiredesk-host.exe ...`
   - Mac: `RUST_LOG=debug ./target/release/wiredesk-client ...`

---

## Шаг 7. Accessibility permission на Mac

Чтобы WireDesk перехватывал системные шорткаты (Cmd+Space, Cmd+C/V) и форвардил их на Windows, нужно разрешение macOS Accessibility. Без него работают только обычные клавиши, а системные комбинации уйдут в чужие приложения (ChatGPT, Spotlight и т.д.).

При первом запуске `wiredesk-client` покажет экран с инструкцией. Делай так:

1. Жми кнопку **Open System Settings** в окне (или вручную: System Settings → Privacy & Security → Accessibility)
2. В Accessibility-списке нажми **+** внизу
3. В Finder-диалоге **Cmd+Shift+G**, вставь полный путь:
   ```
   /Users/USERNAME/path/to/wiredesk/target/release/wiredesk-client
   ```
   (под свой путь). Жми Enter и Open.
4. Включи тумблер справа от `wiredesk-client`
5. **Закрой и перезапусти** `wiredesk-client` — это обязательно. Tap-поток создаётся один раз при старте, после grant'а нужен свежий процесс.

После рестарта окно сразу откроется в обычном UI (без permission-экрана).

> Permission привязана к конкретному пути бинаря. Если перекомпилируешь в другую папку — заново добавить.

## Шаг 8. Проверка ввода

В окне клиента:

1. Нажми `Capture` (или **Cmd+Esc**) — фокус ввода уходит на Host
2. Подвигай мышью — курсор на Windows-экране (через capture-карту в QuickTime/VLC) едет
3. Напечатай в Notepad — набирается, кириллица тоже
4. **Cmd+Space** — переключение языка ввода на Windows (Win+Space)
5. **Cmd+C** в Windows-приложении → текст копируется (Ctrl+C effect), через ~1 сек прилетает в Mac clipboard
6. **Cmd+V** в Mac-приложении (после переключения фокуса) — вставит синхронизированный текст
7. **Cmd+Enter** — fullscreen toggle (полезно для «третьего монитора» через HDMI capture)
8. **Cmd+Esc** — выход из capture, на Mac снова все клавиши работают штатно
9. Кликни в любое другое Mac-приложение — capture автоматически паузится, Mac shortcuts работают

---

## Шаг 9. Терминал в Ghostty/iTerm

Закрой `wiredesk-client` (он держит порт). В Ghostty:

```bash
cd ~/Data/prjcts/wiredesk
./target/release/wiredesk-term --port /dev/cu.usbserial-XXX --baud 115200
```

(baud — такой же как у host'а, см. шаг 6).

Появится приглашение PowerShell. **Ctrl+]** — выход с восстановлением локального терминала.

Interactive `wd` теперь использует настоящий PTY на host'е (ConPTY) — vim/htop/nano, ssh без `-tt`, PSReadLine с history через стрелки и Tab autocomplete работают как в нативном терминале. Окно Ghostty можно ресайзить — vim/htop reflow'ят корректно. Ограничение: для `wd --exec` (non-interactive single-shot mode) специально используется pipe-канал — sudo с паролем и интерактивные prompt'ы там работать не будут (по дизайну, для clean stdout sentinel detection).

---

## Порядок дня

1. **Континент выключен** → `git pull`, пересборка при необходимости
2. **Континент включён** → запускаешь `wiredesk-host` на Windows и `wiredesk-client` (или `wiredesk-term`) на Mac, работаешь

После первой сборки сеть на Windows больше не нужна.
