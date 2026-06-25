//! eframe app: main window with capture + settings UI.

use eframe::egui;

use crate::capture;
use crate::config::Settings;

/// Top-level application state carried across frames.
pub struct ScreenshotDaiApp {
    settings: Settings,
    /// Settings form buffer (edited by the text fields until "Save").
    settings_buf: Settings,
    /// Texture handle for the most recent capture, recreated on each capture.
    texture: Option<egui::TextureHandle>,
    /// Status / message line shown to the user.
    message: String,
}

impl ScreenshotDaiApp {
    pub fn new(settings: Settings) -> Self {
        let settings_buf = settings.clone();
        Self {
            settings,
            settings_buf,
            texture: None,
            message: String::new(),
        }
    }
}

impl eframe::App for ScreenshotDaiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.heading("screenshot-dai");
        ui.add_space(4.0);

        // --- Capture ---
        ui.horizontal(|ui| {
            if ui.button("Capture Fullscreen").clicked() {
                self.message.clear();
                match capture::capture_primary_monitor() {
                    Ok(img) => {
                        let path = capture::default_save_path();
                        match capture::save_png(&path, &img) {
                            Ok(()) => {
                                self.message = format!("Saved to {}", path.display());
                                let color_image = capture::rgba_image_to_color_image(&img);
                                let handle = ui.ctx().load_texture(
                                    "captured",
                                    color_image,
                                    egui::TextureOptions::LINEAR,
                                );
                                self.texture = Some(handle);
                            }
                            Err(e) => {
                                self.message = format!("Save failed: {e:#}");
                            }
                        }
                    }
                    Err(e) => {
                        self.message = format!("Capture failed: {e:#}");
                    }
                }
            }
        });

        if !self.message.is_empty() {
            ui.label(&self.message);
        }

        if let Some(handle) = &self.texture {
            ui.add_space(8.0);
            ui.label("Most recent capture:");
            let avail = ui.available_width();
            let size = handle.size_vec2();
            let scale = if size.x > 0.0 {
                (avail / size.x).min(1.0)
            } else {
                1.0
            };
            ui.image(egui::load::SizedTexture::new(handle.id(), size * scale));
        }

        ui.add_space(8.0);
        ui.separator();

        // --- Settings ---
        ui.heading("Settings");
        ui.add_space(4.0);
        egui::Grid::new("settings_grid")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {
                ui.label("OpenAI base URL");
                ui.text_edit_singleline(&mut self.settings_buf.openai_base_url);
                ui.end_row();

                ui.label("API key");
                ui.add(
                    egui::TextEdit::singleline(&mut self.settings_buf.openai_api_key).password(true),
                );
                ui.end_row();

                ui.label("Model");
                ui.text_edit_singleline(&mut self.settings_buf.openai_model);
                ui.end_row();

                ui.label("OCR endpoint");
                ui.text_edit_singleline(&mut self.settings_buf.ocr_endpoint);
                ui.end_row();
            });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Save settings").clicked() {
                match self.settings_buf.save() {
                    Ok(()) => {
                        self.settings = self.settings_buf.clone();
                        self.message = "Settings saved.".to_string();
                    }
                    Err(e) => {
                        self.message = format!("Failed to save settings: {e:#}");
                    }
                }
            }
            if ui.button("Reset").clicked() {
                self.settings_buf = self.settings.clone();
            }
        });
    }
}
