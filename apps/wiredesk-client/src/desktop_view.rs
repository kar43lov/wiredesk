use eframe::egui;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use wiredesk_browser_cli::desktop_client::DesktopClient;

/// One outstanding frame, JPEG decoding off the GUI thread, no idle traffic.
pub struct DesktopView {
    pub enabled: bool,
    client: DesktopClient,
    pending: Option<mpsc::Receiver<Result<egui::ColorImage, String>>>,
    texture: Option<egui::TextureHandle>,
    last: Instant,
    status: String,
}
impl DesktopView {
    pub fn new(client: DesktopClient) -> Self {
        Self {
            enabled: false,
            client,
            pending: None,
            texture: None,
            last: Instant::now() - Duration::from_secs(2),
            status: String::new(),
        }
    }
    pub fn start(&mut self) {
        self.enabled = true;
        self.texture = None;
        // Reuse an in-flight fetch across fast close/reopen cycles. Dropping
        // its receiver would leave the worker running and start another one.
        self.last = Instant::now() - Duration::from_secs(2);
        self.status = "Подключение к экрану Windows…".into();
    }
    pub fn frame_size(&self) -> Option<[u16; 2]> {
        self.texture
            .as_ref()
            .map(|t| [t.size()[0] as u16, t.size()[1] as u16])
    }
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    match result {
                        Ok(image) => {
                            self.texture = Some(ui.ctx().load_texture(
                                "windows-desktop",
                                image,
                                egui::TextureOptions::LINEAR,
                            ));
                            self.status =
                                "Экран Windows · Ctrl+Cmd+Enter — выйти · Cmd+Esc — отпустить ввод"
                                    .into();
                        }
                        Err(e) => {
                            self.texture = None;
                            self.status = e;
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.status = "Не удалось получить кадр".into();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.enabled
            && self.pending.is_none()
            && self.last.elapsed() >= Duration::from_secs(1)
            && !ui.ctx().input(|i| i.viewport().minimized.unwrap_or(false))
        {
            self.last = Instant::now();
            let client = self.client.clone();
            let (tx, rx) = mpsc::sync_channel(1);
            self.pending = Some(rx);
            std::thread::spawn(move || {
                let result = (|| {
                    let jpeg = client.frame().map_err(|e| e.to_string())?;
                    let mut reader = image::ImageReader::new(std::io::Cursor::new(jpeg))
                        .with_guessed_format()
                        .map_err(|e| e.to_string())?;
                    let mut limits = image::Limits::default();
                    limits.max_image_width = Some(1280);
                    limits.max_image_height = Some(8192);
                    limits.max_alloc = Some(64 * 1024 * 1024);
                    reader.limits(limits);
                    let image = reader.decode().map_err(|e| e.to_string())?.to_rgb8();
                    Ok(egui::ColorImage::from_rgb(
                        [image.width() as usize, image.height() as usize],
                        image.as_raw(),
                    ))
                })();
                let _ = tx.send(result);
            });
        }
        let rect = ui.available_rect_before_wrap();
        ui.painter().rect_filled(rect, 0.0, egui::Color32::BLACK);
        if let Some(texture) = &self.texture {
            let size = texture.size_vec2();
            let target = image_rect(rect, size);
            ui.put(
                target,
                egui::Image::new(texture)
                    .fit_to_exact_size(target.size())
                    .alt_text("Экран Windows")
                    .sense(egui::Sense::click_and_drag()),
            );
        } else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                &self.status,
                egui::FontId::proportional(16.0),
                egui::Color32::WHITE,
            );
        }
        ui.allocate_rect(rect, egui::Sense::hover());
    }
}

fn image_rect(rect: egui::Rect, size: egui::Vec2) -> egui::Rect {
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    egui::Rect::from_center_size(rect.center(), size * scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_frame_aspect_matches_mouse_mapping_even_when_config_differs() {
        let outer = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let frame = egui::vec2(1280.0, 960.0);
        let picture = image_rect(outer, frame);
        let mut mapper = crate::input::mapper::InputMapper::new(2560, 1440);
        mapper.set_screen_size(frame.x as u16, frame.y as u16);
        assert_eq!(
            mapper.normalize_mouse(picture.left(), picture.top(), outer.width(), outer.height()),
            (0, 0)
        );
        assert_eq!(
            mapper.normalize_mouse(
                picture.right(),
                picture.bottom(),
                outer.width(),
                outer.height()
            ),
            (65535, 65535)
        );
        let center = mapper.normalize_mouse(
            picture.center().x,
            picture.center().y,
            outer.width(),
            outer.height(),
        );
        assert!(center.0.abs_diff(32767) <= 1 && center.1.abs_diff(32767) <= 1);
    }
}
