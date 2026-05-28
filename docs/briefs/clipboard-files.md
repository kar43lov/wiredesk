## Бриф: Clipboard-файлы

**Цель.** Двунаправленная синхронизация одиночных файлов в clipboard между macOS-клиентом и Windows-хостом, поверх существующего chunked-protocol'а — закрыть half-baked clipboard-фичу для daily use.

**Выбранный подход.** A — Reuse-max. Новая константа `FORMAT_FILE: u8 = 2` в существующем `ClipOffer { format, total_len }` / `ClipChunk { index, data }` pipeline. Filename живёт **inline в первом chunk** как `[name_len: u16][name_utf8][content_bytes...]`. Protocol surface не растёт — никаких новых opcodes. Подход прямо закладывался старым брифом `clipboard-images.md` как естественная эволюция через format-discriminator.

**Альтернативы (отвергнуто):**
- B — Новый `ClipFileOffer` opcode: чище, но +~3 дня effort за minor отдачу. Memory-rule `binary_protocol_extension` применима к **расширению existing payload**, а у нас новое значение already-extensible discriminator — design intent ClipOffer'а с самого начала.
- C — выкинуть arboard полностью: ~2 нед, риск regression в работающем text/image, overkill для MVP.

### Контекст релевантности

- FT232H @ 3 Mbaud verified live 2026-05-28: 20 MB файл идёт ~70 сек. До FT232H фича была малополезна (на CH340 11 KB/s — 20 MB = 30 мин).
- Continent-АП режет все network-каналы, поэтому файлы между Win-host и Mac реально таскать нечем (fz через Континент-туннель / физический USB). Clipboard-copy закрывает основной workflow.

### Требования

**Функциональные:**
- F1. `FORMAT_FILE: u8 = 2` в `wiredesk-protocol::message`.
- F2. Mac NSPasteboard FFI: read `public.file-url` (NSURL list), write NSURL fileURL.
- F3. Win Clipboard FFI: read `CF_HDROP` (DROPFILES struct), write CF_HDROP с wide-char paths.
- F4. Cap `MAX_FILE_BYTES = 20 MB` (raw content) — uniform с PNG, никаких protocol-изменений.
- F5. Receive-side temp path: Mac `~/Library/Caches/WireDesk/<basename>`, Win `%TEMP%\WireDesk\<basename>`.
- F6. Cache vacuum при старте: удалить файлы из WireDesk cache dir старше 24h.
- F7. Filename sanitize на receive-side: strip path separators (`/` и `\`), отбросить leading `..`, оставить basename.
- F8. Loop avoidance: `LastSeen.file: Option<u64>` — hash от file content (не от path).
- F9. Settings UI checkbox "Receive files" на обеих сторонах рядом с "Receive images".
- F10. Progress / cancel — reuse pipeline'а от image transfer.
- F11. UI feedback: status-line "Sending file 'X.pdf' — N/M bytes (P%)", toast при отказе/overcap.

**Нефункциональные:**
- Autosend семантика (как у картинок). Никаких prompt'ов перед Cmd+C.
- Single-file scope. Multi-select из Finder/Explorer — Phase 2 отдельным брифом.
- No directory support. Phase 4 (если когда-нибудь).

### Acceptance criteria

- **AC1.** Cmd+C на одном файле в Finder (Mac) → ≤2 мин (на 20 MB) → Cmd+V в Explorer (Win) создаёт файл с тем же content (sha256 совпадает) и filename.
- **AC2.** Win Explorer Cmd+C → Mac Cmd+V в Finder создаёт файл с тем же content и filename.
- **AC3.** Текст + картинки продолжают работать (regression).
- **AC4.** Файл > 20 MB → toast "File too large", ничего не отправлено.
- **AC5.** Filename с unicode/emoji (`привет 🎉.pdf`) — preserved both directions.
- **AC6.** Filename с path-traversal (`../../etc/passwd`) — sanitized в basename на receive-side, не пишет за пределы cache dir.
- **AC7.** Loop avoidance: paste файла обратно на source-сторону в течение 10 сек не вызывает повторного round-trip'а (per-format LastSeen slot).
- **AC8.** Cancel на progress-bar обрывает chunked transfer и не оставляет частичных файлов на receive-стороне.
- **AC9.** Receive-side toggle "Receive files" off → отказ через `ClipDecline { format: FORMAT_FILE }`, send-side показывает toast.
- **AC10.** `cargo test --workspace` зелёный + `cargo clippy --workspace --all-targets -- -D warnings` clean.

### Тестирование

- **T1.** Protocol: roundtrip `ClipOffer { format: FORMAT_FILE, total_len: 1024 }`.
- **T2.** Protocol: `FORMAT_TEXT_UTF8=0`, `FORMAT_PNG_IMAGE=1`, `FORMAT_FILE=2` — distinct.
- **T3.** First-chunk format parse: serialize/deserialize `[name_len:u16][name_utf8][content]` с unicode filename — byte-equal roundtrip.
- **T4.** Filename sanitize: `../foo`, `..\..\foo`, `/abs/foo`, `C:\abs\foo`, `foo/../bar` → все мап-ятся в basename без path components.
- **T5.** Hash stability: same content → same hash. Different filename + same content → same hash (slot dedup'ится по content, не по name).
- **T6.** Mac NSPasteboard: write fileURL → read back via FFI → URL identical (smoke-тест, `#[ignore]` если CI без GUI).
- **T7.** Win CF_HDROP: write DROPFILES → read back → path identical (smoke-тест с tempfile).
- **T8.** Vacuum cache: создать файл с mtime -25h → vacuum → файл удалён. mtime -23h → файл сохранён.
- **Live:** AC1-AC9 на реальном hardware (FT232H link).

### Что НЕ входит в scope

- **Multi-file selection** (Cmd+C на нескольких файлах) — Phase 2, отдельный бриф.
- **Directories** (zip on-fly) — Phase 4 (если когда-нибудь).
- **File size > 20 MB** — отдельный bump-cap brief после live-validation цены на 20 MB.
- **Mac extended attributes** (quarantine, color labels), **Win NTFS streams** — preserved только basename + content. Quarantine на Win→Mac side — receive-side Mac пометит сам (since файл из non-trusted source).
- **Symlinks** — resolved to target file, не preserved.
- **Перезапись подтверждение** на receive-side — пишем в Cache/temp dir, user сам paste'ит в реальное место через Finder/Explorer.

### Риски

- **R1. Mac NSPasteboard FFI complexity.** Самый большой unknown. Mitigation: смок-тест с одним fileURL до полной интеграции, выбор `objc2-app-kit` (modern + maintained) вместо `objc`/`cocoa` (legacy).
- **R2. Vacuum lifetime mismatch.** User копирует файл, через 25h хочет paste, файл уже удалён vacuum'ом. Trade-off приемлемый (24h обычно > workflow timeline); можно потом bump'нуть до 7d.
- **R3. Concurrent text/image/file Cmd+C race.** Per-format LastSeen slot уже работает для text+image — добавить `file: Option<u64>` и matching dedup logic.
- **R4. CHUNK_SIZE limit.** 1024 × 65535 = 64 MB chunked upper-bound. 20 MB cap влезает с запасом 3×.
- **R5. Filename clash в cache.** Два sequential Cmd+C на файлах с одним именем (но разным content) → второй переписывает первый. Acceptable for MVP (как при ручном copy в одну папку).
- **R6. Filename byte-length vs char-length.** `name_len: u16` — байт-длина UTF-8. Cap имени 64 KB (max u16) более чем достаточен.

### Первые шаги

1. **Protocol-layer (T1, T2)**: добавить `pub const FORMAT_FILE: u8 = 2` в `crates/wiredesk-protocol/src/message.rs` + roundtrip-тест + distinct-constants assert.
2. **Filename packing helpers (T3, T4)**: pure functions `pack_first_chunk(name, content) -> Vec<u8>` + `unpack_first_chunk(bytes) -> (name, rest)` + `sanitize_basename(raw) -> String` в `crates/wiredesk-protocol` (или новый crate `wiredesk-clipboard-files`). Тесты на unicode + path-traversal.
3. **Mac platform glue**: новый модуль `apps/wiredesk-client/src/clipboard_files.rs` с `objc2-app-kit` FFI — функции `poll_file_url() -> Option<PathBuf>` и `set_file_url(path: &Path)`. Smoke-тест (T6).
4. **Win platform glue**: новый модуль `apps/wiredesk-host/src/clipboard_files.rs` с `windows` crate — функции `poll_cf_hdrop() -> Option<PathBuf>` и `set_cf_hdrop(path: &Path)`. Smoke-тест (T7).
5. **Cache vacuum (T8)**: hook в `main.rs` start (Mac + Win): пройтись по cache dir, удалить файлы с mtime > 24h.
6. **LastSeen.file slot**: расширить `apps/wiredesk-client/src/clipboard.rs::LastSeen` полем `file: Option<u64>` + `matches_file_hash()` + `set_file()`. То же для Win-side `LastKind::File(u64)`.
7. **Poll path**: extend Mac `clipboard.rs` poll thread — после text/image веток добавить file branch (poll fileURL → read content → hash → dedup → chunked send). Симметрично Win-side в `apps/wiredesk-host/src/clipboard.rs::tick`.
8. **Receive path**: extend `IncomingClipboard` reassembly — branch на `format == FORMAT_FILE`, unpack first chunk, sanitize basename, write to cache dir, call `set_file_url`/`set_cf_hdrop` через platform glue.
9. **Settings UI**: новый checkbox "Receive files" в Mac chrome panel (рядом с "Receive images") + Win nwg Settings.
10. **Live AC1-AC9** на железе + cargo test + clippy clean.

**Сложность:** medium-low. Pipeline (chunked transfer, progress, cancel, LRU dedup, status-line) переиспользуется. Новое: 2 platform FFI модуля + filename packing semantics + cache vacuum.

**Ветка:** `feat/clipboard-files` (создать новую — старая `feat/clipboard-rich` уже merged как PR #7, имя занято semantically).

**Зависимости:**
- Жёстких нет. FT232H уже shipped — perf готов.
- Soft: после `feat/host-port-dropdown` (PR #24, уже merged) — ничего общего, просто сосед.

**Estimated effort:** 3-5 дней (один файл, bidirectional, 20 MB cap). Без multi-select / directories.
