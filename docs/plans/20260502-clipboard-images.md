# Clipboard-картинки в WireDesk

## Overview

Двунаправленная синхронизация PNG-картинок в clipboard между macOS-клиентом и Windows-хостом. Reuse существующего chunked-протокола `ClipOffer`/`ClipChunk` через `format=1` (PNG, lossless). Текстовый clipboard (`format=0`) продолжает работать без изменений.

**Проблема.** Сейчас clipboard в WireDesk поддерживает только UTF-8 текст. Скриншоты, скопированные через `Cmd+Shift+Ctrl+4` (Mac) или Snipping Tool (Win), не передаются на peer.

**Польза.** Естественный copy-paste цикл для скриншотов между Mac и Host без USB-флешки. Покрывает 80% повседневных use-cases (скриншот окна → вставить в чат / bug-report / документ).

**Интеграция.** Минимальные изменения протокола (только новое значение existing-поля `format`). Никаких новых message types. Никаких новых UI-блоков — прогресс в существующей status-line. Никакого breaking-change для текста.

**Out of scope:** файлы (file URLs / `CF_HDROP`), JPEG/lossy compression, картинки > 1 MB encoded (отказ с toast), overlay-progress с Cancel.

## Context (from discovery)

- **Файлы/компоненты:**
  - `crates/wiredesk-protocol/src/message.rs` — `ClipOffer { format: u8, total_len }`, `ClipChunk { index, data }`. Поле `format` уже есть, всегда `0` в текущем коде.
  - `apps/wiredesk-client/src/clipboard.rs` — Mac side: `ClipboardState`, `spawn_poll_thread`, `IncomingClipboard`. arboard 3.6.1 уже подключён.
  - `apps/wiredesk-host/src/clipboard.rs` — Win side: `ClipboardSync`. Симметрично Mac.
  - `apps/wiredesk-client/src/app.rs` — `WireDeskApp::render_status_line` (TODO: проверить точное имя при реализации).
  - `apps/wiredesk-client/src/main.rs` — wiring channels между threads.
- **Patterns:**
  - Hash-based loop avoidance (`DefaultHasher` от content).
  - Chunked transfer: `ClipOffer` → N×`ClipChunk` (256 B/chunk), reassembly через `BTreeMap<u16, Vec<u8>>`.
  - Mac: 3 thread'а (writer + reader + clipboard poll) + main UI thread.
  - Host: single tick-loop, `ClipboardSync::poll()` возвращает `Vec<Message>` в очередь отправки.
  - Duplication между Mac и Host clipboard.rs **приемлема** (CLAUDE.md явно).
- **Зависимости:**
  - `arboard 3.6.1` (обе стороны) — `get_image()/set_image()` для `ImageData { width, height, bytes: Cow<[u8]> }` (RGBA).
  - `image 0.25.10` — уже в transitive deps (через arboard). Нужно поднять до direct dep с `default-features=false, features=["png"]`.
  - PNG encoder: `image::codecs::png::PngEncoder::write_image(rgba, w, h, Rgba8)`.
  - PNG decoder: `image::load_from_memory_with_format(bytes, ImageFormat::Png)`.

## Development Approach

- **Testing approach:** Regular (код → тесты в той же task).
- Каждая task — атомарный логический блок. Все тесты в task должны проходить до перехода к следующей.
- Каждый task **обязан** включать unit-тесты для нового/изменённого кода (success + error/edge cases).
- `cargo test --workspace` зелёный после каждого task.
- `cargo clippy --workspace --all-targets -- -D warnings` clean после каждого task.
- Backward-compat: текстовый clipboard (`format=0`) работает 1-в-1 как раньше — добавить regression-тест в Task 1.
- При обнаружении новых задач/блокеров — обновлять этот план (➕/⚠️ префиксы).

## Testing Strategy

- **Unit-тесты** обязательны для каждой task (см. Development Approach).
- **E2E тесты** — у проекта нет UI-based e2e (Playwright/Cypress). Live-проверка делается вручную в финальном Task (см. AC1–AC6).
- **Regression-набор** для текстового clipboard — отдельный тест в Task 1 (roundtrip ClipOffer{format=0}).

## Progress Tracking

- Завершённые items — `[x]` сразу после выполнения.
- Новые задачи — `➕ Task N+1: ...`.
- Блокеры — `⚠️ ...` с описанием.
- При значительном изменении scope — править этот план в той же commit.

## Solution Overview

**Архитектура.**

```
Mac side (poll thread):
  loop every 500ms:
    if get_text() ok and not loop-echo:    [unchanged]
      → send ClipOffer{format=0} + ClipChunks
    else if get_image() ok and not loop-echo:    [NEW]
      rgba = ImageData
      hash = hash(rgba.bytes)
      if hash == last_known: skip
      png = encode_rgba_to_png(rgba)
      if png.len() > MAX_IMAGE_BYTES: toast + skip
      → send ClipOffer{format=1, total_len=png.len()} + ClipChunks
      counter.store(0); total = png.len()
      [counter increments per chunk sent]

Mac side (reader thread, IncomingClipboard):
  on_offer(format, total_len):    [save format, was: drop]
  on_chunk(index, data):
    [accumulate in BTreeMap]
    [update receiving counter for status-line]
    when received_total >= expected:
      match format:
        0 → set_text(...) [unchanged]
        1 → rgba = decode_png(buf); set_image(rgba)    [NEW]

Win side: симметрично.
```

**Ключевые решения.**

1. **Reuse ClipOffer.format** — поле существует, занято только значением 0. Добавляем `1=PNG` константу.
2. **Hash от RGBA, не от PNG bytes** — round-trip arboard PNG↔RGBA нестабилен (различные compression options дают разные encoded байты), RGBA стабилен.
3. **PNG encode в poll thread** — не в writer thread, потому что poll уже работает раз в 500 ms и encode-латенси ~50–150 ms терпима.
4. **Лимит проверяется после encode** — невозможно предсказать размер PNG из RGBA dimensions без encode'а (compression ratio зависит от контента).
5. **Counter `Arc<AtomicU64>`** — для прогресса. Writer thread инкрементирует после каждого `transport.send()`. UI читает в `update()`. Минимальное coupling.
6. **Toast** в Mac — как уже сделанный inline-toast (3 сек) в Settings panel. На Host — `log::info!` (нет интерактивного UI для toast).
7. **`enum LastKind { Text(u64), Image(u64), None }`** в `ClipboardState` — заменяет текущий `last_hash: u64`. Различение нужно чтобы dedup для одного типа не блокировал отправку другого.

**Edge cases (явно).**

- **Interleaved offers (race text+image copy).** Сценарий: текстовый ClipOffer{format=0} в полёте → пользователь делает Cmd+C на картинке → следующий poll-tick шлёт ClipOffer{format=1}. Receiver видит второй offer, пока ещё реассемблирует первый. Правило: **новый ClipOffer сбрасывает любую in-progress reassembly** (drop accumulated chunks + `log::warn!("clipboard: incoming offer aborted previous reassembly")`). Sender — single-threaded poll, проблема только на receiver'е.
- **Peer disconnect во время image-transfer.** Транспорт падает посередине → `outgoing_progress < outgoing_total` залипает (status-line «Sending — 340/780 KB» висит); `incoming_chunks` оседают в памяти receiver'а. Правило: при reconnect (новый Hello) и при `TransportEvent::Disconnected` — **обнулить outgoing/incoming counters + сбросить `IncomingClipboard` state** (expected_len=0, expected_format=0, received.clear()). У отправителя `last_kind` сохраняется (после reconnect не нужно повторно слать тот же контент).
- **Reset method.** В `IncomingClipboard` добавить `pub fn reset(&mut self)`, в `ClipboardSync` (Host) — то же. Вызывается из disconnect-handler.

## Technical Details

**Протокол** (изменений минимум).

```rust
// crates/wiredesk-protocol/src/message.rs
pub const FORMAT_TEXT_UTF8: u8 = 0;
pub const FORMAT_PNG_IMAGE: u8 = 1;
// ClipOffer { format, total_len } — без изменений
// ClipChunk { index, data } — без изменений
```

**Зависимости.**

```toml
# apps/wiredesk-client/Cargo.toml + apps/wiredesk-host/Cargo.toml
image = { version = "0.25", default-features = false, features = ["png"] }
```

**Структуры.**

```rust
// apps/wiredesk-client/src/clipboard.rs (Mac)
enum LastKind {
    Text(u64),    // hash от строки
    Image(u64),   // hash от RGBA bytes
    None,
}

#[derive(Clone, Default)]
pub struct ClipboardState {
    last: Arc<Mutex<LastKind>>,
}

// IncomingClipboard:
pub struct IncomingClipboard {
    state: ClipboardState,
    expected_len: u32,
    expected_format: u8,    // NEW: стораж формата из offer
    received: BTreeMap<u16, Vec<u8>>,
    received_total: u32,
    clip: Option<arboard::Clipboard>,
    progress: Arc<AtomicU64>,    // NEW: для status-line
}

// New helper functions (private to clipboard.rs):
fn encode_rgba_to_png(img: &arboard::ImageData) -> Result<Vec<u8>, image::ImageError> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(&img.bytes, img.width as u32, img.height as u32, image::ExtendedColorType::Rgba8)?;
    Ok(out)
}

fn decode_png_to_rgba(bytes: &[u8]) -> Result<arboard::ImageData<'static>, image::ImageError> {
    let dyn_img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)?;
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Ok(arboard::ImageData {
        width: w as usize,
        height: h as usize,
        bytes: std::borrow::Cow::Owned(rgba.into_raw()),
    })
}
```

**Константы.**

```rust
const MAX_IMAGE_BYTES: usize = 1024 * 1024;  // 1 MB encoded
// MAX_CLIPBOARD_BYTES (256 KB) — остаётся для text

// Для unit-тестов (нельзя протестировать реалистичный 1MB-кейс на synthetic RGBA —
// PNG жмёт 4×4 паттерн в сотни байт). Параметризуем лимит, в проде используем
// MAX_IMAGE_BYTES, в тестах задаём низкий порог.
fn check_image_size(png: &[u8], limit: usize) -> Result<(), TooLarge> { ... }
```

**Wiring (Mac side).**

```
main.rs:
  let outgoing_progress = Arc::new(AtomicU64::new(0));
  let outgoing_total = Arc::new(AtomicU64::new(0));
  let incoming_progress = ...
  let incoming_total = ...
  clipboard::spawn_poll_thread(state, outgoing_tx, outgoing_progress.clone(), outgoing_total.clone());
  // IncomingClipboard через reader_thread получает incoming_progress + incoming_total
  // Передать всё четыре в WireDeskApp::new для render
```

**Status-line формат** (existing render):
```
"Connected | Sending image — 340/780 KB (43%)"     // outgoing > 0 && total > 0
"Connected | Receiving image — 120/780 KB (15%)"   // incoming > 0 && total > 0
"Connected"                                          // both zero
```

После завершения трансфера — counter сбрасывается в 0 (writer/reader сами обнуляют после finish или по таймеру).

## What Goes Where

- **Implementation Steps** — все code/tests/docs в этом репо.
- **Post-Completion** — live-проверка AC1–AC6 на реальном железе (Mac + Win с null-modem).

## Implementation Steps

### Task 1: Расширить protocol — константа FORMAT_PNG_IMAGE и regression-тест

**Files:**
- Modify: `crates/wiredesk-protocol/src/message.rs`

- [x] добавить публичные константы `FORMAT_TEXT_UTF8: u8 = 0` и `FORMAT_PNG_IMAGE: u8 = 1` в `message.rs` (модульный уровень).
- [x] добавить тест `roundtrip_clip_offer_image`: `ClipOffer { format: FORMAT_PNG_IMAGE, total_len: 245760 }` сериализуется и десериализуется идентично.
- [x] добавить regression-тест `roundtrip_clip_offer_text`: `ClipOffer { format: FORMAT_TEXT_UTF8, total_len: 1024 }` (есть `roundtrip_clip_offer` с произвольным `format=1`, переименовать или дополнить — без потери покрытия).
- [x] запустить `cargo test -p wiredesk-protocol` — все тесты зелёные.
- [x] запустить `cargo clippy -p wiredesk-protocol --all-targets -- -D warnings` — clean.

### Task 2: Добавить direct dep `image` в client и host crates

**Files:**
- Modify: `apps/wiredesk-client/Cargo.toml`
- Modify: `apps/wiredesk-host/Cargo.toml`

- [ ] добавить в `apps/wiredesk-client/Cargo.toml` секцию `[dependencies]`: `image = { version = "0.25", default-features = false, features = ["png"] }`.
- [ ] добавить ту же строку в `apps/wiredesk-host/Cargo.toml`.
- [ ] запустить `cargo build --workspace` — собирается без новых ошибок.
- [ ] запустить `cargo tree -p wiredesk-client | grep image` и убедиться что используется уже существующий `image v0.25.x` (никакого duplicate-version bloat).
- [ ] запустить `cargo clippy --workspace --all-targets -- -D warnings` — clean.

### Task 3: Mac side — PNG codec helpers + LastKind enum (без отправки)

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs`

- [ ] добавить private функции `encode_rgba_to_png(&arboard::ImageData) -> Result<Vec<u8>, image::ImageError>` и `decode_png_to_rgba(&[u8]) -> Result<arboard::ImageData<'static>, image::ImageError>` (смотри Technical Details).
- [ ] добавить private функцию `hash_bytes(&[u8]) -> u64` (используется и для PNG-RGBA, и в перспективе для file-list).
- [ ] заменить `ClipboardState.last_hash: Arc<Mutex<u64>>` на `last: Arc<Mutex<LastKind>>` где `enum LastKind { Text(u64), Image(u64), None }`. Старые методы `get/set` адаптировать под matching по типу.
- [ ] обновить существующий код poll thread / commit чтобы использовать новый `LastKind::Text(hash)`.
- [ ] добавить unit-тест `encode_decode_roundtrip`: synthetic 4×4 RGBA → encode PNG → decode → byte-equal RGBA.
- [ ] добавить unit-тест `hash_bytes_stable`: один и тот же RGBA-буфер дважды даёт одинаковый hash.
- [ ] добавить unit-тест `last_kind_dedup_text_does_not_block_image`: после `set(Text(h1))`, попытка отправить image с hash h2 должна пройти (не задевается dedup'ом).
- [ ] запустить `cargo test -p wiredesk-client` — все тесты зелёные.
- [ ] запустить `cargo clippy -p wiredesk-client --all-targets -- -D warnings` — clean.

### Task 4: Mac side — отправка картинки (poll thread)

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs`

- [ ] добавить константу `MAX_IMAGE_BYTES: usize = 1024 * 1024` (1 MB) в `clipboard.rs`.
- [ ] расширить `spawn_poll_thread`: после неудачного `get_text()` (или после успешного, если text empty) пробовать `get_image()`. При успехе — hash от `image.bytes` (RGBA), сравнить с `LastKind::Image(...)`, если новый — encode PNG.
- [ ] если encoded PNG > `MAX_IMAGE_BYTES` — `log::warn!("clipboard: image too large ({} bytes), skipping", png.len())` + skip (toast в UI — отдельный сигнал, реализуется в Task 7).
- [ ] иначе — `outgoing_tx.send(ClipOffer{format: FORMAT_PNG_IMAGE, total_len})` + chunked `ClipChunk`'и (256 B/chunk).
- [ ] обновить `LastKind` в state на `Image(hash)`.
- [ ] добавить параметры `outgoing_progress: Arc<AtomicU64>` и `outgoing_total: Arc<AtomicU64>` в сигнатуру `spawn_poll_thread`. После send'а ClipOffer — `total.store(png.len() as u64)`, `progress.store(0)`. После каждого ClipChunk — `progress.fetch_add(chunk.len(), Relaxed)`. После последнего chunk — оставить значения как есть (UI прочитает 100%, потом сам по таймеру очистит, либо очистим при следующем offer).
- [ ] добавить unit-тест `image_too_large_skipped` через параметризацию: вынести size-check в pure helper `fn check_image_size(png_len: usize, limit: usize) -> Result<(), ImageTooLarge>` и тестировать с маленьким `limit` (например 512 байт), а poll thread в проде передаёт `MAX_IMAGE_BYTES`. Тест проверяет что при превышении лимита логика возвращает skip-сигнал, не вызывая send.
- [ ] добавить unit-тест `image_emit_offer_and_chunks`: synthetic 4×4 RGBA → один ClipOffer + N ClipChunk через mpsc, total_len совпадает с encoded PNG length, sum(chunks) = encoded.
- [ ] запустить `cargo test -p wiredesk-client` — все тесты зелёные.
- [ ] запустить `cargo clippy -p wiredesk-client --all-targets -- -D warnings` — clean.

### Task 5: Mac side — приём картинки (IncomingClipboard) + edge cases

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs`
- Modify: `apps/wiredesk-client/src/main.rs`

- [ ] добавить поле `expected_format: u8` в `IncomingClipboard`. Изменить сигнатуру `on_offer(format: u8, total_len: u32)`.
- [ ] **abort previous reassembly:** в `on_offer` если `self.received_total > 0 && self.received_total < self.expected_len` → `log::warn!("incoming offer aborted previous reassembly ({} bytes accumulated)")` + `self.received.clear() + self.received_total = 0` перед сохранением нового offer'а.
- [ ] добавить публичный метод `pub fn reset(&mut self)` — обнуляет `expected_len`, `expected_format`, `received_total`, `received.clear()`, обнуляет `incoming_progress`/`incoming_total` (`store(0, Relaxed)`).
- [ ] в `commit()` ветвление: `match self.expected_format { FORMAT_TEXT_UTF8 => set_text(...), FORMAT_PNG_IMAGE => decode + set_image(...), _ => log::warn!("unknown format {}, skipping") }`.
- [ ] для image-ветки: `decode_png_to_rgba(&buf)` → если `Ok(image)` → hash от `image.bytes`, `state.set(Image(hash))`, `clip.set_image(image)`. Если `Err` — log::warn + skip.
- [ ] добавить параметры `incoming_progress: Arc<AtomicU64>` и `incoming_total: Arc<AtomicU64>` в `IncomingClipboard::new`. В `on_offer` — `total.store(total_len as u64, Relaxed)`, `progress.store(0, Relaxed)`. В `on_chunk` — `progress.fetch_add(data.len() as u64, Relaxed)`.
- [ ] **wiring в `main.rs::reader_thread`:** заменить текущее `Message::ClipOffer { total_len, .. } => incoming_clip.on_offer(total_len)` на `Message::ClipOffer { format, total_len } => incoming_clip.on_offer(format, total_len)`. Без этого compile-ошибка/regression (сейчас `format` отбрасывается через `..`).
- [ ] **disconnect handling в reader_thread:** при `Message::Disconnect` или ошибке транспорта — `incoming_clip.reset()` перед выходом из цикла (либо отправить событие, чтобы `WireDeskApp` обнулил outgoing-counters; см. Task 7a).
- [ ] обновить `main.rs::main` — создать `Arc<AtomicU64>`'ы для incoming + передать в `IncomingClipboard::new`.
- [ ] unit-тест `incoming_image_reassembly`: synthetic PNG → on_offer + N×on_chunk → перед `clip.set_image()` извлечь decoded RGBA для проверки byte-equality с исходником. (Можно передать `Option<arboard::Clipboard>` = `None` в тестовом конструкторе и проверять `state` + accumulated buf через test-only accessor.)
- [ ] unit-тест `incoming_text_reassembly_unchanged`: regression — text path работает как раньше.
- [ ] unit-тест `incoming_invalid_png_skipped`: format=1, payload не PNG → commit не паникует, image hash не записан в state.
- [ ] unit-тест `incoming_offer_during_reassembly_aborts_previous`: on_offer(0, 1024) → on_chunk(0, 256 байт) → on_offer(1, 512) → проверить что `received` очищен, `expected_format=1`, `expected_len=512`.
- [ ] unit-тест `incoming_reset_clears_state`: накапливаем 3 chunk'а → `reset()` → проверить что `expected_len=0`, `received.is_empty()`, counters обнулены.
- [ ] unit-тест `incoming_image_then_text_no_loop`: receive image → state становится `Image(h)` → следующий poll thread с тем же RGBA должен пропустить (regression для AC6).
- [ ] запустить `cargo test -p wiredesk-client` — все тесты зелёные.
- [ ] запустить `cargo clippy -p wiredesk-client --all-targets -- -D warnings` — clean.

### Task 6: Host side — симметричная реализация (отправка + приём + edge cases)

**Files:**
- Modify: `apps/wiredesk-host/src/clipboard.rs`
- Modify: `apps/wiredesk-host/src/session.rs` (или там, где Session диспатчит `Message::ClipOffer`)

- [ ] продублировать `encode_rgba_to_png` / `decode_png_to_rgba` / `hash_bytes` / `LastKind` enum логику из Mac (CLAUDE.md разрешает duplication).
- [ ] **удалить локальный `FORMAT_TEXT_UTF8: u8 = 0`** из `apps/wiredesk-host/src/clipboard.rs:13` — теперь импортируем `wiredesk_protocol::message::{FORMAT_TEXT_UTF8, FORMAT_PNG_IMAGE}` (константы добавлены в Task 1).
- [ ] заменить `last_hash: u64` в `ClipboardSync` на `last: LastKind` (без `Arc<Mutex>` — host clipboard sync однопоточный, в tick-loop).
- [ ] расширить `poll()`: после неудачного `get_text()` пробовать `get_image()`. Same flow: hash → dedup → encode → size-check (через тот же pure helper `check_image_size`) → ClipOffer{format=1} + ClipChunks.
- [ ] **прогресс-state на Host:** локальные поля `outgoing_progress: u64`, `outgoing_total: u64`, `incoming_progress: u64`, `incoming_total: u64` в `ClipboardSync` (просто `u64`, не `Arc<AtomicU64>` — Host single-threaded). Используются только для логирования в Task 8.
- [ ] **abort previous reassembly:** в `on_offer` — та же логика, что в Mac (warn + clear если новый offer пришёл во время незавершённой сборки).
- [ ] добавить `pub fn reset(&mut self)` — обнуляет всё incoming-state.
- [ ] **wiring `format` в Host-диспатче:** найти место где `Session` или `session.rs` обрабатывает `Message::ClipOffer` и убедиться что `format` передаётся в `clip.on_offer(format, total_len)`. Сейчас может быть `Message::ClipOffer { total_len, .. } => clip.on_offer(total_len)` — поправить на полное матчирование `{ format, total_len }`.
- [ ] **disconnect handling:** при потере соединения / новом Hello в Session — вызвать `clipboard.reset()`.
- [ ] расширить `commit()`: ветвление по `expected_format`, decode PNG → `set_image` при format=1, log::warn при invalid PNG.
- [ ] unit-тест `host::encode_decode_roundtrip`.
- [ ] unit-тест `host::hash_bytes_stable`.
- [ ] unit-тест `host::image_too_large_skipped` (через `check_image_size` helper с маленьким `limit`).
- [ ] unit-тест `host::incoming_image_reassembly`.
- [ ] unit-тест `host::incoming_invalid_png_skipped`.
- [ ] unit-тест `host::incoming_text_reassembly_unchanged`.
- [ ] unit-тест `host::incoming_offer_during_reassembly_aborts_previous`.
- [ ] unit-тест `host::reset_clears_state`.
- [ ] запустить `cargo test -p wiredesk-host` — все тесты зелёные.
- [ ] запустить `cargo clippy -p wiredesk-host --all-targets -- -D warnings` — clean.

### Task 7a: Status-line UI на Mac — counters + рендер прогресса

**Pre-discovery (≤5 мин перед началом):** найти точное место рендера status/connection-строки в `apps/wiredesk-client/src/app.rs` (вероятный кандидат — рядом с `render_capture_info` или в основном `update()` flow). Зафиксировать имя функции в комментарии этой задачи.

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs`
- Modify: `apps/wiredesk-client/src/main.rs`
- Modify: `apps/wiredesk-client/src/clipboard.rs`

- [ ] в `WireDeskApp` добавить четыре `Arc<AtomicU64>`: `outgoing_progress`, `outgoing_total`, `incoming_progress`, `incoming_total`.
- [ ] в `main.rs::main` создать эти `Arc<AtomicU64>`'ы и передать в `spawn_poll_thread` + `reader_thread` (через `IncomingClipboard::new`) + `WireDeskApp::new`.
- [ ] добавить pure helper `format_progress(action: &str, current: u64, total: u64) -> Option<String>`:
  - возвращает `Some("Sending image — 340/780 KB (43%)")` при `total > 0 && current <= total`,
  - возвращает `None` при `total == 0`.
- [ ] в обнаруженной status-render функции `app.rs`: вызвать `format_progress("Sending image", outgoing_progress.load(...), outgoing_total.load(...))` и `format_progress("Receiving image", ...)`. Конкатенировать с базовым `"Connected"` через `" | "`. Если оба None — рендер без изменений.
- [ ] **timer-сброс:** при `current >= total > 0` запомнить `last_complete: Option<Instant>`, через 1 сек после этого — `total.store(0, Relaxed)`. Простейшее решение — сделать обнуление в `clipboard.rs` сразу после отправки последнего chunk (writer thread); UI просто отрендерит что есть.
- [ ] **disconnect-сброс:** при `TransportEvent::Disconnected` в `WireDeskApp::handle_event` — `outgoing_progress/total/incoming_progress/total .store(0, Relaxed)`.
- [ ] unit-тест `format_progress_active`: `format_progress("Sending image", 340*1024, 780*1024)` содержит `"340"`, `"780"`, `"43%"`.
- [ ] unit-тест `format_progress_idle`: `total=0` → `None`.
- [ ] unit-тест `format_progress_complete`: `current == total` → `Some("... 100%)")`.
- [ ] запустить `cargo test -p wiredesk-client` — все тесты зелёные.
- [ ] запустить `cargo clippy --workspace --all-targets -- -D warnings` — clean.

### Task 7b: Toast при превышении лимита изображения

**Mechanism (зафиксировано):** расширить существующий `TransportEvent` enum в `apps/wiredesk-client/src/app.rs` новым variant'ом `TransportEvent::Toast(String)` — `clipboard.rs::spawn_poll_thread` использует **уже существующий `events_tx`**, никаких новых каналов. UI в `handle_event` сохраняет toast в `WireDeskApp.transient_toast: Option<(String, Instant)>` (если поля ещё нет — добавить) и рендерит 3 секунды.

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs`
- Modify: `apps/wiredesk-client/src/clipboard.rs`
- Modify: `apps/wiredesk-client/src/main.rs` (если `events_tx` нужно дополнительно clone'нуть в poll-thread)

- [ ] добавить `TransportEvent::Toast(String)` variant.
- [ ] добавить (если нет) `WireDeskApp.transient_toast: Option<(String, Instant)>` + рендер в `update()` (3 сек, потом `take()`).
- [ ] в `handle_event` обрабатывать `TransportEvent::Toast(msg)` → `self.transient_toast = Some((msg, Instant::now()))`.
- [ ] в `clipboard.rs::spawn_poll_thread` принимать `events_tx: mpsc::Sender<TransportEvent>` (clone из main.rs) и при `check_image_size(png.len(), MAX_IMAGE_BYTES) == Err(...)` отправлять `TransportEvent::Toast(format!("image too large ({} KB), copy a smaller selection", png.len() / 1024))`.
- [ ] unit-тест `toast_emitted_on_oversized_image`: synthetic case (через тестовый низкий лимит) → poll thread пушит `TransportEvent::Toast` в канал.
- [ ] запустить `cargo test -p wiredesk-client` — все тесты зелёные.
- [ ] запустить `cargo clippy --workspace --all-targets -- -D warnings` — clean.

### Task 8: Host — start/finish логи для image-transfer

**Files:**
- Modify: `apps/wiredesk-host/src/clipboard.rs`

- [ ] в `ClipboardSync::poll()` при создании ClipOffer{format=1} — `log::info!("clipboard: sending image to peer ({} bytes)", png.len())`.
- [ ] в `commit()` для image-ветки — `log::info!("clipboard: received image from peer ({} bytes)", total)`.
- [ ] не throttle'ить middle-of-transfer логи (нет UI, средний прогресс не нужен — start/finish достаточно).
- [ ] запустить `cargo test -p wiredesk-host` — все тесты зелёные (новых тестов в Task 8 не требуется — start/finish логи покрыты косвенно через существующие image-roundtrip тесты Task 6).
- [ ] запустить `cargo clippy -p wiredesk-host --all-targets -- -D warnings` — clean.

### Task 9: Verify acceptance criteria + final docs/move

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `/Users/pgmac/.claude/projects/-Users-pgmac-Data-prjcts-wiredesk/memory/project_wiredesk.md`
- Move: `docs/plans/20260502-clipboard-images.md` → `docs/plans/completed/20260502-clipboard-images.md`

- [ ] запустить `cargo test --workspace` — все тесты зелёные. Записать total count в commit message.
- [ ] запустить `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- [ ] запустить `cargo build --release --workspace` — собирается на macOS.
- [ ] запустить `./scripts/build-mac-app.sh` — успешно создаёт `target/release/WireDesk.app`.
- [ ] verify AC4 покрыт unit-тестом `image_too_large_skipped` (Task 4 + Task 6).
- [ ] verify AC7 удовлетворён (тесты зелёные, новые roundtrip-тесты в `wiredesk-protocol`).
- [ ] README.md: в секции «What WireDesk does» обновить bullet про clipboard — «Syncs clipboard text in both directions automatically» → «Syncs clipboard text **and PNG images** in both directions (images up to 1 MB encoded)».
- [ ] CLAUDE.md: в секции «Clipboard sync» добавить параграф про image format=1, MAX_IMAGE_BYTES, hash от RGBA, status-line counter, edge cases (interleaved offers, reset on disconnect).
- [ ] memory/project_wiredesk.md: обновить «Полный набор функций» — добавить пункт про image clipboard.
- [ ] обновить test count в CLAUDE.md / README.md (174 теста → новое значение).
- [ ] переместить план: `mkdir -p docs/plans/completed && mv docs/plans/20260502-clipboard-images.md docs/plans/completed/`.

## Post-Completion

*Items requiring manual intervention or external systems — informational only.*

**Live verification (на реальном железе Mac + Win с null-modem).**

**Bandwidth realism note.** Бриф изначально указывал AC1 «≤30 сек», но это покрывает только небольшие скриншоты (~250–330 KB encoded). Для FullHD-скриншота (типично 500 KB – 1 MB после PNG) при 11 KB/s wire-throughput реалистичны 50–100 секунд. AC1/AC2 ниже скорректированы — оригинальный AC1 в брифе остаётся как **soft target** для типичных use-cases.

- AC1: Mac `Cmd+Shift+Ctrl+4` (скриншот окна, 200–500 KB) → ≤60 сек → Win Cmd+V в Paint вставляет идентичную картинку. Скриншот всего экрана (~1 MB) → ≤120 сек.
- AC2: Win Snipping Tool (small region, ≤500 KB) → ≤60 сек → Mac Cmd+V в Preview вставляет идентичную картинку.
- AC3: regression — `Cmd+C` на тексте на обеих сторонах продолжает работать (text clipboard не сломан).
- AC4: скопировать большой скриншот (FullHD-полный, ~1.5 MB после PNG) → toast «image too large» появляется → Cmd+V на peer'е возвращает то, что было раньше (не пустоту, не зависание).
- AC5: во время передачи ~500 KB-картинки в status-line видно `"Sending image — N/M KB (P%)"`, прогресс растёт ≥ 2 раза за время передачи.
- AC6: после получения картинки на peer'е, тот peer не отправляет её обратно (loop avoidance — наблюдать через debug-log на отправке).
- **edge: interleaved** — Cmd+C на тексте, сразу же (до завершения) Cmd+C на картинке → на receiver'е в логе видно `"incoming offer aborted previous reassembly"`, картинка вставляется корректно.
- **edge: disconnect** — отсоединить serial-кабель в середине image-transfer → reconnect → status-line очищается, следующий Cmd+C+V работает (нет залипшего state).

**External system updates** — нет (всё внутри проекта).

**Бенчмарк** (опционально, если хочется измерить точно): записать timestamp начала отправки и timestamp Cmd+V на peer'е для скриншота 250 KB — должно быть ~25 сек ± 5 сек.
