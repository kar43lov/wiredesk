use eframe::egui;
use wiredesk_core::error::Result;
use wiredesk_protocol::message::Message;
use wiredesk_protocol::packet::Packet;
use wiredesk_transport::transport::Transport;

use super::keymap;

/// Converts egui input events into WireDesk protocol messages and sends them.
pub struct InputMapper {
    host_screen_w: u16,
    host_screen_h: u16,
    seq: u16,
    last_mouse_x: u16,
    last_mouse_y: u16,
}

impl InputMapper {
    pub fn new(host_screen_w: u16, host_screen_h: u16) -> Self {
        Self {
            host_screen_w,
            host_screen_h,
            seq: 0,
            last_mouse_x: 0,
            last_mouse_y: 0,
        }
    }

    pub fn set_screen_size(&mut self, w: u16, h: u16) {
        self.host_screen_w = w;
        self.host_screen_h = h;
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seq;
        self.seq = self.seq.wrapping_add(1);
        s
    }

    /// Normalize mouse position from window coordinates to 0..65535 range,
    /// accounting for aspect ratio difference between window and host screen.
    pub fn normalize_mouse(&self, window_x: f32, window_y: f32, window_w: f32, window_h: f32) -> (u16, u16) {
        if window_w <= 0.0 || window_h <= 0.0 {
            return (0, 0);
        }

        let host_aspect = self.host_screen_w as f32 / self.host_screen_h as f32;
        let window_aspect = window_w / window_h;

        // Fit host screen into window with letterboxing
        let (effective_w, effective_h, offset_x, offset_y) = if window_aspect > host_aspect {
            // Window wider than host → pillarbox (black bars on sides)
            let ew = window_h * host_aspect;
            (ew, window_h, (window_w - ew) / 2.0, 0.0)
        } else {
            // Window taller than host → letterbox (black bars top/bottom)
            let eh = window_w / host_aspect;
            (window_w, eh, 0.0, (window_h - eh) / 2.0)
        };

        let rel_x = ((window_x - offset_x) / effective_w).clamp(0.0, 1.0);
        let rel_y = ((window_y - offset_y) / effective_h).clamp(0.0, 1.0);

        ((rel_x * 65535.0) as u16, (rel_y * 65535.0) as u16)
    }

    pub fn send_mouse_move(
        &mut self,
        transport: &mut dyn Transport,
        window_x: f32,
        window_y: f32,
        window_w: f32,
        window_h: f32,
    ) -> Result<()> {
        let (x, y) = self.normalize_mouse(window_x, window_y, window_w, window_h);

        // Debounce: skip if position hasn't changed
        if x == self.last_mouse_x && y == self.last_mouse_y {
            return Ok(());
        }
        self.last_mouse_x = x;
        self.last_mouse_y = y;

        let seq = self.next_seq();
        transport.send(&Packet::new(Message::MouseMove { x, y }, seq))
    }

    pub fn send_mouse_button(
        &mut self,
        transport: &mut dyn Transport,
        button: u8,
        pressed: bool,
    ) -> Result<()> {
        let seq = self.next_seq();
        transport.send(&Packet::new(Message::MouseButton { button, pressed }, seq))
    }

    pub fn send_mouse_scroll(
        &mut self,
        transport: &mut dyn Transport,
        delta_x: i16,
        delta_y: i16,
    ) -> Result<()> {
        let seq = self.next_seq();
        transport.send(&Packet::new(Message::MouseScroll { delta_x, delta_y }, seq))
    }

    pub fn send_key(
        &mut self,
        transport: &mut dyn Transport,
        key: &egui::Key,
        modifiers: &egui::Modifiers,
        pressed: bool,
    ) -> Result<()> {
        let Some(scancode) = keymap::egui_key_to_scancode(key) else {
            return Ok(());
        };
        let mods = keymap::egui_modifiers_to_u8(modifiers);
        let seq = self.next_seq();
        let msg = if pressed {
            Message::KeyDown { scancode, modifiers: mods }
        } else {
            Message::KeyUp { scancode, modifiers: mods }
        };
        transport.send(&Packet::new(msg, seq))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiredesk_transport::mock::MockTransport;

    #[test]
    fn normalize_same_aspect() {
        let mapper = InputMapper::new(1920, 1080);
        // Window is same aspect ratio
        let (x, y) = mapper.normalize_mouse(480.0, 270.0, 960.0, 540.0);
        assert_eq!(x, 32767); // ~center
        assert_eq!(y, 32767);
    }

    #[test]
    fn normalize_corners() {
        let mapper = InputMapper::new(1920, 1080);
        let (x, y) = mapper.normalize_mouse(0.0, 0.0, 960.0, 540.0);
        assert_eq!(x, 0);
        assert_eq!(y, 0);

        let (x, y) = mapper.normalize_mouse(960.0, 540.0, 960.0, 540.0);
        assert_eq!(x, 65535);
        assert_eq!(y, 65535);
    }

    #[test]
    fn send_key_maps_correctly() {
        let (mut client_t, mut host_t) = MockTransport::pair();
        let mut mapper = InputMapper::new(1920, 1080);

        mapper.send_key(
            &mut client_t,
            &egui::Key::A,
            &egui::Modifiers::default(),
            true,
        ).unwrap();

        let packet = host_t.recv().unwrap();
        match packet.message {
            Message::KeyDown { scancode, modifiers } => {
                assert_eq!(scancode, 0x1E); // 'A' scancode
                assert_eq!(modifiers, 0);
            }
            other => panic!("expected KeyDown, got {other:?}"),
        }
    }

    #[test]
    fn send_key_cmd_becomes_ctrl() {
        let (mut client_t, mut host_t) = MockTransport::pair();
        let mut mapper = InputMapper::new(1920, 1080);

        let mods = egui::Modifiers { command: true, ..Default::default() };
        mapper.send_key(&mut client_t, &egui::Key::C, &mods, true).unwrap();

        let packet = host_t.recv().unwrap();
        match packet.message {
            Message::KeyDown { scancode, modifiers } => {
                assert_eq!(scancode, 0x2E); // 'C'
                assert_eq!(modifiers, 0x01); // CTRL
            }
            other => panic!("expected KeyDown, got {other:?}"),
        }
    }

    #[test]
    fn mouse_debounce() {
        let (mut client_t, mut host_t) = MockTransport::pair();
        let mut mapper = InputMapper::new(1920, 1080);

        mapper.send_mouse_move(&mut client_t, 100.0, 100.0, 1920.0, 1080.0).unwrap();
        // Same position → should be debounced (no second packet)
        mapper.send_mouse_move(&mut client_t, 100.0, 100.0, 1920.0, 1080.0).unwrap();

        let _first = host_t.recv().unwrap();
        // Second recv should block/fail since no second packet was sent
        // (MockTransport uses blocking recv, so we can't easily test this)
    }
}
