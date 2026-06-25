//! eframe app: main window with capture + settings UI.

use std::sync::{Arc, Mutex};

use eframe::egui;
use image::RgbaImage;

use crate::capture;
use crate::config::Settings;
use crate::ui::pin_window::PinSession;

/// Visual classification of the status message, so its color doesn't depend on
/// substring matching against the message text (which can include file paths).
#[derive(Clone, Copy, PartialEq)]
enum MsgKind {
    Info,
    Success,
    Error,
}

/// Top-level application state carried across frames.
pub struct ScreenshotDaiApp {
    settings: Settings,
    /// Settings form buffer (edited by the text fields until "Save").
    settings_buf: Settings,
    /// Texture handle for the most recent capture, recreated on each capture.
    texture: Option<egui::TextureHandle>,
    /// Status / message line shown to the user.
    message: String,
    /// Visual classification of `message`.
    message_kind: MsgKind,
    /// Active region-selection overlay session, if any.
    region_session: Option<Arc<Mutex<crate::ui::region_overlay::RegionSession>>>,
    /// Most recent capture (fullscreen OR region), used for "Pin to desktop".
    last_image: Option<RgbaImage>,
    /// Physical px / logical point for `last_image` (the capturing monitor's
    /// scale factor). Used to size pinned windows correctly.
    last_scale: f32,
    /// Active pinned-screenshot sessions.
    pins: Vec<Arc<Mutex<PinSession>>>,
    /// Counter for unique stable pin ids.
    next_pin_id: u64,
}

impl ScreenshotDaiApp {
    pub fn new(settings: Settings) -> Self {
        let settings_buf = settings.clone();
        Self {
            settings,
            settings_buf,
            texture: None,
            message: String::new(),
            message_kind: MsgKind::Info,
            region_session: None,
            last_image: None,
            last_scale: 1.0,
            pins: Vec::new(),
            next_pin_id: 0,
        }
    }

    /// Set the status message and its visual classification.
    fn set_message(&mut self, kind: MsgKind, text: impl Into<String>) {
        self.message = text.into();
        self.message_kind = kind;
    }
}

impl eframe::App for ScreenshotDaiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("screenshot-dai");
            ui.add_space(4.0);

            // --- Capture ---
            ui.label(
                egui::RichText::new("Capture")
                    .text_style(egui::TextStyle::Name("Section".into()))
                    .color(crate::ui::theme::TEXT_SECONDARY),
            );
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui
                    .add(crate::ui::theme::primary_button("Capture Fullscreen"))
                    .clicked()
                {
                    self.message.clear();
                    match capture::capture_monitor_under_cursor() {
                        Ok((img, scale)) => {
                            let path = capture::default_save_path();
                            match capture::save_png(&path, &img) {
                                Ok(()) => {
                                    self.set_message(
                                        MsgKind::Success,
                                        format!("Saved to {}", path.display()),
                                    );
                                    let color_image = capture::rgba_image_to_color_image(&img);
                                    let handle = ui.ctx().load_texture(
                                        "captured",
                                        color_image,
                                        egui::TextureOptions::LINEAR,
                                    );
                                    self.texture = Some(handle);
                                    self.last_image = Some(img.clone());
                                    self.last_scale = scale;
                                }
                                Err(e) => {
                                    self.set_message(MsgKind::Error, format!("Save failed: {e:#}"));
                                }
                            }
                        }
                        Err(e) => {
                            self.set_message(MsgKind::Error, format!("Capture failed: {e:#}"));
                        }
                    }
                }

                if ui
                    .add(crate::ui::theme::secondary_button("Capture Region"))
                    .clicked()
                {
                    match crate::ui::region_overlay::start_session(ui.ctx()) {
                        Ok(s) => {
                            self.region_session = Some(Arc::new(Mutex::new(s)));
                            self.message.clear();
                        }
                        Err(e) => self
                            .set_message(MsgKind::Error, format!("Region capture failed: {e:#}")),
                    }
                }

                if ui
                    .add_enabled(
                        self.last_image.is_some(),
                        crate::ui::theme::secondary_button("Pin to Desktop"),
                    )
                    .clicked()
                {
                    if let Some(img) = &self.last_image {
                        self.next_pin_id += 1;
                        let id = self.next_pin_id;
                        let scale = self.last_scale;
                        let session =
                            crate::ui::pin_window::PinSession::new(ui.ctx(), id, img, scale);
                        self.pins.push(Arc::new(Mutex::new(session)));
                    }
                }
            });

            // Show the overlay while a session is active.
            if let Some(session) = self.region_session.clone() {
                let (origin_logical, size_logical) = {
                    let g = session.lock().expect("poisoned");
                    let ol = egui::pos2(
                        g.origin_physical.0 as f32 / g.scale,
                        g.origin_physical.1 as f32 / g.scale,
                    );
                    (ol, g.size_logical)
                };
                ui.ctx().show_viewport_immediate(
                    egui::ViewportId::from_hash_of("region_overlay"),
                    egui::ViewportBuilder::default()
                        .with_decorations(false)
                        .with_resizable(false)
                        .with_always_on_top()
                        .with_position([origin_logical.x, origin_logical.y])
                        .with_inner_size(size_logical)
                        .with_title("screenshot-dai region"),
                    move |ui, _class| {
                        crate::ui::region_overlay::draw_overlay(ui, &session);
                    },
                );
            }

            // Harvest the result (takes the session out; restores it if not finished).
            let mut new_texture: Option<egui::TextureHandle> = None;
            let mut new_message: Option<(MsgKind, String)> = None;
            let mut new_last_scale: Option<f32> = None;
            let taken = self.region_session.take();
            if let Some(session) = taken {
                let finished = session.lock().expect("poisoned").finished;
                if finished {
                    let (result, session_scale) = {
                        let mut g = session.lock().expect("poisoned");
                        (g.result.take(), g.scale)
                    };
                    match result {
                        Some(crate::ui::region_overlay::RegionResult::Cropped(img)) => {
                            let path = capture::default_save_path();
                            match capture::save_png(&path, &img) {
                                Ok(()) => {
                                    new_message = Some((
                                        MsgKind::Success,
                                        format!("Region saved to {}", path.display()),
                                    ))
                                }
                                Err(e) => {
                                    new_message =
                                        Some((MsgKind::Error, format!("Save failed: {e:#}")))
                                }
                            }
                            let ci = capture::rgba_image_to_color_image(&img);
                            new_texture = Some(ui.ctx().load_texture(
                                "captured",
                                ci,
                                egui::TextureOptions::LINEAR,
                            ));
                            self.last_image = Some(img);
                            new_last_scale = Some(session_scale);
                        }
                        Some(crate::ui::region_overlay::RegionResult::Cancelled) => {
                            new_message =
                                Some((MsgKind::Info, "Region capture cancelled.".to_string()));
                        }
                        None => {
                            // finished flag set without a result — keep the session alive defensively
                            self.region_session = Some(session);
                        }
                    }
                } else {
                    self.region_session = Some(session);
                }
            }
            if let Some(h) = new_texture {
                self.texture = Some(h);
            }
            if let Some((kind, m)) = new_message {
                self.set_message(kind, m);
            }
            if let Some(s) = new_last_scale {
                self.last_scale = s;
            }

            // Show + reap pinned windows.
            for session in &self.pins {
                crate::ui::pin_window::show_pin(ui.ctx(), session);
            }
            self.pins.retain(|s| !s.lock().expect("poisoned").closed);

            if !self.message.is_empty() {
                let msg_color = match self.message_kind {
                    MsgKind::Error => crate::ui::theme::ERROR,
                    MsgKind::Success => crate::ui::theme::SUCCESS,
                    MsgKind::Info => crate::ui::theme::TEXT_SECONDARY,
                };
                ui.label(egui::RichText::new(&self.message).color(msg_color));
            }

            if let Some(handle) = &self.texture {
                ui.add_space(8.0);
                egui::Frame::group(ui.style())
                    .fill(crate::ui::theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0, crate::ui::theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(crate::ui::theme::RADIUS_LG))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Most Recent Capture")
                                .text_style(egui::TextStyle::Name("Caption".into()))
                                .color(crate::ui::theme::TEXT_SECONDARY),
                        );
                        let avail = ui.available_width();
                        let size = handle.size_vec2();
                        let scale = if size.x > 0.0 {
                            (avail / size.x).min(1.0)
                        } else {
                            1.0
                        };
                        ui.image(egui::load::SizedTexture::new(handle.id(), size * scale));
                    });
            } else {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "No capture yet. Press Capture Fullscreen or Capture Region.",
                    )
                    .color(crate::ui::theme::TEXT_SECONDARY),
                );
            }

            ui.add_space(32.0);
            ui.separator();

            // --- Settings ---
            ui.heading("Settings");
            ui.add_space(4.0);
            egui::Grid::new("settings_grid")
                .num_columns(2)
                .spacing([16.0, 10.0])
                .show(ui, |ui| {
                    ui.label("OpenAI base URL");
                    ui.text_edit_singleline(&mut self.settings_buf.openai_base_url);
                    ui.end_row();

                    ui.label("API key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.settings_buf.openai_api_key)
                            .password(true),
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
                if ui
                    .add(crate::ui::theme::secondary_button("Save Settings"))
                    .clicked()
                {
                    match self.settings_buf.save() {
                        Ok(()) => {
                            self.settings = self.settings_buf.clone();
                            self.set_message(MsgKind::Success, "Settings saved.");
                        }
                        Err(e) => {
                            self.set_message(
                                MsgKind::Error,
                                format!("Failed to save settings: {e:#}"),
                            );
                        }
                    }
                }
                if ui.button("Reset").clicked() {
                    self.settings_buf = self.settings.clone();
                }
            });
        });
    }
}
