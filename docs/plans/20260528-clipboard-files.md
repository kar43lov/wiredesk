# Clipboard Files — Single-file bidirectional sync (Mac ↔ Win)

## Overview

Двунаправленная синхронизация одиночных файлов через clipboard между macOS-клиентом и Windows-хостом, поверх существующего chunked-protocol'а. Закрывает half-baked clipboard-фичу для daily use в Континент-АП среде, где Wi-Fi/LAN режутся WFP-фильтрами.

**Подход:** новая константа `FORMAT_FILE: u8 = 2` в существующем `ClipOffer { format, total_len }` / `ClipChunk { index, data }` pipeline. Filename живёт inline в первом chunk как `[name_len: u16][name_utf8][content_bytes...]`. Никаких новых opcodes — design intent ClipOffer'а уже extensible.

**Зачем сейчас:** FT232H @ 3 Mbaud verified live 2026-05-28 → 20 MB файл идёт ~70 сек (на CH340 11 KB/s было ~30 мин — фича была малополезна). Канал готов.

**Source of truth:** `docs/briefs/clipboard-files.md` (полная спецификация).

**Brief → Plan mapping**: все 11 функциональных требований (F1-F11), все 10 acceptance criteria (AC1-AC10), все 8 test classes (T1-T8) из брифа адресованы в Tasks 1-15. Live AC1-AC10 verified в Task 16.

## Context (from discovery)

**Файлы/компоненты:**
- `crates/wiredesk-protocol/src/message.rs` — `FORMAT_TEXT_UTF8=0`, `FORMAT_PNG_IMAGE=1` константы, `Message::ClipOffer/ClipChunk/ClipDecline`, `MessageType` enum (0x22 ClipAck, 0x23 ClipDecline уже заняты — добавлять opcode не нужно).
- `apps/wiredesk-client/src/clipboard.rs` — Mac side: `LastSeen { text_history, image, oversize_image }`, `ClipboardState`, poll thread, `IncomingClipboard` reassembly. Размер cap `MAX_IMAGE_BYTES = 20 MB`. `emit_offer_and_chunks`, `apply_outgoing_progress` — wire-level pipeline.
- `apps/wiredesk-host/src/clipboard.rs` — Win side: `LastKind::{None,Text,Image,OversizeImage}`, `ClipboardSync` single-threaded в `Session::tick`. `build_offer_and_chunks`, `stamp_initial` (startup pre-stamp).
- `apps/wiredesk-client/src/app.rs:129-135,295-348,643-667` — `receive_text`/`receive_images` `Arc<AtomicBool>` toggles + Mac Settings UI checkboxes.
- `apps/wiredesk-host/src/settings_ui.rs` — Win nwg Settings (структура зеркальна Mac).
- `apps/wiredesk-client/src/main.rs:608-637` — `reset_session_state` на reconnect, `IncomingClipboard::reset()`.
- `crates/wiredesk-core/` — общий crate с типами `WireDeskError`, `Result`. Логичное место для платформо-независимой logic не-протокольного уровня (см. Task 3 ниже).

**Patterns:**
- `emit_offer_and_chunks(format, payload)` — pure helper для outbound (Mac).
- `build_offer_and_chunks(format, payload) -> Vec<Message>` — pure helper для outbound (Win).
- `IncomingClipboard::on_offer/on_chunk/commit` — state machine на receive-side, branch'ится по `expected_format`.
- `stamp_initial` — pre-stamp existing clipboard на startup, чтобы pre-existing content не отправлялся.
- `check_image_size` + `format_oversize_toast` — pure helper'ы для size cap'а; такие же нужны для file.
- `apply_outgoing_progress` — wire-level progress tracker; ветвится по `format` в log-сообщении ("image"/"text") — нужно подключить "file".

**Dependencies:**
- `arboard` 3.x — не поддерживает file URLs (Mac) и CF_HDROP (Win). Нужен FFI.
- `windows` crate — уже в deps host'а; даёт `Win32::System::DataExchange::*`, `Win32::Foundation::*`. Возможно нужны новые features (Task 5).
- `objc2-app-kit`, `objc2-foundation` — нужно добавить в Mac client. **Версии резолвить динамически через cargo search / context7 на старте Task 4** — не пинить версии заранее (текущие могут устареть до старта).

## Development Approach

- **Testing approach**: **Regular** (code first, тесты вместе с кодом в той же задаче).
- Pure helpers (sanitize, pack/unpack, vacuum mtime check) — unit-тесты обязательны, легко мокаются.
- Platform FFI — smoke-тесты с `#[ignore]` + `#[cfg(target_os = "...")]` маркерами (требуют GUI session для NSPasteboard / clipboard owner для CF_HDROP); CI всё равно их не запустит.
- Полная задача = код + тесты + tests pass перед переходом к следующей.
- **CRITICAL: каждая задача обязательно с тестами** — даже platform FFI задачи (smoke-тест + pure helpers внутри).
- **CRITICAL: все тесты зелёные перед next task.**
- Backward compat: текст + картинки не должны сломаться (regression AC3).
- Обновлять этот файл при scope-change'ах в ходе работы.
- Test runner: **`cargo test --workspace -- --test-threads=1`** (host parallel flake на macOS — pre-existing baseline).

## Testing Strategy

- **Unit tests**: pure helpers (pack/unpack first chunk, sanitize_basename, cache vacuum mtime filter, size check, format constants, dedup-slot independence).
- **Reassembly tests**: chunked file → `IncomingClipboard::commit` → byte-equal с original content; filename roundtrip; unicode filename; path-traversal sanitize.
- **Smoke FFI tests**: write fileURL → read back (Mac), write DROPFILES → read back (Win). `#[ignore]` маркер.
- **Regression tests**: text/image roundtrip продолжают работать после изменений.
- **No e2e**: у проекта нет browser-style e2e (CLI/serial). Live AC1-AC10 на железе после Task 15.

## Progress Tracking

- Mark completed items with `[x]` immediately when done.
- Add newly discovered tasks with ➕ prefix.
- Document issues/blockers with ⚠️ prefix.
- Update plan if implementation deviates from original scope.

## Solution Overview

**Architecture:** existing chunked pipeline reused 1:1. Filename packed в payload первого chunk, sanitize/write happens в receive-side handler. Platform clipboard glue вынесен в отдельные модули `clipboard_files.rs` на каждой стороне — изолирует FFI complexity. Loop-avoidance — hash от content (как RGBA для image), новый slot `LastSeen.file`/`LastKind::File`.

**Data flow Mac→Win:**
```
Mac NSPasteboard poll
  → public.file-url detected (single URL only — multi rejected)
  → read file content (bytes)
  → hash content → dedup vs LastSeen.file
  → pack first chunk [name_len][name][prefix-bytes]
  → emit_offer_and_chunks(FORMAT_FILE, packed)
  → wire → Win
Win recv:
  → IncomingClipboard.on_offer(format=2)  [respects receive_files flag → ClipDecline on off]
  → on_chunk × N
  → commit: unpack first chunk → sanitize basename
  → write to %TEMP%\WireDesk\<basename>
  → set_cf_hdrop(path)
  → stamp LastKind::File(content_hash)
```

Win→Mac симметрично.

**Key design decisions:**
1. `[name_len: u16]` (max 64 KB filename) — bytes, не chars; UTF-8 raw.
2. Cache lifetime 24h — vacuum при startup. Trade-off ради простоты vs persistence.
3. `sanitize_basename` отбрасывает все path-components, оставляет только final segment + strip `..`, обрабатывает Windows drive-letters + reserved names.
4. Hash от content (не от name) — copy-rename-paste не зацикливается; same content → same hash → dedup.
5. CHUNK_SIZE=1024 переиспользуется — никаких изменений в outbox/heartbeat-budget logic.
6. Multi-file selection (>1 URL/path) → silent skip + debug log; не Phase 1 scope.

## Technical Details

### Protocol delta

```rust
pub const FORMAT_FILE: u8 = 2;
```

`ClipOffer.total_len` для file = `2 + name.len() + content.len()` (включая 2-байтный `name_len` префикс).

### First-chunk layout

```
[name_len: u16 LE][name: UTF-8 bytes][content: raw bytes][padding ?]
```

Где `name_len` = byte-длина UTF-8 имени (не char count). Content начинается сразу после имени; перетекает через границы chunks по mod-CHUNK_SIZE.

### Cache paths

- Mac: `dirs::cache_dir()/WireDesk/` → `~/Library/Caches/WireDesk/`
- Win: `std::env::var("TEMP")` (или `dirs::cache_dir()`) → `%TEMP%\WireDesk\`

### LastSeen extension

Mac:
```rust
pub(crate) struct LastSeen {
    pub text_history: VecDeque<u64>,
    pub image: Option<u64>,
    pub oversize_image: Option<u64>,
    pub file: Option<u64>,            // ← new
    pub oversize_file: Option<u64>,   // ← new
}
```

Win симметрично — добавить `LastKind::File(u64)` и `LastKind::OversizeFile(u64)`.

### Settings additions

Mac `Config` + Win `Config`: `receive_files: bool` (default true). Arc<AtomicBool> wiring как у `receive_images`.

## What Goes Where

- **Implementation Steps** (`[ ]`): protocol + helpers + FFI modules + integration + tests.
- **Post-Completion** (no checkboxes): live hardware AC1-AC10 verification на FT232H link.

## Implementation Steps

### Task 1: Protocol layer — FORMAT_FILE constant + tests

**Files:**
- Modify: `crates/wiredesk-protocol/src/message.rs`

- [x] Add `pub const FORMAT_FILE: u8 = 2;` рядом с существующими `FORMAT_TEXT_UTF8`/`FORMAT_PNG_IMAGE` constants.
- [x] Update doc comment над format constants: явно перечислить text/image/file.
- [x] Add roundtrip test `roundtrip_clip_offer_file`: `Message::ClipOffer { format: FORMAT_FILE, total_len: 65536 }` → serialize → deserialize → equal (brief T1).
- [x] Update `clip_format_constants_are_distinct` test: assert `FORMAT_FILE = 2` + `FORMAT_FILE != FORMAT_TEXT_UTF8` + `FORMAT_FILE != FORMAT_PNG_IMAGE` (brief T2).
- [x] Add `roundtrip_clip_decline_file`: `Message::ClipDecline { format: FORMAT_FILE }` roundtrip.
- [x] Run `cargo test -p wiredesk-protocol` — must pass before Task 2.

### Task 2: Filename packing helpers + sanitize_basename

**Files:**
- Create: `crates/wiredesk-protocol/src/clip_file.rs`
- Modify: `crates/wiredesk-protocol/src/lib.rs` (mod clip_file + reexport)

- [x] Create `clip_file.rs` со следующими pure helper'ами:
  - `pub fn pack_first_chunk(name: &str, content: &[u8]) -> Result<Vec<u8>, ClipFileError>` — формирует `[u16 LE name_len][name][content]`. Возвращает Err если `name.as_bytes().len() > MAX_FILENAME_LEN`.
  - `pub fn unpack_first_chunk(payload: &[u8]) -> Result<(String, Vec<u8>), ClipFileError>` — парсит обратно. Err на труб'ированный header, invalid UTF-8 в name, name_len > payload.
  - `pub fn sanitize_basename(raw: &str) -> String` — strip path separators (`/`, `\`), отбросить все `..` segments, strip Windows drive-letter (`C:`), strip leading `:` или space. Reserved NTFS names (`CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9` — case-insensitive, до и с extension) префиксуются `_`. Empty → fallback `"clipboard.bin"`.
- [x] `pub const MAX_FILE_BYTES: usize = 20 * 1024 * 1024;` — uniform с PNG cap.
- [x] `pub const MAX_FILENAME_LEN: usize = 4096;` — safety cap, ОС лимиты ~255-1024.
- [x] Add `pub enum ClipFileError { Truncated, BadUtf8, NameTooLong, EmptyName }`.
- [x] Write unit tests:
  - `pack_unpack_roundtrip` — ascii name + 1KB content → byte-equal back (brief T3).
  - `pack_unpack_unicode` — `"привет 🎉.pdf"` name + binary content → preserved (brief T3).
  - `unpack_truncated_header` → `Err(Truncated)`.
  - `unpack_truncated_name` → `Err(Truncated)`.
  - `unpack_invalid_utf8_name` → `Err(BadUtf8)`.
  - `sanitize_strips_path` — `"../foo"`, `"..\\foo"`, `"/abs/path/foo"`, `"foo/../bar"` → `"foo"`/`"bar"` (brief T4).
  - `sanitize_strips_windows_drive` — `"C:\\abs\\foo"`, `"C:foo"`, `"C:"` → `"foo"`, `"foo"`, fallback (brief T4).
  - `sanitize_reserved_ntfs_names` — `"CON"`, `"con.txt"`, `"PRN.dat"`, `"COM1.log"`, `"LPT9"` → префиксованы `_`. `"console.txt"` остаётся как есть (prefix-only collision не считается).
  - `sanitize_empty_fallback` — `""` → `"clipboard.bin"`.
  - `sanitize_dot_dot_only` — `".."` → `"clipboard.bin"`.
  - `sanitize_unicode_basename` — `"папка/файл 🎉.txt"` → `"файл 🎉.txt"`.
  - `pack_name_too_long` — name > `MAX_FILENAME_LEN` → `Err(NameTooLong)`.
- [x] Run `cargo test -p wiredesk-protocol` — must pass before Task 3.

### Task 3: Cache vacuum helper (in wiredesk-core)

**Files:**
- Create: `crates/wiredesk-core/src/cache_vacuum.rs`
- Modify: `crates/wiredesk-core/src/lib.rs` (mod cache_vacuum)

Rationale (revised after plan-review): cache_vacuum touches `std::fs`/`std::time` — это не место для protocol crate (pure wire-format). Размещаю в `wiredesk-core` — он уже общий crate с error/types и используется обоими apps.

- [x] Create `cache_vacuum.rs`:
  - `pub fn vacuum_cache_dir(dir: &Path, older_than: Duration) -> Result<usize, std::io::Error>` — read_dir, для каждого regular file: mtime > older_than → remove. Возвращает count удалённых. Silent skip для read errors на отдельных файлах (log::warn). Non-existent dir → Ok(0).
- [x] Pure helper для тестируемости:
  - `pub fn should_remove(mtime: SystemTime, now: SystemTime, older_than: Duration) -> bool` — `now.duration_since(mtime).map(|d| d > older_than).unwrap_or(false)`.
- [x] Write tests (brief T8):
  - `should_remove_old_file` — mtime = now - 25h, older_than = 24h → true.
  - `should_remove_young_file` — mtime = now - 23h → false.
  - `should_remove_future_mtime` — mtime > now (clock skew) → false (no panic).
  - `vacuum_dir_removes_old_files` — tempdir + create 2 files, set mtime via `filetime` crate, vacuum, assert old removed/new survives.
  - `vacuum_dir_ignores_subdirs` — поддиректории не трогать (только regular files).
  - `vacuum_missing_dir_ok` — non-existent path → Ok(0).
- [x] Add `filetime` dev-dep если нужно для setting mtime в тестах.
- [x] Run `cargo test -p wiredesk-core` — must pass before Task 4.

### Task 4: Mac NSPasteboard file FFI module

**Files:**
- Create: `apps/wiredesk-client/src/clipboard_files.rs`
- Modify: `apps/wiredesk-client/Cargo.toml`
- Modify: `apps/wiredesk-client/src/main.rs` (`mod clipboard_files`)

- [x] **Resolve current versions first**: ran `cargo search` (objc2=0.6.4, objc2-app-kit=0.3.2, objc2-foundation=0.3.2). **Decision**: stick with existing project versions (objc2=0.5, objc2-app-kit=0.2, objc2-foundation=0.2) — upgrading the major version would force API migration of `status_bar.rs` / `monitor.rs` / `main.rs` (out-of-scope for clipboard files). Both 0.2.2 and 0.3.2 expose NSPasteboard / NSURL with equivalent shape.
- [x] Add deps в `Cargo.toml`:
  ```toml
  # Already at objc2 = "0.5"; expanded existing app-kit/foundation feature lists:
  objc2-app-kit = { version = "0.2", features = ["NSScreen", "NSStatusBar", "NSStatusItem", "NSPasteboard", "NSPasteboardItem"] }
  objc2-foundation = { version = "0.2", features = ["NSArray", "NSString", "NSGeometry", "NSThread", "NSURL"] }
  ```
- [x] Create `clipboard_files.rs` со следующими public функциями:
  - `pub fn poll_file_url(last_change_count: &mut i64) -> Option<PathBuf>` — `NSPasteboard.generalPasteboard.changeCount()`, если изменился И `pasteboardItems().count() == 1` И item.stringForType(NSPasteboardTypeFileURL) → parse `file://` URL → PathBuf. Multi-file → None + log debug "multi-file selection skipped, out of Phase 1 scope". Counter bumped eagerly даже на skip — иначе sticky multi-file selection пересканировался бы каждый tick.
  - `pub fn set_file_url(path: &Path) -> Result<(), FileClipboardError>` — clear pasteboard, write `NSURL.fileURLWithPath:` + `NSPasteboard.writeObjects([url])` через `ProtocolObject<dyn NSPasteboardWriting>` array.
  - `pub enum FileClipboardError { PasteboardUnavailable, BadPath(String), FfiError(String) }` (thiserror-derived).
  - Bonus pure helpers: `parse_file_url(&str) -> Option<PathBuf>` + `percent_decode(&str) -> String` — RFC-3986 percent decoding для NSURL-produced strings (spaces, unicode); вынесены чтобы URL-parsing path был unit-tested без AppKit session.
- [x] Logging: `log::debug!` на multi-file skip / non-file:// URL detected; никаких log::warn! — FFI errors сейчас только из `writeObjects` returning NO и переносятся в `FfiError` для caller'а.
- [x] Write tests:
  - `set_file_url_returns_ok_for_valid_path` — `#[ignore]` + `#[cfg(target_os = "macos")]` smoke (brief T6) — ✓.
  - `poll_change_count_increments_dedup` — pure-logic тест на change_count tracking (без FFI) — ✓.
  - `polling_multi_file_returns_none` — `#[ignore]` smoke с pre-populated multi-file pasteboard — ✓.
  - Bonus pure tests: `parse_file_url_simple/with_localhost_host/with_space_encoded/unicode_percent_encoded/rejects_http/rejects_empty_path`, `percent_decode_passthrough_invalid_hex/handles_truncated_tail`, `set_file_url_rejects_empty_path` — 10 passing.
- [x] Compile-check на Mac: `cargo check -p wiredesk-client` ✓ clean.
- [x] Run `cargo test -p wiredesk-client -- --test-threads=1` (skip ignored) — 182 passed; 0 failed; 3 ignored. Workspace-wide: 534 passed.

### Task 5: Win CF_HDROP file FFI module

**Files:**
- Create: `apps/wiredesk-host/src/clipboard_files.rs`
- Modify: `apps/wiredesk-host/Cargo.toml` (add windows features if needed)
- Modify: `apps/wiredesk-host/src/main.rs` (mod clipboard_files)

- [x] Verify `windows` crate features в `Cargo.toml` уже включают:
  - `Win32_System_DataExchange` (Open/Close/Get/SetClipboardData/EmptyClipboard) ✓ added
  - `Win32_System_Memory` (GlobalAlloc/GlobalLock/GlobalUnlock) ✓ added
  - `Win32_System_Ole` (CF_HDROP — lives here, not in Shell) ✓ added
  - `Win32_UI_Shell` (DROPFILES, DragQueryFileW, HDROP) ✓ added
  - `Win32_Foundation` (HANDLE, BOOL, POINT, HGLOBAL, GlobalFree — lives here) ✓ pre-existing
- [x] Create `clipboard_files.rs` со следующими public функциями:
  - `pub fn poll_cf_hdrop() -> Option<PathBuf>` — `OpenClipboard(None)` → `GetClipboardData(CF_HDROP)`. `GlobalLock` → HDROP handle. Count via `DragQueryFileW(handle, 0xFFFFFFFF, None)`; если count != 1 → None + log debug. Иначе query length, allocate `Vec<u16>`, `DragQueryFileW(handle, 0, Some(&mut buf))` → UTF-16 → PathBuf via `String::from_utf16_lossy`. `GlobalUnlock` + `CloseClipboard` в каждой return-ветке.
  - `pub fn set_cf_hdrop(path: &Path) -> Result<(), FileClipboardError>` — UTF-16 encode path + двойной NUL, `GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT)`, write DROPFILES header + path block via `ptr::write_unaligned` + `copy_nonoverlapping`, `OpenClipboard` → `EmptyClipboard` → `SetClipboardData(CF_HDROP, handle)`. На success — ownership transferred (don't free); на error — `GlobalFree`.
  - `pub enum FileClipboardError { ClipboardLocked, BadPath(String), AllocFailed, FfiError(String) }` (thiserror-derived; `BadPath` несёт path string для diagnostics).
- [x] DROPFILES struct — `#[repr(C)]` со scalar fields (p_files, pt_x, pt_y, f_nc, f_wide). `pt: POINT` развёрнут в pt_x/pt_y чтобы не тянуть зависимость от windows-rs Foundation тип (struct nameable в pure unit tests). Compile-time `const _: () = assert!(size_of::<DropFiles>() == 20)` ловит layout drift на all targets.
- [x] Path layout: `<wide path bytes>\0\0` (double-null terminated). Encoded через `path_str.encode_utf16().collect()` + `push(0)` × 2.
- [x] Logging: `log::debug!` на OpenClipboard failure + multi-file skip. Error paths возвращают типизированные `FileClipboardError`-варианты — host clipboard.rs позже решает что log'ать (`log::warn!`).
- [x] Write tests:
  - `dropfiles_layout_offset_correct` — assert `size_of::<DropFiles>() == 20` + `DROPFILES_SIZE == 20` ✓.
  - `dropfiles_fields_have_expected_types` — fresh struct size invariant ✓.
  - `set_then_get_roundtrip` — `#[ignore]` + `#[cfg(target_os = "windows")]` smoke (brief T7) ✓.
  - `poll_returns_none_when_no_cf_hdrop` — `#[ignore]` smoke (text clipboard) ✓.
  - `poll_returns_none_for_multi_file` — `#[ignore]` smoke c multi-file selection ✓.
  - Bonus pure tests: `set_cf_hdrop_rejects_empty_path` + non-Windows stub contract tests (`poll_cf_hdrop_stub_returns_none`, `set_cf_hdrop_stub_returns_unavailable`) — 5 passing on macOS.
- [x] Compile-check: cross-compile `cargo check -p wiredesk-host --target x86_64-pc-windows-gnu` ✓ clean. macOS `cargo check -p wiredesk-host` ✓ clean.
- [x] Run `cargo test -p wiredesk-host -- --test-threads=1` (skip ignored) — 113 passed; 0 failed; 2 ignored. Workspace: 539 passing.

### Task 6a: LastSeen / LastKind file slot extension + dedup helpers

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs` (Mac LastSeen)
- Modify: `apps/wiredesk-host/src/clipboard.rs` (Win LastKind)

- [x] **Mac side**:
  - Extend `LastSeen` struct: add `pub file: Option<u64>` + `pub oversize_file: Option<u64>` ✓.
  - Add `LastSeen::matches_file_hash(hash) -> bool` — parallel `matches_image_hash` ✓.
  - Add `ClipboardState::set_file(hash)`, `set_oversize_file(hash)` — parallel image-сетеров ✓. `set_file` mirrors `set_image` and clears matching oversize stamp.
  - Extend `ClipboardState::reset()` — clear file + oversize_file slots ✓ (auto via `*g = LastSeen::default()`; covered by `reset_clears_file_slot` test).
  - Extend `LastKind` test enum (cfg(test)): add `File(u64)`, `OversizeFile(u64)` variants ✓ + `set()` mapping.
- [x] **Win side**:
  - Extend `LastKind` enum: add `File(u64)`, `OversizeFile(u64)` variants ✓.
  - Update `LastKind::matches_image_hash` → renamed/extended `matches_file_hash` (или новый method) ✓ — added new `matches_file_hash` method, kept `matches_image_hash` intact (symmetric pattern).
  - Default behaviour для new variants в любых match'ах (compile-error gate) ✓ — no exhaustive matches on `LastKind` in production code; `matches!` macros and explicit construction sites unaffected.
- [x] Write tests:
  - `lastseen_file_slot_independent_from_image` ✓ (Mac).
  - `lastseen_file_dedup_per_slot` ✓ (Mac).
  - `reset_clears_file_slot` ✓ (Mac).
  - `lastkind_file_oversize_distinct` ✓ (Win → `host_lastkind_file_oversize_distinct`).
  - `lastseen_rapid_text_image_file_text_no_slot_aliasing` ✓ (Mac).
  - Bonus: `set_file_clears_matching_oversize_stamp` (Mac), `lastkind_file_oversize_distinct_test_only` (Mac LastKind test-enum), `host_lastkind_file_dedup_per_slot`, `host_oversize_file_dedup_skips_repoll`, `host_lastkind_text_image_file_slot_independence` (Win).
- [x] Run `cargo test --workspace -- --test-threads=1` — passed: 549 total (+10 net new), 0 failed, 5 ignored. Clippy clean.

### Task 6b: Mac outbound file sync (poll path extension)

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs`

- [x] В poll thread после text/image branches добавить file-branch:
  - Call `clipboard_files::poll_file_url(&mut last_change_count)` ✓ (file_change_count initialised at -1 so the first tick always inspects the pasteboard).
  - If `Some(path)`: stat file, если size > `MAX_FILE_BYTES` → `set_oversize_file(path_hash)` + toast warning через `events_tx.send(TransportEvent::Toast(...))` (no separate `pending_warning` slot — existing TransportEvent::Toast is the channel) ✓.
  - Иначе: read file content (full bytes), hash content (DefaultHasher через `hash_bytes`) ✓.
  - Dedup vs `LastSeen.file` и `LastSeen.oversize_file` — skip emit если match (через `matches_file_hash`) ✓.
  - `pack_first_chunk(basename(path), content)` → `emit_offer_and_chunks(FORMAT_FILE, packed)` ✓.
  - `set_file(content_hash)` ✓.
  - **Refactor**: image branch wrapped in labeled `'image:` block so its early-exits fall through to the file branch (the OS clipboard can carry text + image + file URL from one Cmd+C — we don't want a stale image to suppress file sync). The previous `continue` exits in the image branch were converted to `break 'image`.
- [x] Add pure helper `pack_file_or_warn(path: &Path, limit: usize) -> FilePollOutcome` для тестируемости offline ✓. Returns `Ready { name, hash, packed } | Oversize { path_hash, err } | Skipped(reason)` so the caller (production or test) can do its own state mutations.
- [x] Add `format_oversize_file_toast(&FileTooLarge) -> String` — parallel `format_oversize_toast` ✓. Also added `check_file_size(size_bytes, limit) -> Result<(), FileTooLarge>` mirror of `check_image_size`.
- [x] Write tests (10 new):
  - `mac_outbound_dedup_skips_same_file_hash` — content hash stamp → `matches_file_hash` short-circuits next tick (brief T5) ✓.
  - `mac_outbound_emits_offer_and_chunks_for_file` — synthesize 4 KB content → packed → offer format=FORMAT_FILE, chunks reassemble to packed payload ✓.
  - `mac_outbound_oversize_emits_toast_only` — content > limit → no packets emitted, TransportEvent::Toast on events_tx, oversize_file slot stamped ✓.
  - `mac_outbound_oversize_path_hash_cached` — second poll of same oversize path → `matches_file_hash(path_hash)` short-circuits before toast re-emission ✓.
  - Plus bonus pure tests: `check_file_size_within_limit`, `check_file_size_over_limit_reports_bytes`, `format_oversize_file_toast_includes_kb_and_hint`, `pack_file_or_warn_ready_for_normal_file`, `pack_file_or_warn_oversize_emits_path_hash_and_err`, `pack_file_or_warn_missing_file_skipped` ✓.
- [x] Run `cargo test --workspace -- --test-threads=1` — 559 passed (198 client + 117 host + 14 exec-core + 84 protocol + 83 transport-other + 22 term + 41 transport), 0 failed, 5 ignored. Workspace clippy clean. ✓.

### Task 6c: Win outbound file sync (poll path extension)

**Files:**
- Modify: `apps/wiredesk-host/src/clipboard.rs`

- [x] В `ClipboardSync::poll` после text/image branches добавить file branch — same shape как Mac (Task 6b). Image branch теперь обёрнут в labeled `'image:` block так что его early-exits fall through к file branch (OS clipboard может нести CF_HDROP вместе с CF_DIB; stale image dedup не должен подавлять fresh file sync). Mirror of the Mac 6b refactor.
- [x] Add `pack_file_or_warn` mirror в host clipboard module — реализован inline в `apps/wiredesk-host/src/clipboard.rs` (симметричен Mac side; duplication intentional per CLAUDE.md, протокольный crate остаётся wire-format only).
- [x] Bonus: добавлены `check_file_size`, `format_oversize_file_toast`, `FileTooLarge`, `FilePollOutcome` — pure helpers с подписями зеркальными Mac side, переиспользованы в тестах для unit-coverage без живого clipboard backend'а. Сняты `#[allow(dead_code)]` с `LastKind::File`/`OversizeFile` и `matches_file_hash` — теперь production users.
- [x] Oversize warning идёт через существующий `pending_warning` slot (host UI не имеет TransportEvent::Toast — `take_warning()` → tray balloon). Wording консистентно с Mac toast.
- [x] Write tests (10 new):
  - `host_check_file_size_within_limit` / `..._over_limit_reports_bytes` — boundary semantic.
  - `host_format_oversize_file_toast_includes_kb_and_limit` — wording assertions (KB, "smaller", "limit", "too large").
  - `host_pack_file_or_warn_ready_for_normal_file` — packed layout `[u16 LE name_len][name][content]` byte-equal roundtrip.
  - `host_pack_file_or_warn_oversize_emits_path_hash_and_err` — short-circuits on stat without reading content.
  - `host_pack_file_or_warn_missing_file_skipped` — non-existent path → Skipped.
  - `host_outbound_dedup_skips_same_file_hash` — brief T5 mirror (Win side).
  - `host_outbound_emits_offer_and_chunks_for_file` — offer shape (FORMAT_FILE, total_len=packed.len()) + chunks reassemble byte-for-byte.
  - `host_outbound_oversize_emits_warning_only` — `pending_warning` populated, `pending_outbox` empty, `OversizeFile(path_hash)` stamped.
  - `host_outbound_oversize_path_hash_cached` — repeated oversize path hits dedup short-circuit.
- [x] Run `cargo test --workspace -- --test-threads=1` — 569 passed (198 client + 14 exec-core + 84 protocol + 127 host + 83 transport-other + 22 term + 41 transport), 0 failed, 5 ignored. +10 net new tests. Clippy clean on macOS target (Windows-target pre-existing warnings unaffected, vetted via git stash baseline). Cross-compile `cargo check --target x86_64-pc-windows-gnu` ✓ clean.

### Task 7a: receive_files Arc<AtomicBool> threading + flag-off ClipDecline path

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs` (IncomingClipboard ctor)
- Modify: `apps/wiredesk-host/src/clipboard.rs` (ClipboardSync ctor)
- Modify: `apps/wiredesk-client/src/main.rs` (wire pass-through)
- Modify: `apps/wiredesk-host/src/main.rs`/`session.rs` (wire pass-through)

- [x] Add `receive_files: Arc<AtomicBool>` field в `IncomingClipboard` (Mac) — пройти через constructor signature ✓.
- [x] Same для Win `ClipboardSync` ctor (или session-level state) ✓ — добавил `with_counters_and_toggles(counters, receive_files)` ctor, default-`true` shim в существующем `with_counters` для backward compat. Поле `receive_files: Arc<AtomicBool>` на самом `ClipboardSync` (session-level — proxy уровня Session не требуется).
- [x] Wire через `reset_session_state` / IPC handlers — параллельно с `receive_text`/`receive_images` paths ✓ (Mac main.rs: добавил `receive_files = Arc::new(AtomicBool::new(true))` рядом с receive_text/receive_images; reader_thread сигнатура расширена; reset_session_state живёт на client side и Arc проходит без дополнительной работы т.к. clipboard_state.reset() уже сбрасывает все). На Win — sole consumer уже владеет Arc'ом, нет отдельной reset-логики.
- [x] Extend `on_offer(format == FORMAT_FILE)` — if flag off → emit `Message::ClipDecline { format: FORMAT_FILE }`, не arm reassembly. Mirror на обеих сторонах ✓. На Win `on_offer` signature изменена `-> Option<Message>` (раньше `()`); session.rs forwards through `self.send()`. Mac return type не менялся (уже было `Option<Message>`).
- [x] Write tests ✓:
  - `mac_incoming_file_declined_when_flag_off` — receive_files = false → on_offer FORMAT_FILE → ClipDecline emitted, no reassembly state, чанки в следующих on_chunk дропаются ✓.
  - `mac_incoming_file_accepted_when_flag_on` — receive_files = true → on_offer FORMAT_FILE → expected_format set, ready for chunks ✓.
  - Win mirrors ✓ (`host_incoming_file_declined_when_flag_off`, `host_incoming_file_accepted_when_flag_on`).
  - Bonus: `mac_incoming_file_oversize_offer_dropped_no_decline`, `host_incoming_file_oversize_offer_dropped_no_decline` (cap > MAX_FILE_BYTES + MAX_FILENAME_LEN + 2 → silent drop, не ClipDecline — last is reserved for policy refusals), `host_incoming_text_image_unaffected_by_receive_files_flag` (regression — flag off не ломает text/image приём).
- [x] Run `cargo test --workspace -- --test-threads=1` — 576 passed (201 client + 14 exec-core + 84 protocol + 131 host + 83 transport-other + 22 term + 41 transport), 0 failed, 5 ignored ✓. +7 net new tests vs 569 baseline. `cargo clippy --workspace --all-targets -- -D warnings` clean. Windows cross-compile `cargo check --target x86_64-pc-windows-gnu` clean.

### Task 7b: Mac inbound file commit (unpack + sanitize + write)

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs`

- [x] Extend `IncomingClipboard::on_offer` для file size cap check: `total_len_usize > MAX_FILE_BYTES + MAX_FILENAME_LEN + 2` (header overhead) → silent drop (без ClipDecline — последний reserved for policy refusals, не для broken peers). State reset, чанки потом отбрасываются `expected_len==0` guard'ом в `on_chunk`.
- [x] Extend `IncomingClipboard::commit` — branch on `expected_format == FORMAT_FILE`:
  - `unpack_first_chunk(payload)` → `(name, content)`.
  - `sanitize_basename(name)` → final basename.
  - `dirs::cache_dir().join("WireDesk").join(basename)` — `fs::create_dir_all` если нет.
  - `fs::write(path, content)` — на IO error: log::warn + `remove_file` partial + clear in_flight slot + early return.
  - Call `clipboard_files::set_file_url(&path)` — на FFI error: log::warn (file всё равно в cache, user может вручную найти).
  - `state.set_file(content_hash)` — hash content только что reassembled.
- [x] Cleanup partial-file on reset/abort: track in-flight write path в `IncomingClipboard.in_flight_file_path`; `reset()` → `fs::remove_file(path).ok()` с `NotFound` swallow. Stamp ДО write (panic/abort mid-write оставит breadcrumb); clear ПОСЛЕ successful write+set_file_url.
- [x] Write tests (pure where possible — inject cache_dir):
  - `mac_incoming_file_commits_to_cache` ✓ — feed offer+chunks → commit → tempdir contains expected file + LastSeen.file stamped (brief T5).
  - `mac_incoming_file_sanitizes_traversal` ✓ — name `"../evil.sh"` → file written внутри cache_dir, не outside (brief T4 + AC6).
  - `mac_incoming_file_unicode_filename` ✓ — `"привет 🎉.pdf"` → preserved (brief T3 + AC5).
  - `mac_incoming_file_oversize_declined` ✓ — total_len > cap → silent drop в on_offer + chunks discarded + cache dir empty (AC4).
  - `mac_incoming_partial_file_cleaned_on_reset` ✓ — pre-populate partial + stamp in_flight → reset() removes.
  - `text_and_image_commit_still_work` ✓ — regression (AC3): text + image paths не сломались.
  - Bonus: `mac_incoming_partial_file_missing_no_panic_on_reset`, `mac_incoming_file_unpack_failure_leaves_state_clean`, `mac_incoming_file_reserved_ntfs_name_prefixed`, `mac_incoming_file_empty_name_falls_back_to_clipboard_bin` ✓.
- [x] Run `cargo test --workspace -- --test-threads=1` — 586 passed (214 client + 14 exec-core + 84 protocol + 133 host + 83 transport-other + 22 term + 41 transport), 0 failed, 5 ignored. +10 net new tests vs 576 baseline. `cargo clippy --workspace --all-targets -- -D warnings` clean.

### Task 7c: Win inbound file commit (mirror of 7b)

**Files:**
- Modify: `apps/wiredesk-host/src/clipboard.rs`

- [x] Mirror Mac inbound (Task 7b) на Win-side:
  - Write to `%TEMP%\WireDesk\<basename>` (с fallback на `dirs::cache_dir()` и `std::env::temp_dir()` для misconfigured environments) ✓.
  - `clipboard_files::set_cf_hdrop(&path)` под `#[cfg(windows)]`; на non-Windows builds skip + debug log (FFI stub returns `ClipboardLocked`, не нужно туда стучать) ✓.
  - Stamp `LastKind::File(content_hash)` ✓ — hash от content (не от name), параллельно outbound branch, чтобы copy-rename-paste не зацикливался.
  - Partial-file cleanup on reset ✓ — `in_flight_file_path` slot stamped ДО write (panic/abort breadcrumb), очищается ПОСЛЕ successful write+set_cf_hdrop; `reset()` swallow'ит `NotFound`.
  - `cache_dir_override: Option<PathBuf>` + `#[cfg(test)] fn set_cache_dir_override` — tempdir injection для тестов (зеркало Mac 7b).
  - `CommittedPayload::File { path, name, content }` test-only variant — введён test introspection.
- [x] Write tests (11 new, Mac 7b mirror):
  - `host_incoming_file_commits_to_cache` ✓ — feed offer+chunks → commit → tempdir contains expected file + `LastKind::File(content_hash)` stamped (brief T5 Win mirror).
  - `host_incoming_file_sanitizes_traversal` ✓ — name `"../evil.exe"` → file written внутри cache_dir, не outside (brief T4 + AC6).
  - `host_incoming_file_unicode_filename` ✓ — `"привет 🎉.pdf"` → preserved byte-equal на disk (brief T3 + AC5).
  - `host_incoming_file_oversize_declined` ✓ — total_len > cap → silent drop в on_offer + chunks discarded + cache dir пустая (AC4).
  - `host_incoming_partial_file_cleaned_on_reset` ✓ — pre-populated partial + stamped `in_flight_file_path` → reset() removes.
  - `host_incoming_partial_file_missing_no_panic_on_reset` ✓ — `NotFound` swallowed (vacuum tick / AV quarantine race).
  - `host_text_and_image_commit_still_work` ✓ — regression (AC3): text + image paths не сломались после расширения commit() и reset() lifecycle.
  - `host_incoming_file_unpack_failure_leaves_state_clean` ✓ — bogus payload (`name_len=99` but only 4 bytes) → drop + ready for next offer.
  - `host_incoming_file_reserved_ntfs_name_prefixed` ✓ — `"CON.txt"` → `"_CON.txt"` (особенно важно для Win-host'а — raw `CON.txt` открыл бы console device).
  - `host_incoming_file_empty_name_falls_back_to_clipboard_bin` ✓ — `".."` → `"clipboard.bin"` fallback.
  - `host_resolve_cache_dir_honours_override` ✓ — sanity check для test-injection slot.
- [x] Run `cargo test --workspace -- --test-threads=1` — 597 passed (211 client + 14 exec-core + 84 protocol + 142 host + 83 transport-other + 22 term + 41 transport), 0 failed, 5 ignored. +11 net new tests vs 586 baseline. `cargo clippy --workspace --all-targets -- -D warnings` clean. Windows cross-compile `cargo check --target x86_64-pc-windows-gnu` ✓ clean (clippy на этом target имеет 3 pre-existing warnings из `transfer_overlay.rs` и `session.rs`, vetted via baseline stash — не относятся к 7c).

### Task 7d: Progress label + cancel + send-decline toast for FORMAT_FILE

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs` (apply_outgoing_progress)
- Modify: `apps/wiredesk-host/src/clipboard.rs` (parallel host logic)
- Modify: `apps/wiredesk-client/src/app.rs` или ui module (status-line rendering)

- [x] Extend `apply_outgoing_progress` — match на format включает `FORMAT_FILE` → label "file". Log message: `"clipboard.send START format=FILE total={total_len} bytes"` ✓ — реализовано через `format_label(format)` helper в `apply_outgoing_progress_inner`. Лог теперь даёт `format=FILE` вместо numeric `format=2`. Mirror `format_label` для TEXT/IMAGE/UNKNOWN заодно — grep по логам работает по любому слоту.
- [x] Receive-side: `ClipDecline { format: FORMAT_FILE }` обработчик на send-стороне ✓ — `apply_clip_decline(format, &outgoing_cancel) -> String` pure helper флипает `outgoing_cancel` (writer_thread дренит queued ClipOffer/ClipChunk через `is_clip && cancelling` ветку, существовавшую с image-transfers) + возвращает FORMAT_FILE-специфичный toast `"Peer declined file (Receive files off)"`. Host-side зеркало в `session.rs` через `clipboard.push_warning(...)` → tray balloon (host UI не имеет `TransportEvent::Toast`).
- [x] Cancel button — verify existing cancel UI handles file offer-state correctly ✓ — same `Arc<AtomicBool> outgoing_cancel` shared by text/image/file (`writer_thread` matches на `Message::ClipOffer { .. } | Message::ClipChunk { .. }` без discriminate по format). No code change needed; covered by `clip_decline_file_drops_pending_outbox` test, который проверяет что flag arms на FORMAT_FILE decline.
- [x] Status-line формат: `"Sending file 'X.pdf' — N/M bytes (P%)"` ✓ — `current_outgoing_label: Arc<Mutex<String>>` stash slot, set'ит poll thread перед `emit_offer_and_chunks(FORMAT_FILE, ...)`, clear'ит `apply_outgoing_progress_with_label` на DONE (или Disconnected event в `app.rs`). `outgoing_action_label(label)` pure helper строит "Sending file 'X'" если slot непустой, иначе legacy "Sending clipboard". Wired в обе render-точки (CentralPanel chrome + capture overlay).
- [x] Write tests ✓ — все 4 мандатных:
  - `apply_outgoing_progress_handles_file_format` (clipboard.rs) — `format_label(FORMAT_FILE) == "FILE"` + label slot сохраняется через Offer.
  - `clip_decline_file_drops_pending_outbox` (clipboard.rs) — `apply_clip_decline(FORMAT_FILE, &cancel)` → `cancel == true`; writer's `is_clip && cancelling` drain branch уже unit-tested через cancel button suite.
  - `clip_decline_file_emits_toast` (clipboard.rs) — `apply_clip_decline(FORMAT_FILE, ...) == "Peer declined file (Receive files off)"` + non-file regression (`FORMAT_TEXT_UTF8` keeps legacy generic).
  - `status_line_renders_filename` (app.rs) — `outgoing_action_label("contract.pdf") == "Sending file 'contract.pdf'"` + `format_progress` round-trip с % + KB.
  - Bonus: `apply_outgoing_progress_file_clears_label_on_done` (slot cleanup), `outgoing_action_label_empty_falls_back_to_generic` (back-compat), `send_decline_toast_file_format_is_specific` (toast wording isolation).
- [x] Run `cargo test --workspace -- --test-threads=1` ✓ — 604 passed (212 client + 14 exec-core + 84 protocol + 144 host + 83 transport-other + 22 term + 41 transport + 4 wiredesk-core), 0 failed, 5 ignored. +7 net new tests vs 597 baseline. `cargo clippy --workspace --all-targets -- -D warnings` clean. Windows cross-compile `cargo check -p wiredesk-host --target x86_64-pc-windows-gnu` clean.

### Task 8: Settings UI — "Receive files" checkbox both sides

**Files:**
- Modify: `apps/wiredesk-client/src/app.rs`
- Modify: `apps/wiredesk-client/src/config.rs` (если отдельный)
- Modify: `apps/wiredesk-client/src/main.rs` (wire Arc<AtomicBool> в IncomingClipboard, уже подготовлен в 7a)
- Modify: `apps/wiredesk-host/src/settings_ui.rs`
- Modify: `apps/wiredesk-host/src/config.rs` (если отдельный)

- [ ] **Config (TOML)**:
  - Mac: add `receive_files: bool` default `true` в Config struct с `#[serde(default = "default_true")]`.
  - Win: same.
  - Verify TOML serialize/deserialize round trip + default values работают (back-compat: existing config files без поля → true).
- [ ] **Mac UI** (`app.rs`):
  - Add `receive_files: Arc<AtomicBool>` field в `WireDeskApp`.
  - Add `Config.receive_files` field.
  - В Settings panel рядом с "Receive images (Host → Mac)" добавить checkbox "Receive files (Host → Mac)".
  - Wire через `store(...)` параллельно `receive_images` (рядом строки 643-667 в app.rs).
- [ ] **Win UI** (`settings_ui.rs`):
  - Add checkbox "Receive files" в nwg layout. Group: Clipboard (или where receive_images уже стоит).
  - Wire Arc<AtomicBool> + write back to Config on Save.
- [ ] Write tests:
  - `config_roundtrip_with_receive_files` — TOML serialize + deserialize, default = true.
  - `config_back_compat_missing_receive_files` — TOML без поля → default true (используя `#[serde(default = ...)]`).
- [ ] Run `cargo test --workspace -- --test-threads=1` — all pass before Task 9a.

### Task 9a: Cache vacuum startup hookup (both sides)

**Files:**
- Modify: `apps/wiredesk-client/src/main.rs`
- Modify: `apps/wiredesk-host/src/main.rs`

- [ ] **Mac startup** (`main.rs`): early в `main()` call `wiredesk_core::cache_vacuum::vacuum_cache_dir(cache_path, Duration::from_secs(24 * 3600))`.
  - `cache_path = dirs::cache_dir()?.join("WireDesk")`.
  - Log: `info: "cache vacuum removed N files"`.
  - Errors → log::warn, не блокировать startup.
- [ ] **Win startup**: same with `env::var("TEMP")?.join("WireDesk")`.
- [ ] Write tests:
  - `cache_vacuum_startup_handles_missing_dir` — non-existent → no panic, log warning.
  - Integration-style (если можно): создать old file в tempdir, run vacuum helper, assert removed.
- [ ] Run `cargo test --workspace -- --test-threads=1` — must pass before Task 9b.

### Task 9b: stamp_initial extension for file slot (both sides)

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs`
- Modify: `apps/wiredesk-host/src/clipboard.rs`

- [ ] **Mac**: при init `ClipboardState` (или в poll thread first tick): если NSPasteboard содержит fileURL — hash content + `state.set_file(hash)`. Skip stamping (log warning) для файлов > MAX_FILE_BYTES — let user manually re-copy.
- [ ] **Win**: extend `stamp_initial` в `clipboard.rs` для CF_HDROP detection + content hash. Same > cap skip.
- [ ] Write tests:
  - `stamp_initial_handles_pre_existing_file` — pre-stamped file hash не emits offer на first poll tick.
  - `stamp_initial_skips_oversize_file` — file > cap → no hash, log warning.
- [ ] Run `cargo test --workspace -- --test-threads=1` + `cargo clippy --workspace --all-targets -- -D warnings` — both pass before Task 10.

### Task 10: Verify acceptance criteria + full test suite

**Files:**
- No new code; live verification

- [ ] Run `cargo test --workspace -- --test-threads=1` — full suite green (was 491 → expect ~520+ with new tests).
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- [ ] Build release: `./scripts/build-mac-app.sh` (Mac) + cross-check Win build compiles.
- [ ] **Live AC1** (brief AC1): Cmd+C `contract.pdf` (5 MB) in Mac Finder → ≤ 30 сек на 3 Mbaud → Cmd+V in Win Explorer → file landed with same sha256 + filename.
- [ ] **Live AC2** (brief AC2): Win Explorer Cmd+C → Mac Finder Cmd+V → same content + filename.
- [ ] **Live AC3** (brief AC3): text+image roundtrip продолжает работать (regression).
- [ ] **Live AC4** (brief AC4): 25 MB file → toast "File too large", nothing sent.
- [ ] **Live AC5** (brief AC5): `"привет 🎉.pdf"` → preserved both directions.
- [ ] **Live AC6** (brief AC6): filename с `../../evil` → sanitized на receive, file landed только в cache dir.
- [ ] **Live AC7** (brief AC7): paste файла обратно в течение 10 сек → no round-trip (LastSeen dedup).
- [ ] **Live AC8** (brief AC8): cancel в progress-bar mid-transfer → ничего не висит на receive-стороне.
- [ ] **Live AC9** (brief AC9): receive_files = false → toast "Receive files off" на send-стороне.
- [ ] **Live AC10** (brief AC10): clippy/tests clean (verified выше).
- [ ] If any AC fails: add failure to plan as ⚠️ + fix + retest.

### Task 11: Update documentation + finalize

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md` (известное ограничение про файлы убрать)
- Move: `docs/plans/20260528-clipboard-files.md` → `docs/plans/completed/`

- [ ] Update `CLAUDE.md`:
  - Strip line "Файлы (file URLs / CF_HDROP) — не передаются" из секции "Известные ограничения".
  - Add line про clipboard files в основное описание features.
  - Update test counts (~520+).
- [ ] Update `README.md`:
  - Если есть аналогичная строка про known limitation — strip.
  - Если упоминаются supported clipboard formats — добавить files.
- [ ] Move plan: `mkdir -p docs/plans/completed && mv docs/plans/20260528-clipboard-files.md docs/plans/completed/`.
- [ ] Final `cargo test --workspace -- --test-threads=1` + `cargo clippy` green.

## Post-Completion

*Live hardware AC verification on the real FT232H link (covered in Task 10), plus follow-up considerations once the feature ships:*

- **Live FT232H @ 3 Mbaud test required** — все AC1-AC10 на реальном hardware setup'е (Win11 host + Mac M4 client + two CJMCU-FT232H breakouts + null-modem cable). Smoke unit tests covered, но end-to-end clipboard flow требует физического hardware.
- **Manual verification scenarios**:
  - Large filename with weird chars (`"file with [spaces] (1).tar.gz"`).
  - Read-only / locked source files (handle gracefully — toast warning).
  - Files in iCloud Drive / OneDrive (lazy-download semantics — file may not be on disk).
  - NTFS reserved names round trip (`CON.txt` → `_CON.txt` on receive).
- **Memory update**: после shipping — update `project_clipboard_files.md` memory с MERGED-marker + actual measurements.
- **Phase 2 follow-up briefs** (out of scope here):
  - Multi-file selection (`docs/briefs/clipboard-files-multi.md`).
  - Directories через zip on-fly (`docs/briefs/clipboard-files-dirs.md`).
  - Cap > 20 MB (если ad-hoc cases вылезут).
- **No deploy/external systems**: WireDesk — local desktop binary, нет CI/CD pipeline'а / consuming projects.
