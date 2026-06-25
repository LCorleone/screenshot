//! Region-selection overlay: a frozen snapshot of the whole virtual desktop
//! (all monitors) with a draggable crop box. Esc cancels; mouse-up or Enter
//! confirms. The overlay window is positioned at the virtual-screen top-left
//! (`vmin`, possibly negative) and sized to cover the entire virtual desktop,
//! so the existing crop/to_uv math (offset-from-origin) is already correct for
//! the composite image.

use std::sync::{Arc, Mutex};

use eframe::egui;
use egui::{Align2, Color32, Key, Pos2, Rect, Stroke, Vec2};
use image::RgbaImage;

/// Minimum drag size (logical points) below which a selection is ignored.
const MIN_DRAG: f32 = 4.0;

/// Outcome of a finished region session.
pub enum RegionResult {
    /// User confirmed a selection; contains the cropped pixels.
    Cropped(RgbaImage),
    /// User pressed Esc.
    Cancelled,
}

/// Shared state between the main app and the overlay viewport.
pub struct RegionSession {
    /// Full virtual-desktop composite screenshot (origin = `origin_physical`).
    pub full_image: RgbaImage,
    /// egui texture over `full_image`, kept alive here.
    pub texture: Option<egui::TextureHandle>,
    /// Image dims in physical pixels.
    pub image_px: [usize; 2],
    /// Physical pixels per logical point (primary monitor scale factor;
    /// uniform-DPI assumption for the whole virtual desktop).
    pub scale: f32,
    /// Logical on-screen size of the overlay (= virtual desktop in logical pts).
    pub size_logical: Vec2,
    /// Overlay's top-left in physical virtual-screen pixels (= `vmin`).
    /// (0,0) for a single primary monitor; may be negative on multi-monitor.
    pub origin_physical: (i32, i32),
    /// Drag origin in logical overlay coords while dragging.
    pub drag_start: Option<Pos2>,
    /// Current pointer in logical overlay coords while dragging.
    pub drag_cur: Option<Pos2>,
    /// Hover-snap rect (logical) updated each frame while not dragging.
    pub hover_rect: Option<Rect>,
    /// Set when the session is done.
    pub finished: bool,
    /// Result once finished.
    pub result: Option<RegionResult>,
}

impl RegionSession {
    pub fn new(full_image: RgbaImage, scale: f32, size_logical: Vec2) -> Self {
        let image_px = [full_image.width() as usize, full_image.height() as usize];
        Self {
            full_image,
            texture: None,
            image_px,
            scale,
            size_logical,
            origin_physical: (0, 0),
            drag_start: None,
            drag_cur: None,
            hover_rect: None,
            finished: false,
            result: None,
        }
    }

    /// Current selection rect in logical coords, if any.
    fn current_rect(&self) -> Option<Rect> {
        match (self.drag_start, self.drag_cur) {
            (Some(a), Some(b)) => Some(Rect::from_two_pos(a, b)),
            _ => None,
        }
    }
}

/// Capture the full virtual desktop and build a ready-to-show session.
pub fn start_session(ctx: &egui::Context) -> anyhow::Result<RegionSession> {
    let (img, vmin, vsize) = crate::capture::capture_virtual_desktop()?;
    // scale = primary monitor's scale factor (uniform-DPI assumption).
    let scale = crate::capture::primary_monitor()
        .ok()
        .and_then(|m| m.scale_factor().ok())
        .unwrap_or(1.0)
        .max(0.0001);
    let size_logical = Vec2::new(
        (vsize.0 as f32 / scale).max(1.0),
        (vsize.1 as f32 / scale).max(1.0),
    );
    let mut session = RegionSession::new(img, scale, size_logical);
    session.origin_physical = vmin;
    let color_image = crate::capture::rgba_image_to_color_image(&session.full_image);
    let handle = ctx.load_texture("region-bg", color_image, egui::TextureOptions::LINEAR);
    session.texture = Some(handle);
    Ok(session)
}

/// Draw the overlay into its viewport `ui` and update `session`.
pub fn draw_overlay(ui: &mut egui::Ui, session: &Arc<Mutex<RegionSession>>) {
    let screen = ui.ctx().content_rect();

    // Read-only pass: gather what we need to paint. Also update the live
    // hover-snap rect while the user is NOT dragging.
    let (tex_id, image_px, scale, cur_sel, hover_rect) = {
        let mut g = session.lock().expect("region session poisoned");
        // Hover-snap is only live before a drag begins.
        let hr = if g.drag_start.is_none() {
            let pos = ui.ctx().input(|i| i.pointer.latest_pos());
            pos.and_then(|p| {
                crate::platform::window_rect_at((p.x, p.y), g.origin_physical, g.scale)
                    .map(|(x, y, w, h)| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)))
            })
        } else {
            None
        };
        g.hover_rect = hr;
        let id = g
            .texture
            .as_ref()
            .map(|t| t.id())
            .unwrap_or(egui::TextureId::Managed(0));
        (id, g.image_px, g.scale, g.current_rect(), hr)
    };

    let painter = ui.painter().clone();

    // 1. frozen screenshot filling the overlay
    painter.image(
        tex_id,
        screen,
        Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
        Color32::WHITE,
    );
    // 2. dim everything
    painter.rect_filled(screen, 0.0, Color32::from_black_alpha(140));

    // 3. spotlight + label. While not dragging, a live hover-snap rect
    //    (if any) acts as the preview selection.
    let preview = cur_sel.or(hover_rect);
    if let Some(sel) = preview {
        let uv = to_uv(sel, screen.min, scale, image_px);
        painter.image(tex_id, sel, uv, Color32::WHITE);
        // Active drag selection gets a white border; a pure hover preview
        // gets a softer accent border to signal it isn't confirmed yet.
        let stroke = if cur_sel.is_some() {
            Stroke::new(2.0, Color32::WHITE)
        } else {
            Stroke::new(2.0, Color32::from_rgb(120, 180, 255))
        };
        painter.rect_stroke(sel, 0.0, stroke, egui::epaint::StrokeKind::Inside);
        let pw = ((sel.max.x - sel.min.x) * scale).round() as i32;
        let ph = ((sel.max.y - sel.min.y) * scale).round() as i32;
        painter.debug_text(
            sel.min + Vec2::new(4.0, -4.0),
            Align2::LEFT_BOTTOM,
            Color32::WHITE,
            format!("{} × {}", pw, ph),
        );
    } else {
        painter.debug_text(
            screen.center(),
            Align2::CENTER_CENTER,
            Color32::WHITE,
            "Drag to select · click a window to snap · Esc to cancel",
        );
    }

    // 4. input handling (mutable pass)
    let mut g = session.lock().expect("region session poisoned");
    if g.finished {
        return;
    }
    let (primary_down, latest, esc, enter, released, close_requested) = ui.ctx().input(|i| {
        (
            i.pointer.primary_down(),
            i.pointer.latest_pos(),
            i.key_pressed(Key::Escape),
            i.key_pressed(Key::Enter),
            i.pointer.primary_released(),
            i.viewport().close_requested(),
        )
    });

    if esc || close_requested {
        g.drag_start = None;
        g.drag_cur = None;
        g.finished = true;
        g.result = Some(RegionResult::Cancelled);
        return;
    }

    if enter {
        if let Some(r) = g.current_rect() {
            if (r.max.x - r.min.x).abs() > MIN_DRAG && (r.max.y - r.min.y).abs() > MIN_DRAG {
                if let Some(cropped) = crop(&g.full_image, r, screen.min, g.scale) {
                    g.finished = true;
                    g.result = Some(RegionResult::Cropped(cropped));
                    return;
                }
            }
        }
    }

    if primary_down {
        if g.drag_start.is_none() {
            g.drag_start = latest;
        }
        g.drag_cur = latest;
    } else if released {
        let dragged = g
            .current_rect()
            .map(|r| (r.max.x - r.min.x).abs() > MIN_DRAG && (r.max.y - r.min.y).abs() > MIN_DRAG)
            .unwrap_or(false);
        if dragged {
            // Free-form drag selection: confirm on release.
            if let Some(r) = g.current_rect() {
                if let Some(cropped) = crop(&g.full_image, r, screen.min, g.scale) {
                    g.finished = true;
                    g.result = Some(RegionResult::Cropped(cropped));
                    g.drag_start = None;
                    g.drag_cur = None;
                    return;
                }
            }
        } else {
            // Click without a drag: snap to the window under the cursor at
            // release. Recompute fresh — `g.hover_rect` is stale by this frame
            // (the read pass clears it once drag_start became Some on press).
            let hr = latest.and_then(|p| {
                crate::platform::window_rect_at((p.x, p.y), g.origin_physical, g.scale)
                    .map(|(x, y, w, h)| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)))
            });
            if let Some(hr) = hr {
                if let Some(cropped) = crop(&g.full_image, hr, screen.min, g.scale) {
                    g.finished = true;
                    g.result = Some(RegionResult::Cropped(cropped));
                    g.drag_start = None;
                    g.drag_cur = None;
                    return;
                }
            }
        }
        g.drag_start = None;
        g.drag_cur = None;
    }
}

/// Map a logical selection rect to image UV coordinates.
fn to_uv(sel: Rect, origin: Pos2, scale: f32, image_px: [usize; 2]) -> Rect {
    let (iw, ih) = (image_px[0] as f32, image_px[1] as f32);
    let x0 = (((sel.min.x - origin.x) * scale).clamp(0.0, iw)) / iw;
    let y0 = (((sel.min.y - origin.y) * scale).clamp(0.0, ih)) / ih;
    let x1 = (((sel.max.x - origin.x) * scale).clamp(0.0, iw)) / iw;
    let y1 = (((sel.max.y - origin.y) * scale).clamp(0.0, ih)) / ih;
    Rect::from_min_max(Pos2::new(x0, y0), Pos2::new(x1, y1))
}

/// Crop the full image to the logical selection rect, returning physical pixels.
fn crop(full: &RgbaImage, sel: Rect, origin: Pos2, scale: f32) -> Option<RgbaImage> {
    let (iw, ih) = (full.width() as i32, full.height() as i32);
    let x = ((sel.min.x - origin.x) * scale).round() as i32;
    let y = ((sel.min.y - origin.y) * scale).round() as i32;
    let w = ((sel.max.x - sel.min.x) * scale).round() as i32;
    let h = ((sel.max.y - sel.min.y) * scale).round() as i32;
    let x = x.clamp(0, iw);
    let y = y.clamp(0, ih);
    if x >= iw || y >= ih {
        return None;
    }
    let w = w.min(iw - x).max(1);
    let h = h.min(ih - y).max(1);
    Some(image::imageops::crop_imm(full, x as u32, y as u32, w as u32, h as u32).to_image())
}
