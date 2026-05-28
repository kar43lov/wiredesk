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

- [ ] В poll thread после text/image branches добавить file-branch:
  - Call `clipboard_files::poll_file_url(&mut last_change_count)`.
  - If `Some(path)`: stat file, если size > `MAX_FILE_BYTES` → `set_oversize_file(hash_path)` + toast warning через `pending_warning` slot.
  - Иначе: read file content (full bytes), hash content (DefaultHasher).
  - Dedup vs `LastSeen.file` и `LastSeen.oversize_file` — skip emit если match.
  - `pack_first_chunk(basename(path), content)` → `emit_offer_and_chunks(FORMAT_FILE, packed)`.
  - `set_file(content_hash)`.
- [ ] Add pure helper `pack_file_or_warn(path: &Path, max: usize) -> Result<Vec<u8>, FileTooLarge>` для тестируемости offline.
- [ ] Add `format_oversize_file_toast(size_bytes: usize) -> String` — parallel `format_oversize_toast`.
- [ ] Write tests:
  - `mac_outbound_dedup_skips_same_file_hash` — state set, helper called с same content → no emit (brief T5).
  - `mac_outbound_emits_offer_and_chunks_for_file` — synthesize 4KB fake content + name → offer format=FORMAT_FILE + correct chunk count + first chunk contains packed name.
  - `mac_outbound_oversize_emits_toast_only` — content > MAX_FILE_BYTES → no offer, warning slot populated.
  - `mac_outbound_oversize_path_hash_cached` — second poll того же oversize file не повторяет toast.
- [ ] Run `cargo test --workspace -- --test-threads=1` — must pass before Task 6c.

### Task 6c: Win outbound file sync (poll path extension)

**Files:**
- Modify: `apps/wiredesk-host/src/clipboard.rs`

- [ ] В `ClipboardSync::poll` после text/image branches добавить file branch — same shape как Mac (Task 6b).
- [ ] Add `pack_file_or_warn` mirror в host clipboard module (или reuse pure helper из protocol/core если разместили там).
- [ ] Write tests:
  - `win_outbound_dedup_skips_same_file_hash` (brief T5 mirror).
  - `win_outbound_emits_offer_and_chunks_for_file`.
  - `win_outbound_oversize_emits_toast_only`.
- [ ] Run `cargo test --workspace -- --test-threads=1` — must pass before Task 7a.

### Task 7a: receive_files Arc<AtomicBool> threading + flag-off ClipDecline path

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs` (IncomingClipboard ctor)
- Modify: `apps/wiredesk-host/src/clipboard.rs` (ClipboardSync ctor)
- Modify: `apps/wiredesk-client/src/main.rs` (wire pass-through)
- Modify: `apps/wiredesk-host/src/main.rs`/`session.rs` (wire pass-through)

- [ ] Add `receive_files: Arc<AtomicBool>` field в `IncomingClipboard` (Mac) — пройти через constructor signature.
- [ ] Same для Win `ClipboardSync` ctor (или session-level state).
- [ ] Wire через `reset_session_state` / IPC handlers — параллельно с `receive_text`/`receive_images` paths.
- [ ] Extend `on_offer(format == FORMAT_FILE)` — if flag off → emit `Message::ClipDecline { format: FORMAT_FILE }`, не arm reassembly. Mirror на обеих сторонах.
- [ ] Write tests:
  - `mac_incoming_file_declined_when_flag_off` — receive_files = false → on_offer FORMAT_FILE → ClipDecline emitted, no reassembly state.
  - `mac_incoming_file_accepted_when_flag_on` — receive_files = true → on_offer FORMAT_FILE → expected_format set, ready for chunks.
  - Win mirrors.
- [ ] Run `cargo test --workspace -- --test-threads=1` — must pass before Task 7b.

### Task 7b: Mac inbound file commit (unpack + sanitize + write)

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs`

- [ ] Extend `IncomingClipboard::on_offer` для file size cap check: `total_len_usize > MAX_FILE_BYTES + MAX_FILENAME_LEN + 2` (header overhead) → ClipDecline.
- [ ] Extend `IncomingClipboard::commit` — branch on `expected_format == FORMAT_FILE`:
  - `unpack_first_chunk(payload)` → `(name, content)`.
  - `sanitize_basename(name)` → final basename.
  - `dirs::cache_dir().join("WireDesk").join(basename)` — `fs::create_dir_all` если нет.
  - `fs::write(path, content)` — на IO error: log::warn + reset reassembly + early return.
  - Call `clipboard_files::set_file_url(&path)` — на FFI error: log::warn (file всё равно в cache, user может вручную найти).
  - `state.set_file(content_hash)` — hash content только что reassembled.
- [ ] Cleanup partial-file on reset/abort: track in-flight write path в IncomingClipboard state; reset() → `fs::remove_file(path).ok()`.
- [ ] Write tests (pure where possible — inject cache_dir):
  - `mac_incoming_file_commits_to_cache` — feed offer+chunks → commit → tempdir contains expected file (brief T5).
  - `mac_incoming_file_sanitizes_traversal` — name `"../evil.sh"` → file written внутри cache_dir, not outside (brief T4 + AC6).
  - `mac_incoming_file_unicode_filename` — `"привет 🎉.pdf"` → preserved (brief T3 + AC5).
  - `mac_incoming_file_oversize_declined` — total_len > cap → declined + state reset (AC4).
  - `mac_incoming_partial_file_cleaned_on_reset` — start file write, reset → partial removed.
  - Regression: `text_and_image_commit_still_work` — text+image paths не сломались.
- [ ] Run `cargo test --workspace -- --test-threads=1` — must pass before Task 7c.

### Task 7c: Win inbound file commit (mirror of 7b)

**Files:**
- Modify: `apps/wiredesk-host/src/clipboard.rs`

- [ ] Mirror Mac inbound (Task 7b) на Win-side:
  - Write to `env::var("TEMP")/WireDesk/<basename>` (или `dirs::cache_dir()`).
  - `clipboard_files::set_cf_hdrop(&path)`.
  - Stamp `LastKind::File(content_hash)`.
  - Partial-file cleanup on reset.
- [ ] Write tests — Mac tests mirror (brief T3/T4/T5 Win-side coverage).
- [ ] Run `cargo test --workspace -- --test-threads=1` — must pass before Task 7d.

### Task 7d: Progress label + cancel + send-decline toast for FORMAT_FILE

**Files:**
- Modify: `apps/wiredesk-client/src/clipboard.rs` (apply_outgoing_progress)
- Modify: `apps/wiredesk-host/src/clipboard.rs` (parallel host logic)
- Modify: `apps/wiredesk-client/src/app.rs` или ui module (status-line rendering)

- [ ] Extend `apply_outgoing_progress` — match на format включает `FORMAT_FILE` → label "file". Log message: `"clipboard.send START format=FILE total={total_len} bytes"`.
- [ ] Receive-side: `ClipDecline { format: FORMAT_FILE }` обработчик на send-стороне — drop pending outbox + emit toast "Peer declined file (Receive files off)".
- [ ] Cancel button — verify existing cancel UI handles file offer-state correctly (most likely shares state with image transfers; explicit assert если так).
- [ ] Status-line формат: `"Sending file 'X.pdf' — N/M bytes (P%)"` — extend UI render branch на format=FILE с filename из в-flight transfer state (need to capture filename when emit_offer fires — add to outgoing-state).
- [ ] Write tests:
  - `apply_outgoing_progress_handles_file_format` — emit ClipOffer{format=FORMAT_FILE} → outgoing_total=N, log message содержит "FILE".
  - `clip_decline_file_drops_pending_outbox` — outbox primed, ClipDecline { FORMAT_FILE } received → outbox drained.
  - `clip_decline_file_emits_toast` — send-side decline → toast slot populated with "declined" message.
  - `status_line_renders_filename` — pure helper testing the formatter for file label.
- [ ] Run `cargo test --workspace -- --test-threads=1` — must pass before Task 8.

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
