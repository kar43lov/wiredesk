# Бриф: UI redesign + 3 фичи (issue #5)

## Цель

Превратить Settings UI обеих сторон WireDesk из «утилитарно-разработческого» в native-looking, добавить три недостающие фичи (auto-detect CH340, Save & Restart, multi-monitor fullscreen) — всё одним PR `feat/ui-redesign`.

## Выбранный подход

**Refactor in-place:** остаёмся на nwg для Windows host и egui для macOS client. Полируем существующий UI стандартными средствами этих библиотек (Frame, ImageFrame, Font в nwg; RichText, Frame::group, CollapsingHeader в egui). Не переписываем стек.

Почему: nwg-launcher только что прошёл AC1-AC10 на железе. Tray, single-instance, autostart, embed-manifest — всё на native Win32, на egui это пришлось бы перевозить с риском регрессов. UX-проблемы решаются стандартными nwg-средствами.

## Требования

См. issue #5 — полный список 8 Critical + 12 Improve + 7 Nice-to-have UX-пунктов + 3 фичи. Краткая таблица:

| Группа | Win | Mac |
|--------|-----|-----|
| Typography | Segoe UI global default | `impl Display for ConnectionState`, RichText status glyph |
| Window icons | embed .ico в PE-headers | W-литера в heading |
| Status | ImageFrame slot + cycling bitmap | Большой glyph + явный текст с причиной |
| Layout | 3 `Frame` блока | `Frame::group()` или CollapsingHeader |
| Buttons | button-bar внизу справа, default=Save | Capture как primary крупная цветная |
| Capture | — | red banner сверху в capture-mode |
| Permission | — | numbered groups, кнопка на шаге 1 |
| Feature 1 | auto-detect CH340 (VID 0x1A86) | — |
| Feature 2 | Save & Restart | — |
| Feature 3 | — | monitor selection через NSScreen FFI |

## Acceptance criteria

1. Все 8 Critical UX-пунктов закрыты (4 Win + 4 Mac)
2. ≥80% Improve UX-пунктов закрыты, остаток → follow-up issue
3. Auto-detect: CH340 подключён → Detect → port подставлен; нет CH340 → message; несколько → список и просьба выбрать
4. Save & Restart: меняю port → жму → новый процесс работает с новыми настройками
5. Monitor selection (3 монитора Mac): выбираю Right → Cmd+Enter → fullscreen на правом → Cmd+Enter → возврат на исходный
6. Регресс: clipboard, Cmd+Space, Cmd+C/V — без изменений
7. AC1-AC10 (launcher-ui live-test) — все ещё проходят
8. Скриншоты до/после в обеих платформах в PR
9. cargo test --workspace, cargo clippy --workspace --all-targets -- -D warnings — clean
10. cross-check `cargo check --target x86_64-pc-windows-gnu -p wiredesk-host` — clean

## Тестирование

- **Pure-helper unit-тесты** (новые):
  - `detect_ch340_port(&[SerialPortInfo]) -> DetectResult` — NotFound/Found/Multiple, моки SerialPortInfo
  - `Display for ConnectionState` — табличка строк
  - NSScreen index validation — fallback при невалидном индексе
- **Existing UI tests:** make_app() и should_show_chrome() гарды — продолжают проходить
- **Manual / live-test:** только в конце PR, после коммита 9. Скриншоты для PR — обе платформы, до/после.
- **Live-test gate:** перед merge'ем в master, на реальной паре машин с CH340-кабелем.

## Риски

1. **`nwg::Frame` без header-label.** Если оказывается просто container без рамки и заголовка — fallback на `GroupBox` если есть, или panel + separator + Label сверху. Не блокирует scope, лишь визуальная вариация.
2. **embed-resource на mingw cross-compile.** `embed_resource::compile()` требует windres который на macOS отсутствует. Mitigation: cargo:rerun-if-changed условный, или `embed-manifest`-style pure-Rust альтернатива. Worst case — иконки прокидываются через `nwg::Window::builder().icon(...)` без PE-resource.
3. **macOS Spaces поведение при OuterPosition + Fullscreen.** Порядок критичен: сначала переместить, дождаться repaint, потом fullscreen. Может потребоваться `request_repaint` между командами.
4. **Save & Restart race condition.** Новый процесс получает `Already running` если успевает acquire mutex до exit'а старого. Mitigation: `thread::sleep(Duration::from_millis(200))` перед `stop_thread_dispatch`, или передача `--wait-for-pid` новому процессу.
5. **Огромный PR** — пользователь явно выбрал «всё в один PR» и «live-test только в конце». Риск что в commit 6 что-то сломается и обнаружится на этапе live-test через 50 файлов изменений. Mitigation: после каждого коммита `cargo test --workspace` + `cargo clippy` + `cargo check --target x86_64-pc-windows-gnu` на полу-автомате, и `cargo build --release` обоих бинарей. Live-тест на железе — только в конце.

## Стратегия коммитов (9 штук)

```
commit 1: chore(ui): typography pass — Segoe UI on Win + Display for ConnectionState on Mac
commit 2: feat(ui): window icons — embed .ico in Win PE-headers + W asset in Mac heading
commit 3: feat(ui): unified status indicators — ImageFrame on Win, RichText+text on Mac
commit 4: refactor(ui): grouped settings layout — Frame blocks on Win, group()/CollapsingHeader on Mac
commit 5: refactor(ui): button-bar conventions — primary right-aligned, default action keyboarded
commit 6: feat(client): capture-mode banner + permission-screen step-by-step
commit 7: feat(host): auto-detect CH340 button (VID 0x1A86 filter)
commit 8: feat(host): Save & Restart button (Command::spawn + stop_thread_dispatch)
commit 9: feat(client): monitor selection for fullscreen (NSScreen FFI)
```

После коммита 9 — live-test gate, потом PR в master. Все коммиты на ветке `feat/ui-redesign`.

## Первые шаги

1. `git checkout -b feat/ui-redesign master`
2. **commit 1 — typography pass.** Quick win с минимальным риском, оба бинаря компилируются и работают сразу.
3. После каждого commit'а — `cargo build` (debug — быстро) + `cargo test -p` затронутых крейтов.
4. После commit 5 — пробный `cargo build --release` обоих бинарей (release медленнее, но проверим что Win-сторона собирается чисто).
5. После commit 9 — live-test на железе перед PR.

## Сложность

**medium-high.** ~9 commits / ~15-20 файлов / ~1000-1500 LOC. 1-2 дня фокусной работы. Большинство — стандартные nwg/egui control'ы; FFI к NSScreen — единственный не-тривиальный момент.

## Где живёт работа

Ветка `feat/ui-redesign`, master стабилен на `532a3df` (LICENSE). PR в master только после live-теста на реальном железе с обоими кабелями + CH340 + (опционально) multi-monitor для AC по выбору монитора.
