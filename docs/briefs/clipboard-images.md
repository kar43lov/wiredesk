## Бриф: Clipboard-картинки

**Цель.** Двунаправленная синхронизация PNG-картинок в clipboard между macOS-клиентом и Windows-хостом через существующий chunked-протокол.

**Выбранный подход.** A — reuse `ClipOffer`/`ClipChunk` с `format=1` (PNG). Минимум изменений протокола, никакого дублирования chunked-логики, открывает дверь будущим форматам (file-list = `format=2`) без ломки совместимости. Альтернативы (новые message types, generic mime-type) отвергнуты как overkill / YAGNI-violation.

**Требования.**
- Wire format: PNG lossless, единственный кодек.
- Лимит: `MAX_IMAGE_BYTES = 1 MB` (encoded). Превышение → toast в status-line, ничего не отправлено.
- Текстовый clipboard (`format=0`) продолжает работать без изменений.
- PNG encoding/decoding — через `image` crate (минимальные features: `png`). Уже в transitive deps.
- Loop avoidance — hash от **RGBA** bytes (не encoded PNG, чтобы избежать round-trip drift).
- UX: status-line показывает `"Sending image — N/M bytes (P%)"` через `Arc<AtomicU64>` counter из writer thread.

**Acceptance criteria.**
- AC1. Mac Cmd+Shift+Ctrl+4 (screenshot в clipboard) → ≤30 сек → Win Cmd+V в Paint вставляет идентичную картинку.
- AC2. Win Snipping Tool → Mac Cmd+V в Preview вставляет идентичную картинку.
- AC3. Текстовый Cmd+C/V не сломан (regression).
- AC4. Картинка > 1MB encoded → toast, ничего не отправлено.
- AC5. Status-line обновляется во время передачи (видно прогресс).
- AC6. Loop avoidance работает (peer не возвращает только что полученную картинку).
- AC7. `cargo test --workspace` зелёный + новые roundtrip-тесты.
- AC8. `cargo clippy --workspace --all-targets -- -D warnings` clean.

**Тестирование.**
- T1. Protocol: roundtrip `ClipOffer { format: 1 }`.
- T2. Client clipboard: synthetic 4×4 RGBA → encode PNG → decode → byte-equal.
- T3. Client clipboard: hash stability на RGBA.
- T4. Host clipboard: симметрично T2/T3.
- T5. IncomingClipboard reassembly: chunked PNG → commit → decode успешен.
- Live AC1/AC2/AC3/AC4/AC5/AC6.

**Риски.**
- arboard `get_image()` ошибки при non-image clipboard — silent skip, как для текста.
- PNG encode latency на больших RGBA (~50–150 ms) — выполняется в poll thread, UI не блокируется.
- Bandwidth: 1MB ≈ 100 сек @ 11 KB/s. Долго, но в рамках выбранного scope (status-line компенсирует).
- Hash коллизия RGBA (DefaultHasher u64 на ~8MB) — пренебрежимо.

**Первые шаги.**
1. Расширить `wiredesk-protocol`: добавить константу `FORMAT_PNG_IMAGE: u8 = 1`, добавить тест `roundtrip_clip_offer_image`.
2. Добавить `image = { version = "0.25", default-features = false, features = ["png"] }` в `wiredesk-client/Cargo.toml` и `wiredesk-host/Cargo.toml`.
3. Расширить `ClipboardState` (Mac) и `ClipboardSync` (Host): хранить `last_kind: Option<ClipKind>` где `ClipKind = Text(u64) | Image(u64)`, hash от RGBA для image.
4. Реализовать encode (poll path) и decode + `set_image` (commit path) на обеих сторонах. Reassembly меняем минимально — сохранить `format` из offer'а.
5. Status-line: shared counter `Arc<AtomicU64>` для отправляемого, поле в `WireDeskApp` + рендер; принимающая сторона аналогично.
6. Тесты T1–T5 + live AC.

**Сложность:** medium (несложно индивидуально, но касается сразу 4 файлов на 2 сторонах + UI + протокол + новый dep).

**Ветка:** `feat/clipboard-rich` (уже создана).
