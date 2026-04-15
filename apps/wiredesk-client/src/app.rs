use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use eframe::egui;
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

use crate::input::mapper::InputMapper;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

/// Messages from the transport thread to the UI.
#[allow(dead_code)]
pub enum TransportEvent {
    Connected { host_name: String, screen_w: u16, screen_h: u16 },
    Disconnected(String),
    ClipboardFromHost(String),
    Heartbeat,
}

pub struct WireDeskApp {
    state: ConnectionState,
    capturing: bool,
    host_name: String,
    screen_w: u16,
    screen_h: u16,
    clipboard_text: String,
    status_msg: String,
    serial_port: String,
    events_rx: Option<mpsc::Receiver<TransportEvent>>,
    transport: Option<Arc<Mutex<Box<dyn Transport>>>>,
    mapper: InputMapper,
    seq: u16,
}

impl WireDeskApp {
    pub fn new(
        serial_port: String,
        events_rx: mpsc::Receiver<TransportEvent>,
        transport: Arc<Mutex<Box<dyn Transport>>>,
    ) -> Self {
        Self {
            state: ConnectionState::Disconnected,
            capturing: false,
            host_name: String::new(),
            screen_w: 1920,
            screen_h: 1080,
            clipboard_text: String::new(),
            status_msg: "ready".into(),
            serial_port,
            events_rx: Some(events_rx),
            transport: Some(transport),
            mapper: InputMapper::new(1920, 1080),
            seq: 0,
        }
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    /// Send a sequence of key packets through transport (used for special combos).
    fn send_key_sequence(&mut self, keys: &[(u16, u8, bool)]) {
        if let Some(transport) = self.transport.clone() {
            if let Ok(mut t) = transport.lock() {
                for &(scancode, modifiers, pressed) in keys {
                    let seq = self.next_seq();
                    let msg = if pressed {
                        Message::KeyDown { scancode, modifiers }
                    } else {
                        Message::KeyUp { scancode, modifiers }
                    };
                    let _ = t.send(&Packet::new(msg, seq));
                }
            }
        }
    }

    fn toggle_capture(&mut self) {
        self.capturing = !self.capturing;
        if self.capturing {
            self.status_msg = "input captured (Ctrl+Alt+G to release)".into();
        } else {
            self.status_msg = "input released".into();
        }
    }
}

impl eframe::App for WireDeskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process transport events
        if let Some(ref rx) = self.events_rx {
            while let Ok(event) = rx.try_recv() {
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
                }
            }
        }

        // Check for global hotkey: Ctrl+Alt+G
        let hotkey_pressed = ctx.input(|i: &egui::InputState| {
            i.key_pressed(egui::Key::G)
                && i.modifiers.ctrl
                && i.modifiers.alt
        });
        if hotkey_pressed {
            self.toggle_capture();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("WireDesk");
            ui.separator();

            // Connection status
            let status_color = match self.state {
                ConnectionState::Connected => egui::Color32::GREEN,
                ConnectionState::Connecting => egui::Color32::YELLOW,
                ConnectionState::Disconnected => egui::Color32::RED,
            };
            ui.horizontal(|ui| {
                ui.colored_label(status_color, "\u{25CF}"); // ●
                ui.label(format!("{:?}", self.state));
                if self.state == ConnectionState::Connected {
                    ui.label(format!("- {} ({}x{})", self.host_name, self.screen_w, self.screen_h));
                }
            });

            ui.label(format!("Serial: {}", self.serial_port));
            ui.separator();

            // Capture toggle
            let capture_label = if self.capturing {
                "Input: CAPTURED (Ctrl+Alt+G to release)"
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
            if self.state == ConnectionState::Connected {
                ui.horizontal(|ui| {
                    if ui.button("Ctrl+Alt+Del").clicked() {
                        send_cad = true;
                    }
                    if ui.button("Win key").clicked() {
                        send_win = true;
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

            ui.separator();
            ui.small(&self.status_msg);
        });

        // Handle captured input — collect events first, then send
        if self.capturing && self.state == ConnectionState::Connected {
            // Collect events from egui (no borrow on self)
            let events: Vec<egui::Event> = ctx.input(|input: &egui::InputState| {
                input.events.clone()
            });
            let mouse_pos = ctx.input(|input: &egui::InputState| input.pointer.hover_pos());
            let screen_rect = ctx.screen_rect();

            // Now send through transport (borrows self mutably)
            if let Some(transport) = self.transport.clone() {
                if let Ok(mut t) = transport.lock() {
                    for event in &events {
                        match event {
                            egui::Event::Key { key, pressed, modifiers, .. } => {
                                let _ = self.mapper.send_key(&mut *t, key, modifiers, *pressed);
                            }
                            egui::Event::PointerButton { button, pressed, .. } => {
                                let btn = match button {
                                    egui::PointerButton::Primary => 0,
                                    egui::PointerButton::Secondary => 1,
                                    egui::PointerButton::Middle => 2,
                                    _ => continue,
                                };
                                let _ = self.mapper.send_mouse_button(&mut *t, btn, *pressed);
                            }
                            egui::Event::MouseWheel { delta, .. } => {
                                let _ = self.mapper.send_mouse_scroll(
                                    &mut *t,
                                    (delta.x * 120.0) as i16,
                                    (delta.y * 120.0) as i16,
                                );
                            }
                            _ => {}
                        }
                    }

                    if let Some(pos) = mouse_pos {
                        let _ = self.mapper.send_mouse_move(
                            &mut *t,
                            pos.x,
                            pos.y,
                            screen_rect.width(),
                            screen_rect.height(),
                        );
                    }
                }
            }
        }

        // Request repaint to keep event loop alive
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}
