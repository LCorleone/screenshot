//! Region-selection overlay + in-place editor.
//!
//! Two phases share one fullscreen viewport:
//! - [`Phase::Select`]: a frozen snapshot of the whole virtual desktop (all
//!   monitors) with a draggable crop box. Esc cancels; mouse-up (real drag),
//!   click-to-snap, or Enter confirms and transitions to `Edit`.
//! - [`Phase::Edit`]: the cropped selection becomes the editable canvas, shown
//!   at full brightness in its original on-screen position (the rest of the
//!   overlay stays dimmed — a spotlight). A floating toolbar offers the
//!   Rectangle annotation tool plus terminal actions (Pin/Save/Copy/Cancel).
//!
//! The overlay window is positioned at the virtual-screen top-left (`vmin`,
//! possibly negative) and sized to cover the entire virtual desktop, so the
//! existing crop/to_uv math (offset-from-origin) is already correct for the
//! composite image.
//!
//! Coordinate model: the overlay covers the full virtual desktop; local (0,0)
//! corresponds to `origin_physical` in screen space. The selection rect `sel`
//! is in overlay-local logical points. The editor document =
//! `crop(full_image, sel, origin=screen.min, scale)`; it is painted back at the
//! rect `sel` (its original position), so a logical point `(lx,ly)` relative to
//! `sel.min` maps to doc physical `(lx*scale, ly*scale)`.

use std::sync::{Arc, Mutex};

use eframe::egui;
use egui::{Align2, Color32, Key, Pos2, Rect, Stroke, Vec2};
use image::RgbaImage;

/// Minimum drag size (logical points) below which a selection or a rectangle
/// stroke is ignored.
const MIN_DRAG: f32 = 4.0;
/// Maximum number of full-doc snapshots kept for undo.
/// Max number of full-doc snapshots kept for undo. Each snapshot is a full
/// physical-resolution `RgbaImage`, so this is also bounded by
/// [`UNDO_BYTES_BUDGET`] (whichever binds first). Kept modest to bound memory
/// on large (e.g. 4K) captures.
const UNDO_CAP: usize = 10;
/// Soft byte budget for the undo stack (~256 MB). Oldest snapshots are dropped
/// once exceeded, in addition to the [`UNDO_CAP`] count limit.
const UNDO_BYTES_BUDGET: usize = 256 * 1024 * 1024;

/// Overlay phase.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Drag to select / hover-snap a window / Enter to confirm.
    Select,
    /// The cropped selection is the editable canvas + floating toolbar.
    Edit,
}

/// Active annotation tool. E1 ships only the Rectangle (a vertical slice of the
/// tool pipeline); the default is no tool.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// No annotation tool active (pointer does nothing on the canvas).
    None,
    /// Rectangle outline tool.
    Rect,
}

/// Terminal outcome produced by the editor. The app harvests one of these when
/// `finished` becomes true and the overlay closes.
pub enum EditorOutcome {
    /// Float the (edited) document as a pinned window.
    Pin(RgbaImage),
    /// App opens an rfd save dialog and writes the document as PNG.
    Save(RgbaImage),
    /// App copies the document to the system clipboard.
    Copy(RgbaImage),
    /// No action (Esc / Cancel button / closed).
    Cancel,
}

/// Toolbar action collected from the floating toolbar's button clicks, applied
/// during the mutable input pass. Decoupling click detection (inside the
/// `egui::Area` closure) from state mutation (inside the brief session lock)
/// keeps mutex hold times short.
#[derive(Clone, Copy)]
enum ToolbarAction {
    New,
    ToggleRect,
    Undo,
    Pin,
    Save,
    Copy,
    Cancel,
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

    /// Current phase (Select or Edit).
    pub phase: Phase,

    // --- Select-phase interaction state ---
    /// Drag origin in logical overlay coords while dragging a selection.
    pub drag_start: Option<Pos2>,
    /// Current pointer in logical overlay coords while dragging a selection.
    pub drag_cur: Option<Pos2>,
    /// Hover-snap rect (logical) updated each frame while not dragging.
    pub hover_rect: Option<Rect>,

    // --- Edit-phase state ---
    /// Active annotation tool.
    pub active_tool: Tool,
    /// The cropped document (editor canvas), in physical pixels.
    pub doc: Option<RgbaImage>,
    /// egui texture over `doc`; re-uploaded only when the doc changes.
    pub doc_texture: Option<egui::TextureHandle>,
    /// The selection rect (= on-screen position of the doc), overlay-local
    /// logical. The doc is painted at this rect.
    pub doc_rect: Option<Rect>,
    /// Full-doc snapshots for undo (cap [`UNDO_CAP`]).
    pub undo_stack: Vec<RgbaImage>,
    /// Rectangle-tool drag origin (overlay-local logical), while drawing.
    pub rect_drag_start: Option<Pos2>,
    /// Rectangle-tool current pointer (overlay-local logical), while drawing.
    pub rect_drag_cur: Option<Pos2>,

    /// Set when the session is done.
    pub finished: bool,
    /// Result once finished.
    pub result: Option<EditorOutcome>,
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
            phase: Phase::Select,
            drag_start: None,
            drag_cur: None,
            hover_rect: None,
            active_tool: Tool::None,
            doc: None,
            doc_texture: None,
            doc_rect: None,
            undo_stack: Vec::new(),
            rect_drag_start: None,
            rect_drag_cur: None,
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

    /// Re-create the doc texture from `doc` (call after any doc change: commit,
    /// undo, or entering edit). Replaces the old handle; the previous handle
    /// drops once no longer referenced.
    fn refresh_doc_texture(&mut self, ctx: &egui::Context) {
        if let Some(doc) = &self.doc {
            let color_image = crate::capture::rgba_image_to_color_image(doc);
            self.doc_texture =
                Some(ctx.load_texture("editor-doc", color_image, egui::TextureOptions::LINEAR));
        } else {
            self.doc_texture = None;
        }
    }

    /// Commit a mutated document: snapshot the *current* (pre-mutation) doc onto
    /// the undo stack (cap [`UNDO_CAP`]), install `new_doc`, and refresh the
    /// texture. Ordering guarantees undo restores the pre-edit state.
    fn commit_doc(&mut self, ctx: &egui::Context, new_doc: RgbaImage) {
        if let Some(old) = self.doc.take() {
            self.undo_stack.push(old);
            // Bound by both count and total bytes; drop oldest when exceeded.
            while self.undo_stack.len() > UNDO_CAP {
                self.undo_stack.remove(0);
            }
            while self.undo_bytes() > UNDO_BYTES_BUDGET && self.undo_stack.len() > 1 {
                self.undo_stack.remove(0);
            }
        }
        self.doc = Some(new_doc);
        self.refresh_doc_texture(ctx);
    }

    /// Total bytes currently held in the undo stack.
    fn undo_bytes(&self) -> usize {
        self.undo_stack.iter().map(|img| img.len()).sum()
    }

    /// Transition from `Select` to `Edit`: crop the selection, make it the doc,
    /// upload its texture, and clear select-phase interaction state. `origin`
    /// is the overlay content rect's min (= `Pos2::ZERO` for this viewport).
    /// If the crop is empty the phase is left unchanged.
    fn enter_edit(&mut self, ctx: &egui::Context, sel: Rect, origin: Pos2) {
        let Some(cropped) = crop(&self.full_image, sel, origin, self.scale) else {
            return;
        };
        self.doc = Some(cropped);
        self.doc_rect = Some(sel);
        self.undo_stack.clear();
        self.rect_drag_start = None;
        self.rect_drag_cur = None;
        self.active_tool = Tool::None;
        self.hover_rect = None;
        self.drag_start = None;
        self.drag_cur = None;
        self.phase = Phase::Edit;
        self.refresh_doc_texture(ctx);
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

/// Snapshot of session state gathered in the read pass, so the painter and the
/// toolbar closure can work without holding the session lock.
struct ReadData {
    phase: Phase,
    tex_id: egui::TextureId,
    image_px: [usize; 2],
    scale: f32,
    cur_sel: Option<Rect>,
    hover_rect: Option<Rect>,
    doc_rect: Option<Rect>,
    doc_tex_id: Option<egui::TextureId>,
    active_tool: Tool,
    rect_drag_start: Option<Pos2>,
    rect_drag_cur: Option<Pos2>,
    undo_available: bool,
    finished: bool,
}

/// Draw the overlay into its viewport `ui` and update `session`.
pub fn draw_overlay(ui: &mut egui::Ui, session: &Arc<Mutex<RegionSession>>) {
    let screen = ui.ctx().content_rect();
    let ctx = ui.ctx().clone();

    // --- READ PASS: gather paint data + update the live hover-snap rect.
    //     The hover `latest_pos` read goes through `ctx.input`, so it is done
    //     outside the session lock; only the brief hover_rect write is locked.
    let pos_latest = if session.lock().expect("region session poisoned").phase == Phase::Select {
        ctx.input(|i| i.pointer.latest_pos())
    } else {
        None
    };
    let r: ReadData = {
        let mut g = session.lock().expect("region session poisoned");
        let hr = if g.phase == Phase::Select && g.drag_start.is_none() {
            pos_latest.and_then(|p| {
                crate::platform::window_rect_at((p.x, p.y), g.origin_physical, g.scale)
                    .map(|(x, y, w, h)| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)))
            })
        } else {
            None
        };
        g.hover_rect = hr;
        ReadData {
            phase: g.phase,
            tex_id: g
                .texture
                .as_ref()
                .map(|t| t.id())
                .unwrap_or(egui::TextureId::Managed(0)),
            image_px: g.image_px,
            scale: g.scale,
            cur_sel: g.current_rect(),
            hover_rect: hr,
            doc_rect: g.doc_rect,
            doc_tex_id: g.doc_texture.as_ref().map(|t| t.id()),
            active_tool: g.active_tool,
            rect_drag_start: g.rect_drag_start,
            rect_drag_cur: g.rect_drag_cur,
            undo_available: !g.undo_stack.is_empty(),
            finished: g.finished,
        }
    };

    if r.finished {
        return;
    }

    let painter = ui.painter().clone();
    let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));

    // --- PAINT ---
    match r.phase {
        Phase::Select => {
            // 1. frozen screenshot + 2. dim everything
            painter.image(r.tex_id, screen, full_uv, Color32::WHITE);
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(140));

            // 3. spotlight + label. While not dragging, a live hover-snap rect
            //    (if any) acts as the preview selection.
            let preview = r.cur_sel.or(r.hover_rect);
            if let Some(sel) = preview {
                let uv = to_uv(sel, screen.min, r.scale, r.image_px);
                painter.image(r.tex_id, sel, uv, Color32::WHITE);
                // Active drag selection gets a white border; a pure hover
                // preview gets a softer accent border.
                let stroke = if r.cur_sel.is_some() {
                    Stroke::new(2.0, Color32::WHITE)
                } else {
                    Stroke::new(2.0, crate::ui::theme::ACCENT_BLUE_BRIGHT)
                };
                painter.rect_stroke(sel, 0.0, stroke, egui::epaint::StrokeKind::Inside);
                let pw = ((sel.max.x - sel.min.x) * r.scale).round() as i32;
                let ph = ((sel.max.y - sel.min.y) * r.scale).round() as i32;
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
        }
        Phase::Edit => {
            // Dimmed full screenshot, then the doc at full brightness in its
            // original position (spotlight).
            painter.image(r.tex_id, screen, full_uv, Color32::WHITE);
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(140));
            if let (Some(doc_tex), Some(drect)) = (r.doc_tex_id, r.doc_rect) {
                painter.image(doc_tex, drect, full_uv, Color32::WHITE);
                painter.rect_stroke(
                    drect,
                    0.0,
                    Stroke::new(1.0, crate::ui::theme::ACCENT_BLUE_BRIGHT),
                    egui::epaint::StrokeKind::Inside,
                );
            }

            // Live rectangle-tool preview (painter only; no doc mutation yet).
            // Matches the rasterizer: dark outer band, white inner band inset
            // by ~t_dark logical px so the white outline sits inside the dark.
            if r.active_tool == Tool::Rect {
                if let (Some(ds), Some(dc)) = (r.rect_drag_start, r.rect_drag_cur) {
                    let rect = Rect::from_two_pos(ds, dc);
                    painter.rect_stroke(
                        rect,
                        0.0,
                        Stroke::new(3.0, Color32::from_black_alpha(220)),
                        egui::epaint::StrokeKind::Inside,
                    );
                    // ~1 logical pt inset ~= t_dark at scale 1.0 (good enough
                    // for a live preview; the rasterizer is the source of truth).
                    let inner = rect.shrink2(Vec2::splat(1.0));
                    painter.rect_stroke(
                        inner,
                        0.0,
                        Stroke::new(2.0, Color32::WHITE),
                        egui::epaint::StrokeKind::Inside,
                    );
                }
            }
        }
    }

    // --- TOOLBAR (Edit phase only) ---
    let mut action: Option<ToolbarAction> = None;
    if r.phase == Phase::Edit {
        if let Some(sel) = r.doc_rect {
            let active_tool = r.active_tool;
            let undo_available = r.undo_available;
            let pos = toolbar_position(sel, screen);
            egui::Area::new(egui::Id::new("editor_toolbar"))
                .order(egui::Order::Foreground)
                .fixed_pos(pos)
                .show(&ctx, |ui| {
                    egui::Frame::group(ui.style())
                        .fill(crate::ui::theme::SURFACE)
                        .stroke(egui::Stroke::new(1.0, crate::ui::theme::BORDER))
                        .corner_radius(egui::CornerRadius::same(crate::ui::theme::RADIUS_SM))
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.set_min_height(36.0);
                            ui.horizontal(|ui| {
                                if ui.add(crate::ui::theme::secondary_button("New")).clicked() {
                                    action = Some(ToolbarAction::New);
                                }
                                let rect_btn = if active_tool == Tool::Rect {
                                    egui::Button::new(
                                        egui::RichText::new("Rect").color(Color32::BLACK),
                                    )
                                    .fill(crate::ui::theme::ACCENT_BLUE_BRIGHT)
                                    .corner_radius(egui::CornerRadius::same(
                                        crate::ui::theme::RADIUS_SM,
                                    ))
                                    .min_size(egui::vec2(0.0, 36.0))
                                } else {
                                    crate::ui::theme::secondary_button("Rect")
                                };
                                if ui.add(rect_btn).clicked() {
                                    action = Some(ToolbarAction::ToggleRect);
                                }
                                if ui
                                    .add_enabled(
                                        undo_available,
                                        crate::ui::theme::secondary_button("Undo"),
                                    )
                                    .clicked()
                                {
                                    action = Some(ToolbarAction::Undo);
                                }
                                ui.separator();
                                if ui.add(crate::ui::theme::secondary_button("Pin")).clicked() {
                                    action = Some(ToolbarAction::Pin);
                                }
                                if ui.add(crate::ui::theme::primary_button("Save")).clicked() {
                                    action = Some(ToolbarAction::Save);
                                }
                                if ui.add(crate::ui::theme::secondary_button("Copy")).clicked() {
                                    action = Some(ToolbarAction::Copy);
                                }
                                if ui
                                    .add(crate::ui::theme::secondary_button("Cancel"))
                                    .clicked()
                                {
                                    action = Some(ToolbarAction::Cancel);
                                }
                            });
                        });
                });
        }
    }

    // --- INPUT PASS (mutable) ---
    // Read raw pointer/key state WITHOUT the session lock, so the lock is held
    // only briefly while mutating.
    let (primary_down, latest, esc, enter, released, close_requested) = ctx.input(|i| {
        (
            i.pointer.primary_down(),
            i.pointer.latest_pos(),
            i.key_pressed(Key::Escape),
            i.key_pressed(Key::Enter),
            i.pointer.primary_released(),
            i.viewport().close_requested(),
        )
    });

    let mut g = session.lock().expect("region session poisoned");
    if g.finished {
        return;
    }

    // Esc / window-close → Cancel (both phases).
    if esc || close_requested {
        g.drag_start = None;
        g.drag_cur = None;
        g.rect_drag_start = None;
        g.rect_drag_cur = None;
        g.finished = true;
        g.result = Some(EditorOutcome::Cancel);
        return;
    }

    match g.phase {
        Phase::Select => {
            // Enter confirms the current drag selection (if it's real).
            if enter {
                if let Some(sel) = g.current_rect() {
                    if (sel.max.x - sel.min.x).abs() > MIN_DRAG
                        && (sel.max.y - sel.min.y).abs() > MIN_DRAG
                    {
                        g.enter_edit(&ctx, sel, screen.min);
                    }
                }
                g.drag_start = None;
                g.drag_cur = None;
                if g.phase == Phase::Edit {
                    return;
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
                    .map(|r| {
                        (r.max.x - r.min.x).abs() > MIN_DRAG && (r.max.y - r.min.y).abs() > MIN_DRAG
                    })
                    .unwrap_or(false);
                if dragged {
                    // Free-form drag selection: enter the editor on release.
                    if let Some(sel) = g.current_rect() {
                        g.enter_edit(&ctx, sel, screen.min);
                        g.drag_start = None;
                        g.drag_cur = None;
                        return;
                    }
                } else {
                    // Click without a drag: snap to the window under the cursor
                    // at release. Recompute fresh — `g.hover_rect` is stale by
                    // this frame (the read pass clears it once drag_start became
                    // Some on press).
                    let hr = latest.and_then(|p| {
                        crate::platform::window_rect_at((p.x, p.y), g.origin_physical, g.scale).map(
                            |(x, y, w, h)| Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)),
                        )
                    });
                    if let Some(hr) = hr {
                        g.enter_edit(&ctx, hr, screen.min);
                        g.drag_start = None;
                        g.drag_cur = None;
                        return;
                    }
                }
                g.drag_start = None;
                g.drag_cur = None;
            }
        }
        Phase::Edit => {
            // 1. Apply a toolbar action, if any.
            match action {
                Some(ToolbarAction::New) => {
                    // Back to select; KEEP the same full_image (no re-capture).
                    g.phase = Phase::Select;
                    g.doc = None;
                    g.doc_texture = None;
                    g.doc_rect = None;
                    g.undo_stack.clear();
                    g.rect_drag_start = None;
                    g.rect_drag_cur = None;
                    g.active_tool = Tool::None;
                    g.drag_start = None;
                    g.drag_cur = None;
                    g.hover_rect = None;
                    return;
                }
                Some(ToolbarAction::ToggleRect) => {
                    g.active_tool = if g.active_tool == Tool::Rect {
                        Tool::None
                    } else {
                        Tool::Rect
                    };
                    g.rect_drag_start = None;
                    g.rect_drag_cur = None;
                }
                Some(ToolbarAction::Undo) => {
                    if let Some(prev) = g.undo_stack.pop() {
                        g.doc = Some(prev);
                        g.refresh_doc_texture(&ctx);
                    }
                }
                Some(ToolbarAction::Pin) => {
                    if let Some(img) = g.doc.clone() {
                        g.finished = true;
                        g.result = Some(EditorOutcome::Pin(img));
                    }
                    return;
                }
                Some(ToolbarAction::Save) => {
                    if let Some(img) = g.doc.clone() {
                        g.finished = true;
                        g.result = Some(EditorOutcome::Save(img));
                    }
                    return;
                }
                Some(ToolbarAction::Copy) => {
                    if let Some(img) = g.doc.clone() {
                        g.finished = true;
                        g.result = Some(EditorOutcome::Copy(img));
                    }
                    return;
                }
                Some(ToolbarAction::Cancel) => {
                    g.finished = true;
                    g.result = Some(EditorOutcome::Cancel);
                    return;
                }
                None => {}
            }

            // 2. Rectangle-tool pointer interaction on the canvas.
            if g.active_tool == Tool::Rect {
                if let Some(drect) = g.doc_rect {
                    if primary_down {
                        if let Some(p) = latest {
                            if drect.contains(p) {
                                if g.rect_drag_start.is_none() {
                                    g.rect_drag_start = Some(p);
                                }
                                g.rect_drag_cur = Some(p);
                            }
                        }
                    } else if released {
                        if let (Some(ds), Some(dc)) = (g.rect_drag_start, g.rect_drag_cur) {
                            let drag =
                                (dc.x - ds.x).abs() > MIN_DRAG && (dc.y - ds.y).abs() > MIN_DRAG;
                            if drag {
                                if let Some(base) = g.doc.clone() {
                                    let mut nd = base;
                                    rasterize_rect(&mut nd, drect.min, ds, dc, g.scale);
                                    g.commit_doc(&ctx, nd);
                                }
                            }
                            g.rect_drag_start = None;
                            g.rect_drag_cur = None;
                        }
                    }
                }
            }
        }
    }
}

/// Pick a toolbar position just outside the selection's bottom-right corner,
/// flipping to the opposite side (above-left) when it would run off-screen.
/// Uses a rough toolbar size estimate; it only needs to be approximately right.
fn toolbar_position(sel: Rect, screen: Rect) -> Pos2 {
    // 7 buttons * ~56pt each, ~48pt tall with padding.
    let tb_w = 7.0 * 56.0;
    let tb_h = 48.0;
    let mut pos = sel.max + Vec2::new(4.0, 4.0);
    if pos.x + tb_w > screen.max.x {
        pos.x = (sel.min.x - tb_w - 4.0).max(screen.min.x);
    }
    if pos.y + tb_h > screen.max.y {
        pos.y = (sel.min.y - tb_h - 4.0).max(screen.min.y);
    }
    pos
}

/// Rasterize a rectangle stroke onto `doc` (physical pixels). `sel_min_logical`
/// is the doc's top-left in overlay-local logical coords; `drag_start` /
/// `drag_cur` are the stroke's corners in the same space. The stroke is a
/// white ~2-logical-px outline with a thin (~1px) dark outline behind it for
/// contrast on light backgrounds.
fn rasterize_rect(
    doc: &mut RgbaImage,
    sel_min_logical: Pos2,
    drag_start: Pos2,
    drag_cur: Pos2,
    scale: f32,
) {
    let to_px = |p: Pos2| -> (i32, i32) {
        (
            ((p.x - sel_min_logical.x) * scale).round() as i32,
            ((p.y - sel_min_logical.y) * scale).round() as i32,
        )
    };
    let (ax, ay) = to_px(drag_start);
    let (bx, by) = to_px(drag_cur);
    let rx0 = ax.min(bx);
    let ry0 = ay.min(by);
    let rx1 = ax.max(bx);
    let ry1 = ay.max(by);

    let t_white = ((2.0 * scale).round() as i32).max(1);
    let t_dark = ((1.0 * scale).round() as i32).max(1);

    // Dark outer band first...
    fill_border_ring(doc, rx0, ry0, rx1, ry1, t_white + t_dark, [0, 0, 0, 255]);
    // ...then white inner band (inset by t_dark), leaving a thin dark ring.
    if rx1 - rx0 > 2 * t_dark && ry1 - ry0 > 2 * t_dark {
        fill_border_ring(
            doc,
            rx0 + t_dark,
            ry0 + t_dark,
            rx1 - t_dark,
            ry1 - t_dark,
            t_white,
            [255, 255, 255, 255],
        );
    }
}

/// Fill the `thickness`-pixel border ring of the half-open rect
/// `[rx0, rx1) × [ry0, ry1)` with `color`, clamped to image bounds.
fn fill_border_ring(
    img: &mut RgbaImage,
    rx0: i32,
    ry0: i32,
    rx1: i32,
    ry1: i32,
    thickness: i32,
    color: [u8; 4],
) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let t = thickness.max(0);
    if rx1 <= rx0 || ry1 <= ry0 || t == 0 {
        return;
    }
    let y_start = ry0.max(0);
    let y_end = ry1.min(h);
    let x_start = rx0.max(0);
    let x_end = rx1.min(w);
    let px = image::Rgba(color);
    for y in y_start..y_end {
        let in_top = y < ry0 + t;
        let in_bottom = y >= ry1 - t;
        for x in x_start..x_end {
            let in_left = x < rx0 + t;
            let in_right = x >= rx1 - t;
            if in_top || in_bottom || in_left || in_right {
                img.put_pixel(x as u32, y as u32, px);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_border_ring_draws_a_white_outline_with_dark_outer_ring() {
        // 12x8 doc, rect covering the whole doc, scale 1. White ~2px, dark ~1px.
        let mut doc = image::RgbaImage::new(12, 8);
        // Fill with a mid-gray so we can see both rings distinctly.
        for p in doc.pixels_mut() {
            *p = image::Rgba([128, 128, 128, 255]);
        }
        rasterize_rect(
            &mut doc,
            Pos2::new(0.0, 0.0),
            Pos2::new(0.0, 0.0),
            Pos2::new(12.0, 8.0),
            1.0,
        );
        // Outer ring (1px) should be dark, next ring (2px) white, interior gray.
        assert_eq!(*doc.get_pixel(0, 0), image::Rgba([0, 0, 0, 255])); // corner dark
        assert_eq!(*doc.get_pixel(1, 1), image::Rgba([255, 255, 255, 255])); // white ring
        assert_eq!(*doc.get_pixel(2, 2), image::Rgba([255, 255, 255, 255])); // white ring
        assert_eq!(*doc.get_pixel(3, 3), image::Rgba([128, 128, 128, 255])); // interior untouched
        // Bottom-right edge: outer dark on last col/row.
        assert_eq!(*doc.get_pixel(11, 7), image::Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn fill_border_ring_clamps_to_image_bounds_without_panicking() {
        let mut doc = image::RgbaImage::new(4, 4);
        let before = doc.clone();
        // Rect extends far beyond the image and its border ring lies entirely
        // off-image, so (correctly) nothing is drawn and nothing panics.
        fill_border_ring(&mut doc, -5, -5, 20, 20, 2, [255, 255, 255, 255]);
        assert_eq!(doc, before);

        // A partially off-image rect draws only the in-image portion of its
        // border ring (right + bottom edges land inside this 4x4 doc).
        let mut doc = image::RgbaImage::new(4, 4);
        fill_border_ring(&mut doc, -2, -2, 4, 4, 2, [255, 255, 255, 255]);
        // in-right: x >= 4-2 = 2 ; in-bottom: y >= 2  → cols/rows {2,3} drawn.
        for y in 0..4 {
            for x in 0..4 {
                let expected = if x >= 2 || y >= 2 {
                    image::Rgba([255, 255, 255, 255])
                } else {
                    image::Rgba([0, 0, 0, 0])
                };
                assert_eq!(*doc.get_pixel(x, y), expected, "at ({x},{y})");
            }
        }
    }

    #[test]
    fn fill_border_ring_noop_for_degenerate_rect() {
        let mut doc = image::RgbaImage::new(4, 4);
        let before = doc.clone();
        fill_border_ring(&mut doc, 1, 1, 1, 3, 2, [255, 255, 255, 255]); // zero width
        assert_eq!(doc, before);
        let mut doc = image::RgbaImage::new(4, 4);
        let before = doc.clone();
        fill_border_ring(&mut doc, 0, 0, 4, 4, 0, [255, 255, 255, 255]); // zero thickness
        assert_eq!(doc, before);
    }
}
