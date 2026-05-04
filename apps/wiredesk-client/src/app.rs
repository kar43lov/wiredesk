use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;

use crate::config::ClientConfig;
use crate::input::mapper::InputMapper;
use crate::keyboard_tap::{self, TapEvent, TapHandle};
use crate::monitor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => write!(f, "Not connected"),
            ConnectionState::Connecting => write!(f, "Connecting…"),
            ConnectionState::Connected => write!(f, "Connected"),
        }
    }
}

/// Messages from the transport thread to the UI.
#[allow(dead_code)]
pub enum TransportEvent {
    Connected { host_name: String, screen_w: u16, screen_h: u16 },
    Disconnected(String),
    ClipboardFromHost(String),
    Heartbeat,
    ShellOutput(Vec<u8>),
    ShellExit(i32),
    ShellError(String),
    /// Transient user-facing notification — surfaced as an inline toast in the
    /// chrome panel for ~3 seconds. Used by the clipboard poll thread when an
    /// oversize image is dropped (so the user knows their copy didn't make it
    /// to the peer), but kept generic for any future single-line warning.
    Toast(String),
}

pub struct WireDeskApp {
    state: ConnectionState,
    capturing: bool,
    fullscreen: bool,
    host_name: String,
    screen_w: u16,
    screen_h: u16,
    clipboard_text: String,
    status_msg: String,
    runtime_serial_port: String, // what the running process actually opened
    events_rx: Option<mpsc::Receiver<TransportEvent>>,
    outgoing_tx: mpsc::Sender<Packet>,
    tap_events_rx: Option<mpsc::Receiver<TapEvent>>,
    tap_handle: Option<TapHandle>,
    mapper: InputMapper,
    seq: u16,
    // Permission state
    permission_granted: bool,
    last_perm_check: Instant,
    // Terminal-over-serial state
    shell_open: bool,
    shell_output: String,
    shell_input: String,
    shell_kind: String, // "" (default), "powershell", "cmd"
    // Settings panel state — TOML-backed configuration the user is editing.
    pending_config: ClientConfig,
    config_dirty: bool,
    save_toast: Option<(String, Instant)>,
    // Cached available serial ports for the combo-box; refreshed on demand.
    available_ports: Vec<String>,
    // Cached monitor list for the Settings combo-box. `NSScreen::screens()`
    // is a sync IPC call — refreshing once per second (or on combo focus) is
    // plenty for a UI that's already gated behind an open settings panel,
    // and avoids hammering the call at 60 FPS while the panel just sits open.
    cached_monitors: Vec<monitor::MonitorInfo>,
    cached_monitors_at: Option<Instant>,
    // Sticky banner shown when an entered fullscreen fell back to "current
    // display" because the saved `preferred_monitor` name doesn't match any
    // live display. Carries its own TTL so it doesn't clobber `status_msg`
    // when a real disconnect message wants the same slot.
    monitor_fallback_msg: Option<(String, Instant)>,
    /// Queued `OuterPosition` to restore after fullscreen exit. macOS
    /// Spaces transitions take ~500ms and a position command sent during
    /// the transition lands the window off-screen on a Space that the
    /// user's display no longer shows. Holding the position here and
    /// applying it from `update()` after the timer ensures macOS has
    /// finished the transition before we move the window.
    pending_position_restore: Option<(egui::Pos2, Instant)>,
    /// Last time we re-applied the bundle's Dock icon. winit/eframe's
    /// NSApp init can overwrite the icon ~1s after creator runs, so we
    /// re-apply periodically until macOS settles on our icon. Three or
    /// four passes during the first 10 seconds is enough.
    last_dock_icon_apply: Option<Instant>,
    dock_icon_apply_count: u8,
    // Saved outer position before entering fullscreen on a non-active monitor.
    // Restored on fullscreen exit so the chrome window returns to where the
    // user originally had it. None when fullscreen wasn't entered via an
    // explicit `OuterPosition` move (e.g. preferred_monitor=None or stale
    // name falling back to "fullscreen on current display").
    original_position: Option<egui::Pos2>,
    // Snapshot of `preferred_monitor` taken once at startup. Used by
    // `toggle_fullscreen` so unsaved edits in the Settings panel don't
    // change live runtime behaviour — that contradicts the documented
    // "Save & Restart" contract for port/baud/etc. Stays fixed for the
    // process lifetime; user must restart to pick up a new value.
    runtime_preferred_monitor: Option<String>,
    // Clipboard transfer progress counters. Shared with the clipboard
    // poll thread (outgoing) and the reader thread's IncomingClipboard
    // (incoming). Zero means "no transfer in flight"; non-zero `total`
    // unlocks the "Sending/Receiving image — N/M KB (P%)" status line.
    // Reset to zero on `TransportEvent::Disconnected` so a half-finished
    // transfer doesn't leave the progress display stuck after the link
    // drops.
    outgoing_progress: Arc<AtomicU64>,
    outgoing_total: Arc<AtomicU64>,
    incoming_progress: Arc<AtomicU64>,
    incoming_total: Arc<AtomicU64>,
    /// Runtime image-clipboard toggles (Settings). Both flags share their
    /// `Arc` with the poll thread (`send_images`) and the reader thread's
    /// `IncomingClipboard` (`receive_images`) — toggling here takes effect on
    /// the next poll tick / next incoming offer, no restart required. Text
    /// clipboard sync is unaffected by either flag.
    send_images: Arc<AtomicBool>,
    receive_images: Arc<AtomicBool>,
    send_text: Arc<AtomicBool>,
    receive_text: Arc<AtomicBool>,
    /// Karabiner-Elements `left_command ↔ left_option` compensation. Shared
    /// with the keyboard tap thread; flipping the Settings checkbox takes
    /// effect on the next FlagsChanged / KeyDown the tap sees.
    swap_option_command: Arc<AtomicBool>,
    /// One-shot flag set when the Terminal panel transitions to open.
    /// Consumed by the next render frame to give the shell input field
    /// focus without the user having to click into it. Without this the
    /// user has to click before typing the first command.
    shell_just_opened: bool,
    /// User-pressed Cancel for the outgoing transfer. Shared with the
    /// writer thread, which drops queued ClipOffer/ClipChunk packets while
    /// the flag is set and re-arms it once the stale batch drains.
    outgoing_cancel: Arc<AtomicBool>,
    /// Same shape, for the incoming transfer. The reader thread drops
    /// further chunks of the current offer and resets reassembly state.
    incoming_cancel: Arc<AtomicBool>,
    /// Generic 3-second toast surfaced by `TransportEvent::Toast`. Currently
    /// used by the clipboard poll thread to warn the user when a copied image
    /// exceeds `MAX_IMAGE_BYTES` (Task 7b). Distinct from `save_toast`, which
    /// belongs to the Settings panel — this one is chrome-wide so it shows up
    /// regardless of whether the Settings collapse is open.
    transient_toast: Option<(String, Instant)>,
}

// ---- UI palette / sizing constants ---------------------------------------
// Names for the values that appear in more than one place or carry
// non-obvious meaning. Single-use sizes stay inline at the call site.

/// Warning-orange — used by the permission-restart hint and the per-monitor
/// fullscreen-fallback banner. Same hue both times so the user perceives
/// "warning" as a single visual signal across screens.
const COLOR_WARNING: egui::Color32 = egui::Color32::from_rgb(220, 140, 60);

/// Capture / "release input" red — used as the capture-banner tint and the
/// `Release Input` button fill, so the visual cue follows the user from the
/// chrome panel into the capture/fullscreen banner.
const COLOR_CAPTURE_RED: egui::Color32 = egui::Color32::from_rgb(180, 60, 60);

/// Idle "Capture Input" button blue — paired with `COLOR_CAPTURE_RED` as a
/// state cue (blue idle → red capturing).
const COLOR_CAPTURE_BLUE: egui::Color32 = egui::Color32::from_rgb(60, 110, 180);

/// Capture banner font size — large enough to read across the room while
/// the user is interacting with the Host monitor, not the Mac.
const BANNER_FONT_SIZE: f32 = 20.0;

/// Connection-state glyph (●) size in the chrome status row. Larger than
/// egui's default so it visually matches the Win-side ImageFrame indicator.
const STATUS_GLYPH_SIZE: f32 = 18.0;

/// Heading icon (top-left WireDesk logo) edge length in the chrome panel.
const HEADING_ICON_SIZE: f32 = 28.0;

/// Minimum size of the primary capture toggle button, in egui points.
const CAPTURE_BTN_MIN_SIZE: egui::Vec2 = egui::vec2(200.0, 32.0);

/// Numbered-step glyph size on the Accessibility permission screen.
const STEP_NUMBER_SIZE: f32 = 20.0;

/// Render a clipboard-transfer progress fragment for the chrome status row.
///
/// Returns `None` when there's no active transfer (`total == 0`) so the
/// caller can skip rendering altogether — this is the common case once the
/// poll thread has zeroed counters after a finished send. When a transfer is
/// in flight, returns a string like `"Sending clipboard — 340/780 KB (43%)"` so
/// the user can see that something is moving across the wire.
///
/// `current` is clamped to `total` so a brief race (writer thread atomically
/// adding before total wraps in tests) never produces "1100%".
///
/// Codex iter3 E5: unit selection — sub-KB transfers (short Cmd+C of a few
/// dozen chars) used to render as "0/0 KB" because of integer division. Now
/// we switch to raw bytes when `total < 1024`, and KB otherwise.
pub fn format_progress(action: &str, current: u64, total: u64) -> Option<String> {
    if total == 0 {
        return None;
    }
    let cur = current.min(total);
    let pct = (cur * 100) / total;
    if total < 1024 {
        Some(format!("{action} — {cur}/{total} B ({pct}%)"))
    } else {
        let cur_kb = cur / 1024;
        let tot_kb = total / 1024;
        Some(format!("{action} — {cur_kb}/{tot_kb} KB ({pct}%)"))
    }
}

/// Compute fill ratio for a clipboard progress bar.
///
/// Returns `Some(ratio)` in 0.0..=1.0 when a transfer is active (`total > 0`),
/// or `None` when idle. Overshoot is clamped to 1.0.
/// Render a progress bar with an inline Cancel button on its right.
/// Clicking the button flips `cancel_flag` to true; the writer/reader
/// thread observes the flag and drops the in-flight clipboard packets,
/// then re-arms the flag itself once the stale batch is drained. The
/// caller decides whether the bar should appear at all (None ratio →
/// no row); we only render once we know there's something to show.
fn render_progress_row(
    ui: &mut egui::Ui,
    ratio: f32,
    text: &str,
    cancel_flag: &Arc<AtomicBool>,
    progress_atomic: &Arc<AtomicU64>,
    total_atomic: &Arc<AtomicU64>,
) {
    ui.horizontal(|ui| {
        let bar_width = ui.available_width() - 70.0;
        ui.add_sized(
            [bar_width.max(120.0), 18.0],
            egui::ProgressBar::new(ratio).text(text),
        );
        if ui
            .small_button("✕ Cancel")
            .on_hover_text("Abort this transfer")
            .clicked()
        {
            cancel_flag.store(true, Ordering::Release);
            progress_atomic.store(0, Ordering::Relaxed);
            total_atomic.store(0, Ordering::Relaxed);
        }
    });
}

pub fn progress_ratio(current: u64, total: u64) -> Option<f32> {
    if total == 0 {
        return None;
    }
    let cur = current.min(total);
    Some(cur as f32 / total as f32)
}

/// Static list of macOS Accessibility-permission setup steps shown on the
/// permission screen. Pure helper so the texts can be unit-tested
/// independently of any UI rendering — a copy-paste typo in step 1 (the
/// one with "System Settings") would otherwise only surface during a live
/// run on macOS.
pub fn permission_steps() -> &'static [&'static str] {
    &[
        "Open System Settings → Privacy & Security → Accessibility.",
        "Click \"+\" and add the wiredesk-client binary.",
        "Toggle the switch ON for wiredesk-client.",
        "Restart WireDesk — required: the tap thread is created at startup.",
    ]
}

impl WireDeskApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_config: ClientConfig,
        events_rx: mpsc::Receiver<TransportEvent>,
        outgoing_tx: mpsc::Sender<Packet>,
        tap_events_rx: mpsc::Receiver<TapEvent>,
        tap_handle: TapHandle,
        outgoing_progress: Arc<AtomicU64>,
        outgoing_total: Arc<AtomicU64>,
        incoming_progress: Arc<AtomicU64>,
        incoming_total: Arc<AtomicU64>,
        send_images: Arc<AtomicBool>,
        receive_images: Arc<AtomicBool>,
        send_text: Arc<AtomicBool>,
        receive_text: Arc<AtomicBool>,
        swap_option_command: Arc<AtomicBool>,
        outgoing_cancel: Arc<AtomicBool>,
        incoming_cancel: Arc<AtomicBool>,
    ) -> Self {
        let runtime_serial_port = initial_config.port.clone();
        let runtime_preferred_monitor = initial_config.preferred_monitor.clone();
        let initial_w = initial_config.width;
        let initial_h = initial_config.height;
        Self {
            state: ConnectionState::Disconnected,
            capturing: false,
            fullscreen: false,
            host_name: String::new(),
            screen_w: initial_w,
            screen_h: initial_h,
            clipboard_text: String::new(),
            status_msg: "ready".into(),
            runtime_serial_port,
            events_rx: Some(events_rx),
            outgoing_tx,
            tap_events_rx: Some(tap_events_rx),
            tap_handle: Some(tap_handle),
            mapper: InputMapper::new(initial_w, initial_h),
            seq: 0,
            permission_granted: keyboard_tap::is_permission_granted(),
            last_perm_check: Instant::now(),
            shell_open: false,
            shell_output: String::new(),
            shell_input: String::new(),
            shell_kind: String::new(),
            shell_just_opened: false,
            pending_config: initial_config,
            config_dirty: false,
            save_toast: None,
            available_ports: Vec::new(),
            cached_monitors: Vec::new(),
            cached_monitors_at: None,
            monitor_fallback_msg: None,
            original_position: None,
            pending_position_restore: None,
            last_dock_icon_apply: None,
            dock_icon_apply_count: 0,
            runtime_preferred_monitor,
            outgoing_progress,
            outgoing_total,
            incoming_progress,
            incoming_total,
            send_images,
            receive_images,
            send_text,
            receive_text,
            swap_option_command,
            outgoing_cancel,
            incoming_cancel,
            transient_toast: None,
        }
    }

    fn refresh_available_ports(&mut self) {
        if let Ok(ports) = serialport::available_ports() {
            self.available_ports = ports
                .into_iter()
                .map(|p| p.port_name)
                .filter(|n| n.starts_with("/dev/cu."))
                .collect();
        }
    }

    /// Refresh the cached monitor list if the cache is stale (older than
    /// `MONITOR_CACHE_TTL`) or empty. `NSScreen::screens()` walks an Obj-C
    /// array, so calling it 60×/s while the settings panel is open is wasteful;
    /// 1 Hz is plenty for a list that only changes when displays are
    /// hot-plugged.
    fn refresh_monitors_if_stale(&mut self) {
        const MONITOR_CACHE_TTL: Duration = Duration::from_secs(1);
        let stale = self
            .cached_monitors_at
            .map(|t| t.elapsed() >= MONITOR_CACHE_TTL)
            .unwrap_or(true);
        if stale {
            self.cached_monitors = monitor::list_monitors();
            self.cached_monitors_at = Some(Instant::now());
        }
    }

    /// Render the editable settings block — only shown in chrome mode.
    /// Mutating any field flips `config_dirty`; Save persists to TOML and
    /// flashes a 3-second toast. Changes don't affect the running session
    /// until the user restarts the binary (per Save+Restart design).
    fn render_settings_panel(&mut self, ui: &mut egui::Ui) {
        let mut dirty = false;
        let mut want_save = false;
        let mut want_save_and_restart = false;
        let mut want_reset = false;
        let mut want_refresh_ports = false;
        let available_ports = self.available_ports.clone();
        // Refresh cached monitors at most once a second — `NSScreen::screens()`
        // is a sync IPC call, no point re-querying it 60×/s while the panel
        // sits open (or at all unless something has changed).
        self.refresh_monitors_if_stale();
        let monitors = self.cached_monitors.clone();

        ui.collapsing("Settings", |ui| {
            let cfg = &mut self.pending_config;

            // ---- Connection group ----
            ui.group(|ui| {
                ui.label(egui::RichText::new("Connection").strong());
                ui.horizontal(|ui| {
                    ui.label("Port:");
                    let combo = egui::ComboBox::from_id_salt("settings_port")
                        .selected_text(cfg.port.clone())
                        .show_ui(ui, |ui| {
                            for p in &available_ports {
                                if ui
                                    .selectable_value(&mut cfg.port, p.clone(), p)
                                    .changed()
                                {
                                    dirty = true;
                                }
                            }
                        });
                    if combo.response.clicked() {
                        want_refresh_ports = true;
                    }
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut cfg.port)
                                .desired_width(220.0)
                                .hint_text("/dev/cu.usbserial-XXX"),
                        )
                        .changed()
                    {
                        dirty = true;
                    }
                });

                ui.horizontal(|ui| {
                    ui.label("Baud:");
                    let mut baud_str = cfg.baud.to_string();
                    if ui
                        .add(egui::TextEdit::singleline(&mut baud_str).desired_width(120.0))
                        .changed()
                    {
                        if let Ok(v) = baud_str.parse::<u32>() {
                            cfg.baud = v;
                            dirty = true;
                        }
                    }
                });
            });

            // ---- Display group ----
            ui.group(|ui| {
                ui.label(egui::RichText::new("Display").strong());
                ui.horizontal(|ui| {
                    ui.label("Host screen:");
                    let mut w_str = cfg.width.to_string();
                    let mut h_str = cfg.height.to_string();
                    if ui
                        .add(egui::TextEdit::singleline(&mut w_str).desired_width(80.0))
                        .changed()
                    {
                        if let Ok(v) = w_str.parse::<u16>() {
                            cfg.width = v;
                            dirty = true;
                        }
                    }
                    ui.label("×");
                    if ui
                        .add(egui::TextEdit::singleline(&mut h_str).desired_width(80.0))
                        .changed()
                    {
                        if let Ok(v) = h_str.parse::<u16>() {
                            cfg.height = v;
                            dirty = true;
                        }
                    }
                });

                // Fullscreen target monitor. The ComboBox **displays** the
                // user-friendly `monitor_label(m)` ("Studio Display
                // (5120×2880)") while the **saved value** is the unique
                // `monitor_identity(m)` ("Studio Display (5120×2880 @ 0,0)").
                // Splitting display from storage lets two physically
                // identical displays — same name, same resolution — coexist
                // in a dual-monitor setup without one shadowing the other on
                // restart (origin disambiguates them, and NSScreen never
                // lets two displays overlap). Default `None` keeps the
                // legacy "fullscreen on whichever display the window sits
                // on" behaviour. Identity also survives reboot / dock /
                // hot-plug as long as the same physical layout returns.
                ui.horizontal(|ui| {
                    ui.label("Fullscreen monitor:");
                    let selected_text = match cfg.preferred_monitor.as_deref() {
                        None => "(active monitor — default)".to_string(),
                        Some(saved) => {
                            match monitors
                                .iter()
                                .find(|m| monitor::monitor_identity(m) == saved)
                            {
                                Some(m) => format!(
                                    "Display {} — {}",
                                    m.index + 1,
                                    monitor::monitor_label(m),
                                ),
                                // Saved identity doesn't match any live
                                // display (unplugged, renamed, resolution
                                // changed, or moved to a different physical
                                // position since last save). Show the saved
                                // identity with an "(unavailable)" hint so
                                // the user knows something stale is selected.
                                None => format!("{saved} (unavailable)"),
                            }
                        }
                    };
                    egui::ComboBox::from_id_salt("settings_preferred_monitor")
                        .selected_text(selected_text)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_value(
                                    &mut cfg.preferred_monitor,
                                    None,
                                    "(active monitor — default)",
                                )
                                .changed()
                            {
                                dirty = true;
                            }
                            for m in &monitors {
                                // Friendly text for the dropdown row …
                                let display_text = format!(
                                    "Display {} — {}",
                                    m.index + 1,
                                    monitor::monitor_label(m),
                                );
                                // … but persist the unique identity so
                                // identical displays don't collide.
                                let identity = monitor::monitor_identity(m);
                                if ui
                                    .selectable_value(
                                        &mut cfg.preferred_monitor,
                                        Some(identity),
                                        display_text,
                                    )
                                    .changed()
                                {
                                    dirty = true;
                                }
                            }
                        });
                });
            });

            // ---- Clipboard group ----
            // Two independent toggles for image clipboard sync. Text always
            // syncs (cheap, low-bandwidth). Each checkbox flips a runtime
            // `Arc<AtomicBool>` shared with the poll thread (send) and
            // IncomingClipboard (receive) — takes effect immediately, no
            // restart needed. Save persists the values to config.toml.
            ui.group(|ui| {
                ui.label(egui::RichText::new("Clipboard").strong());
                let mut send_imgs = cfg.send_images;
                if ui
                    .checkbox(&mut send_imgs, "Send images (Mac → Host)")
                    .changed()
                {
                    cfg.send_images = send_imgs;
                    self.send_images.store(send_imgs, Ordering::Relaxed);
                    dirty = true;
                }
                let mut recv_imgs = cfg.receive_images;
                if ui
                    .checkbox(&mut recv_imgs, "Receive images (Host → Mac)")
                    .changed()
                {
                    cfg.receive_images = recv_imgs;
                    self.receive_images.store(recv_imgs, Ordering::Relaxed);
                    dirty = true;
                }
                let mut send_txt = cfg.send_text;
                if ui
                    .checkbox(&mut send_txt, "Send text (Mac → Host)")
                    .changed()
                {
                    cfg.send_text = send_txt;
                    self.send_text.store(send_txt, Ordering::Relaxed);
                    dirty = true;
                }
                let mut recv_txt = cfg.receive_text;
                if ui
                    .checkbox(&mut recv_txt, "Receive text (Host → Mac)")
                    .changed()
                {
                    cfg.receive_text = recv_txt;
                    self.receive_text.store(recv_txt, Ordering::Relaxed);
                    dirty = true;
                }
                ui.label(
                    egui::RichText::new(
                        "Toggle individual directions per content type. \
                         Useful when an app like Whispr Flow keeps writing \
                         transcribed text into the clipboard.",
                    )
                    .small()
                    .color(egui::Color32::GRAY),
                );
            });

            // ---- System group ----
            ui.group(|ui| {
                ui.label(egui::RichText::new("System").strong());
                ui.horizontal(|ui| {
                    ui.label("Client name:");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut cfg.client_name)
                                .desired_width(220.0),
                        )
                        .changed()
                    {
                        dirty = true;
                    }
                });
                let mut swap_oc = cfg.swap_option_command;
                if ui
                    .checkbox(&mut swap_oc, "Swap ⌥/⌘ on Host (Karabiner-Elements compensation)")
                    .changed()
                {
                    cfg.swap_option_command = swap_oc;
                    self.swap_option_command.store(swap_oc, Ordering::Relaxed);
                    dirty = true;
                }
                ui.label(
                    egui::RichText::new(
                        "Enable if you remap left_command ↔ left_option in \
                         Karabiner-Elements so the same physical keyboard \
                         works on macOS and Windows. Without this WireDesk \
                         forwards Cmd+V as Alt+V to Host. Cmd+Esc / Cmd+Enter \
                         keep firing on the same physical key you press today.",
                    )
                    .small()
                    .color(egui::Color32::GRAY),
                );
            });

            ui.horizontal(|ui| {
                let save_enabled = self.config_dirty || dirty;
                if ui
                    .add_enabled(save_enabled, egui::Button::new("Save && Restart"))
                    .clicked()
                {
                    want_save_and_restart = true;
                }
                if ui
                    .add_enabled(save_enabled, egui::Button::new("Save"))
                    .clicked()
                {
                    want_save = true;
                }
                if ui.button("Reset to defaults").clicked() {
                    want_reset = true;
                }
            });

            if let Some((msg, when)) = &self.save_toast {
                if when.elapsed() < Duration::from_secs(3) {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, msg);
                }
            }
        });

        if dirty {
            self.config_dirty = true;
        }
        if want_refresh_ports {
            self.refresh_available_ports();
        }
        if want_reset {
            self.pending_config = ClientConfig::default();
            self.config_dirty = true;
        }
        if want_save {
            match self.pending_config.save() {
                Ok(()) => {
                    self.config_dirty = false;
                    self.save_toast = Some((
                        "Saved. Restart WireDesk to apply.".to_string(),
                        Instant::now(),
                    ));
                }
                Err(e) => {
                    self.save_toast =
                        Some((format!("Save failed: {e}"), Instant::now()));
                }
            }
        }
        if want_save_and_restart {
            match self.pending_config.save() {
                Ok(()) => {
                    self.config_dirty = false;
                    crate::restart::restart_app();
                }
                Err(e) => {
                    self.save_toast =
                        Some((format!("Save failed: {e}"), Instant::now()));
                }
            }
        }
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    /// Send a sequence of key packets through the outgoing channel (used for special combos).
    fn send_key_sequence(&mut self, keys: &[(u16, u8, bool)]) {
        for &(scancode, modifiers, pressed) in keys {
            let seq = self.next_seq();
            let msg = if pressed {
                Message::KeyDown { scancode, modifiers }
            } else {
                Message::KeyUp { scancode, modifiers }
            };
            let _ = self.outgoing_tx.send(Packet::new(msg, seq));
        }
    }

    fn toggle_capture(&mut self) {
        self.capturing = !self.capturing;
        if self.capturing {
            self.status_msg = "input captured (Cmd+Esc to release)".into();
        } else {
            self.status_msg = "input released".into();
        }
        // Actual tap activation is driven by sync_tap_to_focus() in update()
        // — that way clicking away to another Mac app pauses the tap so Mac
        // shortcuts (Cmd+V to paste etc.) work on the Mac side.
    }

    /// Reconcile the tap's active state with the current intent
    /// (`capturing` flag) AND window focus. Tap intercepts only when both
    /// are true; losing focus pauses the tap so Mac apps work normally
    /// without disturbing the user's `capturing` intent.
    fn sync_tap_to_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        let Some(h) = self.tap_handle.as_ref() else {
            return;
        };
        // Three states:
        // - focused && capturing → ACTIVE: tap intercepts every key, forwards
        //   to Host. Cmd+Esc inside emits ReleaseCapture; Cmd+Enter emits
        //   ToggleFullscreen.
        // - focused && !capturing → PASSIVE: tap watches Cmd+Esc / Cmd+Enter
        //   only. Cmd+Esc emits EngageCapture; Cmd+Enter emits
        //   ToggleFullscreen. Other keys pass through to macOS.
        // - !focused → IDLE: tap doesn't intercept anything. Mac shortcuts
        //   work normally regardless of capture state.
        match (focused, self.capturing) {
            (true, true) => h.enable(),
            (true, false) => h.enable_passive(),
            (false, _) => h.disable(),
        }
    }

    /// Toggle fullscreen with optional per-monitor targeting.
    ///
    /// On entering fullscreen: if `runtime_preferred_monitor` resolves to a
    /// valid `MonitorInfo`, save the current outer position, move the
    /// window onto that monitor's origin, then request `Fullscreen(true)` —
    /// egui-on-macOS treats fullscreen as "expand to the screen this window
    /// is currently on", so the move-then-fullscreen sequence is what
    /// targets a specific display. If `runtime_preferred_monitor` is None
    /// or its name no longer matches any live display, fall back to
    /// "fullscreen on current display" without moving and surface the
    /// fallback via a dedicated TTL'd banner (`monitor_fallback_msg`) so
    /// it doesn't clobber real disconnect reasons in `status_msg`.
    ///
    /// **Reads `runtime_preferred_monitor`, NOT `pending_config`.** Unsaved
    /// edits in the Settings panel must not change live runtime behaviour
    /// — that contradicts the documented "Save & Restart" contract used
    /// by every other Settings field (port, baud, etc.).
    ///
    /// **Known limitation (egui 0.31 + AppKit):** `OuterPosition` is processed
    /// asynchronously on macOS — there's a chance `Fullscreen(true)` fires
    /// before AppKit has actually moved the window, so on rare races the
    /// fullscreen lands on the original display. The two viewport commands
    /// arrive in order and AppKit usually flushes the move before the
    /// fullscreen transition; if this becomes a real problem we'd need a
    /// state machine that waits for the next `update()` after the move
    /// before issuing fullscreen.
    ///
    /// On exiting fullscreen: send `Fullscreen(false)`, then restore the
    /// saved outer position via `OuterPosition` (and clear it via `take()`).
    /// If no position was saved, leave the window where the OS placed it.
    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        if self.fullscreen {
            let monitors = monitor::list_monitors();
            let preferred = self.runtime_preferred_monitor.as_deref();
            let target = monitor::resolve_target_monitor(preferred, &monitors);
            match target {
                Some(m) => {
                    self.original_position = ctx
                        .input(|i| i.viewport().outer_rect.map(|r| r.min));
                    ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(
                        m.frame.min,
                    ));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                    crate::presentation::enter_kiosk();
                }
                None => {
                    // runtime_preferred_monitor was Some(name) but no live
                    // display matches (unplugged or renamed) — surface the
                    // fallback via a dedicated TTL banner instead of
                    // overwriting status_msg, which is what disconnect
                    // reasons (e.g. "disconnected: serial: device busy")
                    // use as their home. Without a separate slot a
                    // fullscreen attempt while disconnected would erase
                    // the real reason.
                    if preferred.is_some() {
                        self.monitor_fallback_msg = Some((
                            "Selected monitor unavailable; fullscreen on current display"
                                .to_string(),
                            Instant::now(),
                        ));
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                    crate::presentation::enter_kiosk();
                }
            }
            // Going fullscreen implies "I want to drive the Host" — auto-engage
            // capture so the user doesn't need a second Cmd+Esc. If capture
            // was already on, this is a no-op.
            if !self.capturing {
                self.toggle_capture();
            }
        } else {
            // Pair with the auto-engage above: leaving fullscreen releases
            // capture so the Mac's keyboard works locally without a second
            // shortcut. If the user had toggled capture independently while
            // fullscreen, we still release it here — single-source-of-truth
            // is fullscreen state for this UX.
            if self.capturing {
                self.toggle_capture();
            }
            crate::presentation::exit_kiosk();
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            if let Some(pos) = self.original_position.take() {
                // Defer OuterPosition until macOS finishes the Spaces
                // transition (~500ms). Sending it inline puts the window
                // on a Space that's already disappearing → user can't find
                // it without Mission Control. The pending state is drained
                // in update() once the timer elapses.
                self.pending_position_restore = Some((pos, Instant::now()));
                ctx.request_repaint_after(Duration::from_millis(700));
            }
        }
    }

    /// Re-apply the Dock icon a few times during startup. winit's NSApp
    /// init (and possibly TCC re-registration after Accessibility prompt)
    /// overwrites whatever was set in the creator callback ~1s in. Two
    /// or three re-applies at 1s / 3s / 6s after launch beat that race
    /// without permanently spinning the AppKit message pump.
    #[cfg(target_os = "macos")]
    fn reapply_dock_icon_if_needed(&mut self) {
        if self.dock_icon_apply_count >= 4 {
            return;
        }
        let due = match self.last_dock_icon_apply {
            None => true,
            Some(t) => {
                let elapsed = t.elapsed();
                let next_delay = match self.dock_icon_apply_count {
                    1 => Duration::from_millis(1_500),
                    2 => Duration::from_secs(3),
                    _ => Duration::from_secs(5),
                };
                elapsed >= next_delay
            }
        };
        if due {
            unsafe {
                crate::force_dock_icon_from_bundle();
            }
            self.last_dock_icon_apply = Some(Instant::now());
            self.dock_icon_apply_count += 1;
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn reapply_dock_icon_if_needed(&mut self) {}

    /// Drain the pending `OuterPosition` after fullscreen exit if its
    /// settle timer has elapsed. Called from `update()`.
    fn drain_pending_position_restore(&mut self, ctx: &egui::Context) {
        let Some((pos, when)) = self.pending_position_restore else {
            return;
        };
        if when.elapsed() >= Duration::from_millis(600) {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
            self.pending_position_restore = None;
        } else {
            // Wake again at the deadline so we don't miss the window.
            ctx.request_repaint_after(
                Duration::from_millis(600).saturating_sub(when.elapsed()),
            );
        }
    }

    fn shell_send(&mut self, msg: Message) {
        let seq = self.next_seq();
        let _ = self.outgoing_tx.send(Packet::new(msg, seq));
    }

    fn shell_open_request(&mut self) {
        if self.shell_open {
            return;
        }
        let kind = self.shell_kind.clone();
        self.shell_send(Message::ShellOpen { shell: kind });
        self.shell_open = true;
        self.shell_output.clear();
        // Tell the next render frame to grab keyboard focus for the
        // shell input — otherwise the user has to click into the field
        // before typing their first command.
        self.shell_just_opened = true;
    }

    fn shell_close_request(&mut self) {
        if !self.shell_open {
            return;
        }
        self.shell_send(Message::ShellClose);
    }

    fn shell_send_input(&mut self, text: &str) {
        // Append a newline so the line gets executed by the shell.
        let mut data = text.as_bytes().to_vec();
        data.push(b'\n');
        self.shell_send(Message::ShellInput { data });
    }

    /// True when WireDesk should render the full chrome (status, buttons,
    /// shell panel). False in capture or fullscreen mode — when the user is
    /// "in" Windows via the HDMI capture monitor and a stray click on a
    /// WireDesk button would be disruptive.
    fn should_show_chrome(&self) -> bool {
        !self.capturing && !self.fullscreen
    }

    /// Permission instruction screen shown when macOS Accessibility
    /// permission is missing. Without it the keyboard tap silently never
    /// fires, so we block the chrome until the user grants it.
    fn render_permission_screen(&self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.heading("Accessibility permission required");
        });
        ui.add_space(12.0);

        ui.label(
            "WireDesk needs macOS Accessibility permission to intercept \
             keyboard shortcuts (Cmd+Space, Cmd+C, etc.) and forward them \
             to the Host. Without it the capture-mode would only receive a \
             subset of keys, and clipboard syncs would be useless.",
        );
        ui.add_space(8.0);

        ui.label("To grant it:");
        ui.add_space(4.0);

        // Each step in its own group with a large numbered glyph on the
        // left. The "Open System Settings" button lives inside step 1 so
        // the action is co-located with the instruction that requires it.
        for (i, step) in permission_steps().iter().enumerate() {
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new((i + 1).to_string())
                            .size(STEP_NUMBER_SIZE)
                            .strong(),
                    );
                    ui.vertical(|ui| {
                        ui.label(*step);
                        if i == 0 && ui.button("Open System Settings").clicked() {
                            #[cfg(target_os = "macos")]
                            {
                                let _ = std::process::Command::new("open")
                                    .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                                    .spawn();
                            }
                        }
                    });
                });
            });
        }

        ui.add_space(12.0);
        ui.label(
            egui::RichText::new(
                "\u{26A0} After granting permission, quit and relaunch \
                 wiredesk-client. The window detects the change but the \
                 tap won't activate without a fresh process.",
            )
            .color(COLOR_WARNING),
        );
    }

    /// Info-only screen shown when capture or fullscreen is active. No
    /// clickable elements — just a description of what the app is doing
    /// and the relevant hotkeys.
    /// Render banner + progress bars as **overlays** (egui::Area), not
    /// inside the CentralPanel. Without this the banner Frame eats the
    /// top ~50px of CentralPanel layout — and since `mouse_pos` is
    /// normalised against the full `screen_rect`, the top of the Host
    /// display gets squashed into those pixels (Host top 3-5% mapped
    /// into Mac top 50px). Net effect: tab close-buttons / menu items
    /// in the very first row become almost unreachable. Overlays don't
    /// participate in CentralPanel layout, so coordinate mapping stays
    /// 1:1 across the full window.
    ///
    /// Banner is non-interactable (just a label, no clicks). Progress
    /// bars must stay interactable (cancel button), so they live in a
    /// separate Area pinned to the BOTTOM of the screen — Host's bottom
    /// row is less click-critical than the top.
    fn render_capture_overlays(&self, ctx: &egui::Context) {
        let screen_rect = ctx.screen_rect();
        let banner_fill = COLOR_CAPTURE_RED.linear_multiply(0.3);

        egui::Area::new(egui::Id::new("capture_banner_overlay"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .fixed_pos(screen_rect.min)
            .show(ctx, |ui| {
                ui.set_width(screen_rect.width());
                egui::Frame::group(ui.style())
                    .fill(banner_fill)
                    .show(ui, |ui| {
                        ui.with_layout(
                            egui::Layout::top_down(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    egui::RichText::new("CAPTURING — Cmd+Esc to release")
                                        .size(BANNER_FONT_SIZE)
                                        .strong()
                                        .color(egui::Color32::WHITE),
                                );
                            },
                        );
                    });
            });

        // Clipboard transfer progress — visible inside capture / fullscreen
        // because the macOS menu bar is hidden in fullscreen and chrome panel
        // is collapsed. Render only when a transfer is in flight.
        let out_cur = self.outgoing_progress.load(Ordering::Relaxed);
        let out_tot = self.outgoing_total.load(Ordering::Relaxed);
        let inc_cur = self.incoming_progress.load(Ordering::Relaxed);
        let inc_tot = self.incoming_total.load(Ordering::Relaxed);
        let any_active = out_tot > 0 || inc_tot > 0;
        if !any_active {
            return;
        }

        // Anchor at bottom-center so progress rows don't occlude Host
        // top row (the area users click most). LeftBottom anchor +
        // full width via set_width works around `Area::default_width`
        // not respecting `set_width` after layout starts.
        egui::Area::new(egui::Id::new("capture_progress_overlay"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -8.0))
            .show(ctx, |ui| {
                ui.set_max_width((screen_rect.width() - 32.0).max(200.0));
                if let (Some(ratio), Some(text)) = (
                    progress_ratio(out_cur, out_tot),
                    format_progress("Sending clipboard", out_cur, out_tot),
                ) {
                    render_progress_row(
                        ui,
                        ratio,
                        &text,
                        &self.outgoing_cancel,
                        &self.outgoing_progress,
                        &self.outgoing_total,
                    );
                }
                if let (Some(ratio), Some(text)) = (
                    progress_ratio(inc_cur, inc_tot),
                    format_progress("Receiving clipboard", inc_cur, inc_tot),
                ) {
                    render_progress_row(
                        ui,
                        ratio,
                        &text,
                        &self.incoming_cancel,
                        &self.incoming_progress,
                        &self.incoming_total,
                    );
                }
            });
    }

    /// Info-text inside CentralPanel: connection status + hotkey
    /// cheatsheet. Only shown when **not** in fullscreen — in fullscreen
    /// the user is looking at the Host monitor and this Mac-side info
    /// is invisible anyway, while keeping it in CentralPanel would
    /// reintroduce the layout-squash problem for Host top row.
    /// Info-text rendered as a non-interactable overlay so it stays
    /// visible in BOTH windowed and fullscreen capture without eating
    /// CentralPanel layout (which would reintroduce the top-row
    /// squash). Anchored center-vertical so it doesn't compete with
    /// banner (top) or progress overlay (bottom).
    ///
    /// `interactable(false)` is the key: cursor passes straight
    /// through, so mouse_pos.y in this region maps to Host as if the
    /// text wasn't there. Visually present, mechanically transparent.
    fn render_capture_info_text(&self, ctx: &egui::Context) {
        egui::Area::new(egui::Id::new("capture_info_overlay"))
            .order(egui::Order::Background)
            .interactable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_max_width(560.0);
                ui.vertical_centered(|ui| {
                    ui.heading("WireDesk — input forwarded to Host");
                    ui.add_space(8.0);
                    if self.state == ConnectionState::Connected {
                        ui.label(format!(
                            "Connected to {} ({}×{})",
                            self.host_name, self.screen_w, self.screen_h
                        ));
                    } else {
                        ui.label("Not connected");
                    }
                });
                ui.add_space(20.0);

                ui.label("Active hotkeys (intercepted locally — not sent to Host):");
                ui.indent("local_hotkeys", |ui| {
                    ui.label("• Cmd+Esc — release capture");
                    ui.label("• Cmd+Enter — toggle fullscreen");
                });
                ui.add_space(8.0);

                ui.label("Forwarded to Host (Cmd → Ctrl mapping):");
                ui.indent("forwarded", |ui| {
                    ui.label("• Cmd+Space → Win+Space (input language toggle)");
                    ui.label("• Cmd+C / Cmd+V → Ctrl+C / Ctrl+V");
                    ui.label("• Cmd+Tab, Cmd+Q, etc. — all Cmd-combos go to Host");
                    ui.label("• Letters, digits, function keys, arrows — direct");
                });
                ui.add_space(12.0);

                ui.label(
                    "Clipboard auto-syncs both ways every ~500 ms — copy on either \
                     side appears on the other.",
                );
                ui.add_space(8.0);
                ui.weak(
                    "Tap pauses automatically when this window loses focus — switch \
                     to another Mac app and Cmd-shortcuts work locally again.",
                );
            });
    }

    /// Human-readable status string for the chrome status row. Pure helper
    /// (no UI/IO) so it's unit-tested separately. `Disconnected` includes
    /// the reason from `status_msg` when one is present, so the user sees
    /// "Disconnected: serial: device busy" instead of just "Not connected".
    fn status_text(&self) -> String {
        match self.state {
            ConnectionState::Connected => format!(
                "Connected to {} ({}×{})",
                self.host_name, self.screen_w, self.screen_h
            ),
            ConnectionState::Connecting => "Connecting…".to_string(),
            ConnectionState::Disconnected => {
                // Echo the most recent status message when it carries a
                // disconnect reason ("disconnected: …"). Otherwise fall
                // back to a plain "Not connected".
                if let Some(rest) = self.status_msg.strip_prefix("disconnected: ") {
                    format!("Disconnected: {rest}")
                } else {
                    "Not connected".to_string()
                }
            }
        }
    }

    fn shell_append_output(&mut self, bytes: &[u8]) {
        // Lossy UTF-8 — shell output may contain mixed encodings or partial sequences.
        let s = String::from_utf8_lossy(bytes);
        self.shell_output.push_str(&s);
        // Cap scrollback at ~64 KB to avoid unbounded growth.
        const MAX: usize = 64 * 1024;
        if self.shell_output.len() > MAX {
            let excess = self.shell_output.len() - MAX;
            self.shell_output.drain(..excess);
        }
    }
}

impl eframe::App for WireDeskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process transport events. Drain into a local Vec first so we can
        // call &mut self helpers without conflicting with the rx borrow.
        let mut pending: Vec<TransportEvent> = Vec::new();
        if let Some(ref rx) = self.events_rx {
            while let Ok(event) = rx.try_recv() {
                pending.push(event);
            }
        }
        for event in pending {
            match event {
                TransportEvent::Connected { host_name, screen_w, screen_h } => {
                    self.state = ConnectionState::Connected;
                    self.host_name = host_name;
                    self.screen_w = screen_w;
                    self.screen_h = screen_h;
                    self.mapper.set_screen_size(screen_w, screen_h);
                    self.status_msg = format!("connected to {}", self.host_name);
                    // Counter reset for re-handshake-without-prior-Disconnect
                    // is owned by `reader_thread` now (Codex iter5): clearing
                    // counters from the UI thread on Connected raced with a
                    // peer ClipOffer arriving in the same frame and wiped the
                    // freshly stored `incoming_total` mid-transfer. The reader
                    // already zeroes both directions at HelloAck before
                    // emitting this event, so the status row starts clean.
                }
                TransportEvent::Disconnected(reason) => {
                    self.state = ConnectionState::Disconnected;
                    self.capturing = false;
                    self.status_msg = format!("disconnected: {reason}");
                    // Drop any in-flight clipboard progress — the wire is
                    // gone, the receiver will reset its IncomingClipboard
                    // separately, and a stale "Sending image — 30%" line in
                    // the status row would mislead the user.
                    self.outgoing_progress.store(0, Ordering::Relaxed);
                    self.outgoing_total.store(0, Ordering::Relaxed);
                    self.incoming_progress.store(0, Ordering::Relaxed);
                    self.incoming_total.store(0, Ordering::Relaxed);
                }
                TransportEvent::ClipboardFromHost(text) => {
                    self.clipboard_text = text;
                }
                TransportEvent::Heartbeat => {}
                TransportEvent::ShellOutput(data) => {
                    self.shell_append_output(&data);
                }
                TransportEvent::ShellExit(code) => {
                    self.shell_open = false;
                    self.shell_append_output(
                        format!("\n[shell exited with code {code}]\n").as_bytes(),
                    );
                }
                TransportEvent::ShellError(msg) => {
                    self.shell_open = false;
                    self.shell_append_output(format!("\n[shell error: {msg}]\n").as_bytes());
                }
                TransportEvent::Toast(msg) => {
                    self.transient_toast = Some((msg, Instant::now()));
                }
            }
        }

        // Re-check Accessibility permission periodically. update() runs at
        // ~60fps; we throttle to once every 2s — enough for the UI to react
        // when the user grants permission via System Settings without
        // hammering the (potentially slow) sync IPC call to SystemServices.
        if self.last_perm_check.elapsed() >= Duration::from_secs(2) {
            self.permission_granted = keyboard_tap::is_permission_granted();
            self.last_perm_check = Instant::now();
        }

        // Reconcile tap state with capture intent + window focus. When the
        // WireDesk window loses focus (user clicks another Mac app), pause
        // the tap so Mac shortcuts (Cmd+V to paste etc.) work normally on
        // the Mac side. Resumes when window gets focus back.
        self.sync_tap_to_focus(ctx);
        self.drain_pending_position_restore(ctx);
        self.reapply_dock_icon_if_needed();

        // Drain TapEvents from the keyboard tap thread.
        let mut pending_tap_events: Vec<TapEvent> = Vec::new();
        if let Some(ref rx) = self.tap_events_rx {
            while let Ok(ev) = rx.try_recv() {
                pending_tap_events.push(ev);
            }
        }
        for ev in pending_tap_events {
            match ev {
                TapEvent::ReleaseCapture => {
                    if self.capturing {
                        self.toggle_capture();
                    }
                }
                TapEvent::EngageCapture => {
                    if !self.capturing {
                        self.toggle_capture();
                    }
                }
                TapEvent::ToggleFullscreen => {
                    self.toggle_fullscreen(ctx);
                }
            }
        }

        let show_chrome = self.should_show_chrome();
        let permission_granted = self.permission_granted;

        // In capture mode banner / progress / info-text all live in
        // overlays so they don't occlude Host's top row coordinate-
        // mapping. Render BEFORE CentralPanel so they sit on top.
        // CentralPanel itself stays empty — Mac coords map 1:1 onto
        // full Host display in both windowed and fullscreen capture.
        if !show_chrome {
            self.render_capture_overlays(ctx);
            self.render_capture_info_text(ctx);
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if !show_chrome {
                return;
            }

            if !permission_granted {
                self.render_permission_screen(ui);
                return;
            }

            // Wrap chrome content in a ScrollArea so the Settings panel
            // can grow past the window's initial height — the user can
            // either resize the window or scroll without losing buttons
            // (Save / Save & Restart / Reset) below the fold.
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
            ui.horizontal(|ui| {
                // 28px icon next to the heading — branding consistency with
                // the Win host title-bar icon. egui downscales the 1024×1024
                // source on the fly; no separate small asset needed.
                ui.add(
                    egui::Image::new(egui::include_image!(
                        "../../../assets/icon-source.png"
                    ))
                    .fit_to_exact_size(egui::vec2(HEADING_ICON_SIZE, HEADING_ICON_SIZE)),
                );
                ui.heading("WireDesk");
            });
            ui.separator();

            // Connection status — large coloured circle + human-friendly
            // text. The circle is painted directly via `ui.painter()`
            // instead of a Unicode glyph because egui's default font lacks
            // U+25CF (BLACK CIRCLE) on some macOS configurations and falls
            // back to an empty tofu box (observed live, ui-redesign branch).
            let status_color = match self.state {
                ConnectionState::Connected => egui::Color32::GREEN,
                ConnectionState::Connecting => egui::Color32::YELLOW,
                ConnectionState::Disconnected => egui::Color32::RED,
            };
            let status_text = self.status_text();
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(STATUS_GLYPH_SIZE, STATUS_GLYPH_SIZE),
                    egui::Sense::hover(),
                );
                ui.painter()
                    .circle_filled(rect.center(), STATUS_GLYPH_SIZE * 0.45, status_color);
                ui.label(status_text);
            });

            ui.label(format!("Serial: {}", self.runtime_serial_port));

            // Clipboard progress line — only renders when a transfer is
            // active. Outgoing and incoming are both possible at the same
            // time (peer copies an image while we copy text); show both in
            // a single row so the chrome layout stays compact.
            // Codex C4: labels are intentionally generic ("clipboard", not
            // "image") — the same counters track text and image transfers
            // and a text Cmd+C briefly flashed "Sending image — 0/512 B"
            // before being dropped. The status-line consumer only knows
            // bytes/total, not the format (which lives one layer down in
            // the Message::ClipOffer that's already long-since enqueued).
            // Visual progress bars per direction. ProgressBar fills from
            // left as bytes hit the wire; text inside the bar shows
            // "Sending/Receiving clipboard — N/M KB (P%)". Bars only render
            // when their respective transfer is active (total > 0).
            let out_cur = self.outgoing_progress.load(Ordering::Relaxed);
            let out_tot = self.outgoing_total.load(Ordering::Relaxed);
            let inc_cur = self.incoming_progress.load(Ordering::Relaxed);
            let inc_tot = self.incoming_total.load(Ordering::Relaxed);
            if let (Some(ratio), Some(text)) = (
                progress_ratio(out_cur, out_tot),
                format_progress("Sending clipboard", out_cur, out_tot),
            ) {
                render_progress_row(
                    ui,
                    ratio,
                    &text,
                    &self.outgoing_cancel,
                    &self.outgoing_progress,
                    &self.outgoing_total,
                );
                ctx.request_repaint_after(Duration::from_millis(250));
            }
            if let (Some(ratio), Some(text)) = (
                progress_ratio(inc_cur, inc_tot),
                format_progress("Receiving clipboard", inc_cur, inc_tot),
            ) {
                render_progress_row(
                    ui,
                    ratio,
                    &text,
                    &self.incoming_cancel,
                    &self.incoming_progress,
                    &self.incoming_total,
                );
                ctx.request_repaint_after(Duration::from_millis(250));
            }

            ui.separator();

            // Capture toggle — primary action, prominent. RichText size
            // 16pt strong + colored fill + min_size [200, 32] makes it the
            // visual anchor of the chrome panel. Color flips between blue
            // (idle) and red (capturing) as a state cue, matching the
            // capture-banner palette in `render_capture_info`.
            let (btn_text, btn_fill) = if self.capturing {
                ("Release Input", COLOR_CAPTURE_RED)
            } else {
                ("Capture Input", COLOR_CAPTURE_BLUE)
            };
            let capture_btn = egui::Button::new(
                egui::RichText::new(btn_text).size(16.0).strong(),
            )
            .fill(btn_fill)
            .min_size(CAPTURE_BTN_MIN_SIZE);
            if ui.add(capture_btn).clicked() {
                self.toggle_capture();
            }
            let capture_label = if self.capturing {
                "Input: CAPTURED (Cmd+Esc to release)"
            } else {
                "Input: released"
            };
            ui.label(capture_label);

            ui.separator();

            // Clipboard
            if !self.clipboard_text.is_empty() {
                ui.label("Clipboard from Host:");
                let preview = if self.clipboard_text.chars().count() > 200 {
                    let truncated: String = self.clipboard_text.chars().take(200).collect();
                    format!("{truncated}...")
                } else {
                    self.clipboard_text.clone()
                };
                ui.code(preview);
                if ui.button("Copy to Mac clipboard").clicked() {
                    ctx.copy_text(self.clipboard_text.clone());
                }
            }

            ui.separator();

            // Special keys buttons
            let mut send_cad = false;
            let mut send_win = false;
            let mut send_lang = false;
            if self.state == ConnectionState::Connected {
                ui.horizontal(|ui| {
                    if ui.button("Ctrl+Alt+Del").clicked() {
                        send_cad = true;
                    }
                    if ui.button("Win key").clicked() {
                        send_win = true;
                    }
                    if ui.button("Lang (Win+Space)").clicked() {
                        send_lang = true;
                    }
                });
            }
            if send_cad {
                self.send_key_sequence(&[
                    (0x1D, 0x01, true),   // Ctrl down
                    (0x38, 0x05, true),   // Alt down
                    (0xE053, 0x05, true), // Del down
                    (0xE053, 0x00, false),
                    (0x38, 0x00, false),
                    (0x1D, 0x00, false),
                ]);
            }
            if send_win {
                self.send_key_sequence(&[
                    (0xE05B, 0x00, true),  // Win down
                    (0xE05B, 0x00, false), // Win up
                ]);
            }
            if send_lang {
                // Win+Space — стандартный шорткат смены языка в Windows 11.
                self.send_key_sequence(&[
                    (0xE05B, 0x08, true), // Win down (META=0x08)
                    (0x39, 0x08, true),   // Space down
                    (0x39, 0x00, false),  // Space up
                    (0xE05B, 0x00, false), // Win up
                ]);
            }

            ui.separator();

            // Terminal-over-serial
            let mut want_open = false;
            let mut want_close = false;
            let mut want_send = false;
            ui.collapsing("Terminal (serial shell)", |ui| {
                if self.state != ConnectionState::Connected {
                    ui.label("Connect first to use the shell.");
                    return;
                }

                ui.horizontal(|ui| {
                    if !self.shell_open {
                        ui.label("Shell:");
                        egui::ComboBox::from_id_salt("shell_kind")
                            .selected_text(if self.shell_kind.is_empty() {
                                "default".into()
                            } else {
                                self.shell_kind.clone()
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.shell_kind, String::new(), "default");
                                ui.selectable_value(&mut self.shell_kind, "powershell".into(), "powershell");
                                ui.selectable_value(&mut self.shell_kind, "cmd".into(), "cmd");
                            });
                        if ui.button("Open").clicked() {
                            want_open = true;
                        }
                    } else {
                        ui.label("shell open");
                        if ui.button("Close").clicked() {
                            want_close = true;
                        }
                        if ui.button("Clear output").clicked() {
                            self.shell_output.clear();
                        }
                    }
                });

                // Output area — read-only, scrollable, monospace
                egui::ScrollArea::vertical()
                    .id_salt("shell_output_scroll")
                    .stick_to_bottom(true)
                    .max_height(220.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.shell_output.as_str())
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .desired_rows(10)
                                .interactive(false),
                        );
                    });

                if self.shell_open {
                    ui.horizontal(|ui| {
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.shell_input)
                                .id_salt("shell_input")
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .hint_text("type a command, Enter to send"),
                        );
                        let enter_pressed = resp.lost_focus()
                            && ui.input(|i: &egui::InputState| i.key_pressed(egui::Key::Enter));
                        if enter_pressed && !self.shell_input.is_empty() {
                            want_send = true;
                            // Pressing Enter takes focus away from a
                            // singleline TextEdit. Reclaim it now so the
                            // next command can be typed without clicking
                            // back into the field.
                            resp.request_focus();
                        }
                        if self.shell_just_opened {
                            resp.request_focus();
                            self.shell_just_opened = false;
                        }
                    });
                }
            });

            if want_open {
                self.shell_open_request();
            }
            if want_close {
                self.shell_close_request();
            }
            if want_send {
                let line = std::mem::take(&mut self.shell_input);
                // Echo into local scrollback so user sees what was sent.
                self.shell_append_output(format!("> {line}\n").as_bytes());
                self.shell_send_input(&line);
            }

            ui.separator();
            self.render_settings_panel(ui);

            ui.separator();
            ui.small(&self.status_msg);
            // Inline TTL banner for the per-monitor fullscreen fallback —
            // separate from `status_msg` so it doesn't overwrite a real
            // disconnect reason when both happen at once. Shows for 5 s
            // then disappears on its own.
            if let Some((msg, when)) = &self.monitor_fallback_msg {
                if when.elapsed() < Duration::from_secs(5) {
                    ui.colored_label(COLOR_WARNING, msg);
                }
            }
            // Generic transient toast (Task 7b) — currently the
            // "image too large" warning from the clipboard poll thread.
            // Rendered in warning-orange to match the other inline alert
            // hue. After the 3-second TTL elapses, drop the value so the
            // chrome doesn't keep allocating layout space for an empty row.
            if self
                .transient_toast
                .as_ref()
                .is_some_and(|(_, when)| when.elapsed() >= Duration::from_secs(3))
            {
                self.transient_toast = None;
            }
            if let Some((msg, _)) = &self.transient_toast {
                ui.colored_label(COLOR_WARNING, msg);
            }
                }); // ScrollArea
        });

        // Handle captured input — push to outgoing channel (non-blocking).
        if self.capturing && self.state == ConnectionState::Connected {
            // When the OS-level keyboard tap is active (macOS + permission),
            // it's the sole source of key events — skip egui forwarding to
            // avoid double KeyDown. Mouse always goes through egui (the tap
            // only intercepts keyboard).
            let tap_owns_keys = self
                .tap_handle
                .as_ref()
                .map(|h| h.is_active())
                .unwrap_or(false);

            let events: Vec<egui::Event> = ctx.input(|input: &egui::InputState| {
                input.events.clone()
            });
            let mouse_pos = ctx.input(|input: &egui::InputState| input.pointer.hover_pos());
            let screen_rect = ctx.screen_rect();

            for event in &events {
                match event {
                    egui::Event::Key { key, pressed, modifiers, .. } => {
                        if tap_owns_keys {
                            continue;
                        }
                        // Don't forward the capture-toggle combo to Host
                        if *key == egui::Key::Escape && modifiers.command {
                            continue;
                        }
                        // Don't forward the fullscreen toggle either
                        if *key == egui::Key::Enter && modifiers.command {
                            continue;
                        }
                        self.mapper.send_key(&self.outgoing_tx, key, modifiers, *pressed);
                    }
                    egui::Event::PointerButton { button, pressed, .. } => {
                        let btn = crate::input::mapper::pointer_button_to_proto(*button);
                        self.mapper.send_mouse_button(&self.outgoing_tx, btn, *pressed);
                    }
                    egui::Event::MouseWheel { delta, .. } => {
                        self.mapper.send_mouse_scroll(
                            &self.outgoing_tx,
                            (delta.x * 120.0) as i16,
                            (delta.y * 120.0) as i16,
                        );
                    }
                    _ => {}
                }
            }

            if let Some(pos) = mouse_pos {
                self.mapper.send_mouse_move(
                    &self.outgoing_tx,
                    pos.x,
                    pos.y,
                    screen_rect.width(),
                    screen_rect.height(),
                );
            }
        }

        // Request repaint to keep event loop alive
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard_tap;

    fn make_app() -> WireDeskApp {
        let (out_tx, _out_rx) = mpsc::channel();
        let (_ev_tx, ev_rx) = mpsc::channel();
        let (tap_tx, tap_rx) = mpsc::channel();
        let swap_flag = Arc::new(AtomicBool::new(false));
        let (synth_tx, _synth_rx) = mpsc::channel();
        let (kick_tx, _kick_rx) = mpsc::channel();
        let tap_handle = keyboard_tap::start(
            out_tx.clone(),
            tap_tx,
            swap_flag.clone(),
            synth_tx,
            kick_tx,
        );
        let outgoing_cancel = Arc::new(AtomicBool::new(false));
        let incoming_cancel = Arc::new(AtomicBool::new(false));
        let cfg = ClientConfig {
            port: "/dev/null".into(),
            ..ClientConfig::default()
        };
        WireDeskApp::new(
            cfg,
            ev_rx,
            out_tx,
            tap_rx,
            tap_handle,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            swap_flag,
            outgoing_cancel,
            incoming_cancel,
        )
    }

    #[test]
    fn chrome_shown_by_default() {
        let app = make_app();
        assert!(app.should_show_chrome());
    }

    #[test]
    fn capturing_hides_chrome() {
        let mut app = make_app();
        app.capturing = true;
        assert!(!app.should_show_chrome());
    }

    #[test]
    fn fullscreen_hides_chrome() {
        let mut app = make_app();
        app.fullscreen = true;
        assert!(!app.should_show_chrome());
    }

    #[test]
    fn shell_open_sets_just_opened_flag() {
        let mut app = make_app();
        assert!(!app.shell_open);
        assert!(!app.shell_just_opened);

        app.shell_open_request();
        assert!(app.shell_open, "shell_open should flip to true");
        assert!(
            app.shell_just_opened,
            "shell_just_opened should be set so the next render frame can grab focus"
        );

        // Idempotency: calling open again while already open must not
        // re-arm the flag (otherwise focus would jump on every redraw
        // that happens to call shell_open_request a second time).
        app.shell_just_opened = false;
        app.shell_open_request();
        assert!(
            !app.shell_just_opened,
            "second shell_open_request while open must NOT re-arm the flag"
        );
    }

    #[test]
    fn permission_state_initialized() {
        // Construct app — permission_granted reflects current TCC state.
        // We don't assert true/false (depends on tester's permission), just
        // that the field exists and is a bool.
        let app = make_app();
        let _: bool = app.permission_granted;
    }

    #[test]
    fn connection_state_display_human_readable() {
        // Display impl is shown in the UI; assert exact strings to catch
        // accidental changes (e.g. translator regressions, copy-paste typos).
        let cases = [
            (ConnectionState::Disconnected, "Not connected"),
            (ConnectionState::Connecting, "Connecting…"),
            (ConnectionState::Connected, "Connected"),
        ];
        for (state, expected) in cases {
            assert_eq!(format!("{state}"), expected, "Display for {state:?}");
        }
    }

    #[test]
    fn status_text_for_each_state() {
        // Pure helper — no UI/IO. Asserts the exact human-readable strings
        // shown in the chrome status row so accidental copy-paste / format
        // drift fails fast.
        let mut app = make_app();

        // Disconnected without a reason — generic fallback.
        app.state = ConnectionState::Disconnected;
        app.status_msg = "ready".into();
        assert_eq!(app.status_text(), "Not connected");

        // Disconnected with a transport reason — propagates to the user.
        app.status_msg = "disconnected: serial: device busy".into();
        assert_eq!(app.status_text(), "Disconnected: serial: device busy");

        // Connecting — current spec uses ellipsis (…).
        app.state = ConnectionState::Connecting;
        assert_eq!(app.status_text(), "Connecting…");

        // Connected — embeds host name and resolution.
        app.state = ConnectionState::Connected;
        app.host_name = "win-host".into();
        app.screen_w = 2560;
        app.screen_h = 1440;
        assert_eq!(app.status_text(), "Connected to win-host (2560×1440)");
    }

    #[test]
    fn permission_steps_has_four_steps() {
        assert_eq!(permission_steps().len(), 4);
    }

    #[test]
    fn permission_steps_first_mentions_system_settings() {
        assert!(permission_steps()[0].contains("System Settings"));
    }

    #[test]
    fn format_progress_active() {
        // Mid-transfer. KB rounding via integer division, percentage as
        // floor((cur*100)/total). 340 KiB / 780 KiB ≈ 43.5% → 43%.
        // Codex C4: label is "Sending clipboard" (generic) — "image" was
        // wrong for text transfers and confused users.
        let s = format_progress("Sending clipboard", 340 * 1024, 780 * 1024).expect("active");
        assert!(s.contains("Sending clipboard"), "action prefix missing: {s}");
        assert!(s.contains("340"), "current KB missing: {s}");
        assert!(s.contains("780"), "total KB missing: {s}");
        assert!(s.contains("43%"), "percentage missing: {s}");
    }

    #[test]
    fn format_progress_idle() {
        // total == 0 → no progress to show.
        assert!(format_progress("Sending clipboard", 0, 0).is_none());
        // total == 0 with stale `current` from a prior transfer should also
        // hide — the writer thread should already have zeroed both, but
        // belt-and-braces against ordering races.
        assert!(format_progress("Sending clipboard", 256, 0).is_none());
    }

    #[test]
    fn format_progress_complete() {
        // Boundary: cur == total exactly → 100%.
        let s = format_progress("Receiving clipboard", 1024, 1024).expect("active");
        assert!(s.contains("100%"), "expected 100% at boundary: {s}");
    }

    #[test]
    fn format_progress_clamps_overshoot() {
        // Defensive: if `current` somehow exceeds `total` (atomic ordering
        // race in tests), don't render >100%.
        let s = format_progress("Sending clipboard", 2048, 1024).expect("active");
        assert!(s.contains("100%"), "overshoot must clamp to 100%: {s}");
    }

    #[test]
    fn format_progress_sub_kb_uses_bytes() {
        // Codex iter3 E5: a short Cmd+C (e.g. 50-byte string) used to
        // render as "0/0 KB" because of integer division. Now totals
        // under 1 KiB render in raw bytes.
        let s = format_progress("Sending clipboard", 25, 50).expect("active");
        assert!(s.contains("25/50 B"), "expected raw bytes for sub-KB: {s}");
        assert!(s.contains("50%"), "percentage missing: {s}");
        assert!(!s.contains("KB"), "must not say KB for sub-KB total: {s}");
    }

    #[test]
    fn format_progress_exactly_1kb_uses_kb() {
        // Boundary: total == 1024 → KB units (the `< 1024` predicate).
        let s = format_progress("Sending clipboard", 512, 1024).expect("active");
        assert!(s.contains("KB"), "1 KiB total must use KB units: {s}");
        assert!(s.contains("0/1 KB"), "expected 0/1 KB at 512/1024: {s}");
    }

    #[test]
    fn progress_ratio_idle_returns_none() {
        assert!(progress_ratio(0, 0).is_none());
        assert!(progress_ratio(256, 0).is_none());
    }

    #[test]
    fn progress_ratio_active_in_unit_range() {
        let r = progress_ratio(256, 1024).expect("active");
        assert!((r - 0.25).abs() < f32::EPSILON, "expected 0.25, got {r}");
    }

    #[test]
    fn progress_ratio_complete_is_one() {
        let r = progress_ratio(1024, 1024).expect("active");
        assert!((r - 1.0).abs() < f32::EPSILON, "expected 1.0, got {r}");
    }

    #[test]
    fn progress_ratio_overshoot_clamped() {
        let r = progress_ratio(2048, 1024).expect("active");
        assert!((r - 1.0).abs() < f32::EPSILON, "overshoot must clamp to 1.0, got {r}");
    }

    #[test]
    fn capture_or_fullscreen_each_hides() {
        let mut app = make_app();
        app.capturing = true;
        app.fullscreen = false;
        assert!(!app.should_show_chrome());
        app.capturing = false;
        app.fullscreen = true;
        assert!(!app.should_show_chrome());
        app.capturing = true;
        app.fullscreen = true;
        assert!(!app.should_show_chrome());
        app.capturing = false;
        app.fullscreen = false;
        assert!(app.should_show_chrome());
    }
}
