use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;

use crate::config::ClientConfig;
use crate::input::mapper::InputMapper;
use crate::keyboard_tap::{self, TapEvent, TapHandle};

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
}

impl WireDeskApp {
    pub fn new(
        initial_config: ClientConfig,
        events_rx: mpsc::Receiver<TransportEvent>,
        outgoing_tx: mpsc::Sender<Packet>,
        tap_events_rx: mpsc::Receiver<TapEvent>,
        tap_handle: TapHandle,
    ) -> Self {
        let runtime_serial_port = initial_config.port.clone();
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
            pending_config: initial_config,
            config_dirty: false,
            save_toast: None,
            available_ports: Vec::new(),
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

    /// Render the editable settings block — only shown in chrome mode.
    /// Mutating any field flips `config_dirty`; Save persists to TOML and
    /// flashes a 3-second toast. Changes don't affect the running session
    /// until the user restarts the binary (per Save+Restart design).
    fn render_settings_panel(&mut self, ui: &mut egui::Ui) {
        let mut dirty = false;
        let mut want_save = false;
        let mut want_reset = false;
        let mut want_refresh_ports = false;
        let available_ports = self.available_ports.clone();

        ui.collapsing("Settings", |ui| {
            let cfg = &mut self.pending_config;

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

            ui.horizontal(|ui| {
                let save_enabled = self.config_dirty || dirty;
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
        let want_active = self.capturing && focused;
        let is_active = self
            .tap_handle
            .as_ref()
            .map(|h| h.is_enabled())
            .unwrap_or(false);
        if want_active != is_active {
            if let Some(h) = self.tap_handle.as_ref() {
                if want_active {
                    h.enable();
                } else {
                    h.disable();
                }
            }
        }
    }

    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
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
        ui.indent("perm_steps", |ui| {
            ui.label("1. Open System Settings → Privacy & Security → Accessibility");
            ui.label("2. Click \"+\" and add the wiredesk-client binary");
            ui.label("3. Toggle the switch ON");
            ui.label("4. Restart WireDesk — required: the tap thread is created at startup");
        });
        ui.add_space(12.0);

        if ui.button("Open System Settings").clicked() {
            #[cfg(target_os = "macos")]
            {
                let _ = std::process::Command::new("open")
                    .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")
                    .spawn();
            }
        }

        ui.add_space(8.0);
        ui.weak(
            "(After granting permission, quit and relaunch wiredesk-client. \
             The window detects the change but the tap won't activate \
             without a fresh process.)",
        );
    }

    /// Info-only screen shown when capture or fullscreen is active. No
    /// clickable elements — just a description of what the app is doing
    /// and the relevant hotkeys.
    fn render_capture_info(&self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.heading("WireDesk — input forwarded to Host");
            ui.add_space(8.0);
            if self.state == ConnectionState::Connected {
                ui.label(format!(
                    "● connected to {} ({}×{})",
                    self.host_name, self.screen_w, self.screen_h
                ));
            } else {
                ui.label("● not connected");
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

        if self.fullscreen {
            ui.add_space(12.0);
            ui.weak("(Cmd+Enter again to exit fullscreen)");
        }
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
                }
                TransportEvent::Disconnected(reason) => {
                    self.state = ConnectionState::Disconnected;
                    self.capturing = false;
                    self.status_msg = format!("disconnected: {reason}");
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
                TapEvent::ToggleFullscreen => {
                    self.toggle_fullscreen(ctx);
                }
            }
        }

        // egui-side hotkeys for the OUT-OF-CAPTURE path. When tap is enabled
        // it consumes these before egui sees them; this branch handles the
        // case where the user presses them without capture being on.
        let (cmd_esc_pressed, cmd_enter_pressed) = ctx.input(|i: &egui::InputState| {
            (
                i.key_pressed(egui::Key::Escape) && i.modifiers.command,
                i.key_pressed(egui::Key::Enter) && i.modifiers.command,
            )
        });
        if cmd_esc_pressed {
            self.toggle_capture();
        }
        if cmd_enter_pressed {
            self.toggle_fullscreen(ctx);
        }

        let show_chrome = self.should_show_chrome();
        let permission_granted = self.permission_granted;

        egui::CentralPanel::default().show(ctx, |ui| {
            if !show_chrome {
                self.render_capture_info(ui);
                return;
            }

            if !permission_granted {
                self.render_permission_screen(ui);
                return;
            }

            ui.horizontal(|ui| {
                // 28px icon next to the heading — branding consistency with
                // the Win host title-bar icon. egui downscales the 1024×1024
                // source on the fly; no separate small asset needed.
                ui.add(
                    egui::Image::new(egui::include_image!(
                        "../../../assets/icon-source.png"
                    ))
                    .fit_to_exact_size(egui::vec2(28.0, 28.0)),
                );
                ui.heading("WireDesk");
            });
            ui.separator();

            // Connection status — large coloured glyph + human-friendly
            // text. The glyph is sized 18pt instead of egui's tiny default
            // so it's readable at a glance, matching the Win-side
            // ImageFrame indicator.
            let status_color = match self.state {
                ConnectionState::Connected => egui::Color32::GREEN,
                ConnectionState::Connecting => egui::Color32::YELLOW,
                ConnectionState::Disconnected => egui::Color32::RED,
            };
            let status_text = self.status_text();
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(
                    egui::RichText::new("\u{25CF}").size(18.0).color(status_color),
                ));
                ui.label(status_text);
            });

            ui.label(format!("Serial: {}", self.runtime_serial_port));
            ui.separator();

            // Capture toggle
            let capture_label = if self.capturing {
                "Input: CAPTURED (Cmd+Esc to release)"
            } else {
                "Input: released"
            };

            let btn_text = if self.capturing { "Release Input" } else { "Capture Input" };
            if ui.button(btn_text).clicked() {
                self.toggle_capture();
            }
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
                        ui.label("● shell open");
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
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY)
                                .hint_text("type a command, Enter to send"),
                        );
                        let enter_pressed = resp.lost_focus()
                            && ui.input(|i: &egui::InputState| i.key_pressed(egui::Key::Enter));
                        if enter_pressed && !self.shell_input.is_empty() {
                            want_send = true;
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
                        let btn = match button {
                            egui::PointerButton::Primary => 0,
                            egui::PointerButton::Secondary => 1,
                            egui::PointerButton::Middle => 2,
                            _ => continue,
                        };
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
        let tap_handle = keyboard_tap::start(out_tx.clone(), tap_tx);
        let cfg = ClientConfig {
            port: "/dev/null".into(),
            ..ClientConfig::default()
        };
        WireDeskApp::new(cfg, ev_rx, out_tx, tap_rx, tap_handle)
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
