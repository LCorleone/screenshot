//! Floating pinned-screenshot windows.

use std::sync::{Arc, Mutex};

use eframe::egui;
use image::RgbaImage;

/// A single pinned screenshot floating over the desktop in its own borderless,
/// always-on-top viewport.
pub struct PinSession {
    /// Stable unique id used to derive the pin's `ViewportId`.
    pub id: u64,
    /// egui texture for the captured image, kept alive here.
    pub texture: Option<egui::TextureHandle>,
    /// Display size in logical points.
    pub size_logical: egui::Vec2,
    /// Set true when the user closes the pin; the app reaps it next frame.
    pub closed: bool,
}

impl PinSession {
    /// Create a new pin from a captured image. `scale` is physical px per
    /// logical point (the monitor scale factor at capture time).
    pub fn new(ctx: &egui::Context, id: u64, img: &RgbaImage, scale: f32) -> Self {
        let scale = if scale.is_finite() && scale > 0.0001 {
            scale
        } else {
            1.0
        };
        let color_image = crate::capture::rgba_image_to_color_image(img);
        let handle = ctx.load_texture(
            format!("pin-{id}"),
            color_image,
            egui::TextureOptions::LINEAR,
        );
        let size_logical = egui::Vec2::new(
            (img.width() as f32 / scale).max(1.0),
            (img.height() as f32 / scale).max(1.0),
        );
        Self {
            id,
            texture: Some(handle),
            size_logical,
            closed: false,
        }
    }
}

/// Show one pin as a deferred viewport. Call every frame while the pin exists;
/// when the caller stops invoking this, egui closes the native window.
pub fn show_pin(ctx: &egui::Context, session: &Arc<Mutex<PinSession>>) {
    let (viewport_id, size_logical, pin_id) = {
        let g = session.lock().expect("pin session poisoned");
        (egui::ViewportId::from_hash_of(g.id), g.size_logical, g.id)
    };

    let session2 = session.clone();
    ctx.show_viewport_deferred(
        viewport_id,
        egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_always_on_top()
            .with_resizable(false)
            .with_inner_size(size_logical)
            .with_title("screenshot-dai pin"),
        move |ui, _class| {
            // Close on OS close-request (Alt+F4 / window ✕) or Esc.
            let close = ui
                .ctx()
                .input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape));

            // Body: the image. Drag anywhere with the primary button held to
            // move the native window via StartDrag.
            egui::CentralPanel::default().show_inside(ui, |ui| {
                let (tex_id, size) = {
                    let g = session2.lock().expect("pin session poisoned");
                    let id = g
                        .texture
                        .as_ref()
                        .map(|t| t.id())
                        .unwrap_or(egui::TextureId::Managed(0));
                    (id, g.size_logical)
                };
                ui.image(egui::load::SizedTexture::new(tex_id, size));

                // Drag-to-move: when the user presses-and-drags on the image,
                // hand control to the OS window mover.
                let drag = ui
                    .ctx()
                    .input(|i| i.pointer.is_decidedly_dragging() && i.pointer.primary_down());
                if drag {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }

                // Close button (top-left corner, over the image).
                egui::Area::new(egui::Id::new(("pin-close", pin_id)))
                    .fixed_pos(egui::pos2(2.0, 2.0))
                    .order(egui::Order::Foreground)
                    .show(ui.ctx(), |ui| {
                        if ui.small_button("✕").clicked() {
                            session2.lock().expect("poisoned").closed = true;
                        }
                    });
            });

            if close {
                session2.lock().expect("poisoned").closed = true;
            }
        },
    );
}
