//! Region-selection overlay + in-place editor.
//!
//! Two phases share one fullscreen viewport:
//! - [`Phase::Select`]: a frozen snapshot of the whole virtual desktop (all
//!   monitors) with a draggable crop box. Esc cancels; mouse-up (real drag),
//!   click-to-snap, or Enter confirms and transitions to `Edit`.
//! - [`Phase::Edit`]: the cropped selection becomes the editable canvas, shown
//!   at full brightness in its original on-screen position (the rest of the
//!   overlay stays dimmed — a spotlight). A floating toolbar offers the
//!   annotation tools (Rectangle / Arrow / Pencil / Text) plus terminal actions
//!   (Pin/Save/Copy/Cancel).
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
//!
//! All annotation tools share one fixed style: a white ~2px stroke with a thin
//! dark backing for contrast on light backgrounds. There is no color/width UI.

use std::sync::{Arc, Mutex};

use eframe::egui;
use egui::{Align2, Color32, Key, Pos2, Rect, Stroke, Vec2};
use image::RgbaImage;

/// Minimum drag size (logical points) below which a selection or a stroke is
/// ignored.
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
/// Dark backing color used behind every white annotation stroke.
const STROKE_DARK: [u8; 4] = [0, 0, 0, 255];
/// Foreground (white) color used for every annotation stroke.
const STROKE_WHITE: [u8; 4] = [255, 255, 255, 255];

/// Overlay phase.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Drag to select / hover-snap a window / Enter to confirm.
    Select,
    /// The cropped selection is the editable canvas + floating toolbar.
    Edit,
}

/// Active annotation tool. The default is no tool.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// No annotation tool active (pointer does nothing on the canvas).
    None,
    /// Rectangle outline tool.
    Rect,
    /// Arrow tool (line + chevron head).
    Arrow,
    /// Pencil / freehand polyline tool.
    Pencil,
    /// Text tool (in-place single-line text, baked via a 5x7 bitmap font).
    Text,
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
    ToggleArrow,
    TogglePencil,
    ToggleText,
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
    /// Shared drag origin (overlay-local logical) for the Rect + Arrow tools
    /// (opposite-corner drag semantics).
    pub tool_drag_start: Option<Pos2>,
    /// Shared current pointer (overlay-local logical) for the Rect + Arrow
    /// tools.
    pub tool_drag_cur: Option<Pos2>,
    /// Freehand polyline points (overlay-local logical) for the Pencil tool.
    pub pencil_points: Vec<Pos2>,
    /// Anchor (overlay-local logical) where the in-progress text started.
    pub text_anchor: Option<Pos2>,
    /// Buffer for the in-progress single-line text.
    pub text_buf: String,
    /// True while the Text tool has an active in-place text input.
    pub text_editing: bool,
    /// One-shot: request keyboard focus for the text input on its first frame.
    pub text_wants_focus: bool,

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
            tool_drag_start: None,
            tool_drag_cur: None,
            pencil_points: Vec::new(),
            text_anchor: None,
            text_buf: String::new(),
            text_editing: false,
            text_wants_focus: false,
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

    /// Clear all in-progress tool interaction state (a drag in flight, a
    /// freehand polyline, or an open text input). Used when switching tools,
    /// starting a new selection, or as the first Esc.
    fn cancel_in_progress(&mut self) {
        self.tool_drag_start = None;
        self.tool_drag_cur = None;
        self.pencil_points.clear();
        self.text_editing = false;
        self.text_anchor = None;
        self.text_buf.clear();
        self.text_wants_focus = false;
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

    /// Bake the in-progress text into the document at its anchor (dark backing
    /// rect + white 5x7 glyphs), push it via [`Self::commit_doc`], then clear
    /// all text state. An empty buffer is a no-op (just clears the state).
    fn commit_text(&mut self, ctx: &egui::Context) {
        let anchor = self.text_anchor;
        let drect = self.doc_rect;
        let scale = self.scale;
        // 2x round(scale): at 100% DPI this yields 10x14 glyphs (vs the 5x7
        // the raw scale gave), closer to the ~13pt live TextEdit preview.
        // Min 2 for legibility. The 5x7 FONT TABLE itself is unchanged.
        let scale_i = ((scale * 2.0).round() as i32).max(2);
        let buf = self.text_buf.trim().to_string();
        let base = self.doc.clone();
        if let (Some(anchor), Some(drect), Some(base)) = (anchor, drect, base) {
            if !buf.is_empty() {
                let ax = ((anchor.x - drect.min.x) * scale).round() as i32;
                let ay = ((anchor.y - drect.min.y) * scale).round() as i32;
                let mut nd = base;
                let (tw, th) = text_bounds(&buf, scale_i);
                // Dark backing rect with ~1 glyph-unit padding for legibility.
                fill_solid_rect(
                    &mut nd,
                    ax - scale_i,
                    ay - scale_i,
                    ax + tw + scale_i - 1,
                    ay + th + scale_i - 1,
                    STROKE_DARK,
                );
                rasterize_text(&mut nd, ax, ay, &buf, scale_i, STROKE_WHITE);
                self.commit_doc(ctx, nd);
            }
        }
        self.text_editing = false;
        self.text_anchor = None;
        self.text_buf.clear();
        self.text_wants_focus = false;
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
        self.cancel_in_progress();
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
    tool_drag_start: Option<Pos2>,
    tool_drag_cur: Option<Pos2>,
    pencil_points: Vec<Pos2>,
    text_editing: bool,
    text_anchor: Option<Pos2>,
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
            tool_drag_start: g.tool_drag_start,
            tool_drag_cur: g.tool_drag_cur,
            pencil_points: g.pencil_points.clone(),
            text_editing: g.text_editing,
            text_anchor: g.text_anchor,
            undo_available: !g.undo_stack.is_empty(),
            finished: g.finished,
        }
    };

    if r.finished {
        return;
    }

    let painter = ui.painter().clone();
    let full_uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));

    // The Edit-phase doc canvas is registered as an interactable widget so the
    // foreground toolbar/text Areas (shown below) consume clicks first and the
    // canvas only receives clicks that land on the doc itself. Captured here in
    // the paint pass (outside the session lock) and reused by the input pass.
    let mut canvas_resp: Option<egui::Response> = None;

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

            // --- Live annotation previews (painter only; no doc mutation yet).
            //     Every tool draws a dark backing pass then a white pass so the
            //     white stroke stays legible on light backgrounds.
            match r.active_tool {
                Tool::Rect => {
                    if let (Some(ds), Some(dc)) = (r.tool_drag_start, r.tool_drag_cur) {
                        paint_rect_preview(&painter, ds, dc);
                    }
                }
                Tool::Arrow => {
                    if let (Some(ds), Some(dc)) = (r.tool_drag_start, r.tool_drag_cur) {
                        paint_arrow_preview(&painter, ds, dc);
                    }
                }
                Tool::Pencil => {
                    if r.pencil_points.len() >= 2 {
                        let dark =
                            egui::epaint::PathStroke::new(3.0, Color32::from_black_alpha(220));
                        let white = egui::epaint::PathStroke::new(2.0, Color32::WHITE);
                        painter.line(r.pencil_points.clone(), dark);
                        painter.line(r.pencil_points.clone(), white);
                    }
                }
                Tool::None | Tool::Text => {}
            }

            // Capture the canvas interaction (Edit-phase only) so the input
            // pass can gate presses/drags on it. The toolbar + text Areas
            // rendered below are `Order::Foreground`, so egui routes any
            // overlapping click to them first; the canvas never sees toolbar
            // clicks → no spurious text box / tool drag from a toolbar click.
            if let Some(drect) = r.doc_rect {
                canvas_resp = Some(ui.interact(
                    drect,
                    egui::Id::new("editor_canvas"),
                    egui::Sense::click_and_drag(),
                ));
            }
        }
    }

    // --- TOOLBAR (Edit phase only) ---
    let mut action: Option<ToolbarAction> = None;
    if r.phase == Phase::Edit {
        if let Some(sel) = r.doc_rect {
            let active_tool = r.active_tool;
            let undo_available = r.undo_available;
            let is_active = |t: Tool| active_tool == t;
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
                                if tool_button(ui, "Rect", is_active(Tool::Rect)) {
                                    action = Some(ToolbarAction::ToggleRect);
                                }
                                if tool_button(ui, "Arrow", is_active(Tool::Arrow)) {
                                    action = Some(ToolbarAction::ToggleArrow);
                                }
                                if tool_button(ui, "Pencil", is_active(Tool::Pencil)) {
                                    action = Some(ToolbarAction::TogglePencil);
                                }
                                if tool_button(ui, "Text", is_active(Tool::Text)) {
                                    action = Some(ToolbarAction::ToggleText);
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

    // --- TEXT EDIT OVERLAY (Edit phase, Text tool, actively editing) ---
    // Rendered as its own foreground Area at the anchor; the TextEdit mutates a
    // local buffer clone (egui keeps widget state by Id across frames), and we
    // write it back / commit under a brief session lock after the Area. Enter
    // or focus-loss (click-away) commits; Esc is handled in the input pass.
    if r.phase == Phase::Edit && r.active_tool == Tool::Text && r.text_editing {
        if let Some(anchor) = r.text_anchor {
            let (mut buf, wants_focus) = {
                let g = session.lock().expect("region session poisoned");
                (g.text_buf.clone(), g.text_wants_focus)
            };
            let mut should_commit = false;
            let area_id = egui::Id::new("text_edit_area");
            egui::Area::new(area_id)
                .order(egui::Order::Foreground)
                .fixed_pos(anchor)
                .show(&ctx, |ui| {
                    egui::Frame::group(ui.style())
                        .fill(crate::ui::theme::SURFACE)
                        .stroke(egui::Stroke::new(1.0, crate::ui::theme::BORDER))
                        .corner_radius(egui::CornerRadius::same(crate::ui::theme::RADIUS_SM))
                        .inner_margin(egui::Margin::same(4))
                        .show(ui, |ui| {
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut buf)
                                    .desired_width(180.0)
                                    .text_color(Color32::WHITE),
                            );
                            if wants_focus && !resp.has_focus() {
                                resp.request_focus();
                            }
                            let enter_pressed = ui.ctx().input(|i| i.key_pressed(Key::Enter));
                            // Single-line TextEdit loses focus on Enter; a
                            // click elsewhere also loses focus → both commit.
                            if resp.lost_focus() || enter_pressed {
                                should_commit = true;
                            }
                        });
                });
            {
                let mut g = session.lock().expect("region session poisoned");
                g.text_buf = buf;
                g.text_wants_focus = false;
                if should_commit {
                    g.commit_text(&ctx);
                }
            }
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

    // Edit-phase pointer triggers gated on the canvas Response, so clicks the
    // foreground toolbar/text Areas consumed don't start a tool drag or place a
    // text box. Select-phase keeps the raw pointer (it needs the full screen).
    let canvas_clicked_primary = canvas_resp
        .as_ref()
        .map(|r| r.clicked_by(egui::PointerButton::Primary))
        .unwrap_or(false);
    let canvas_is_down = canvas_resp.as_ref().map(|r| r.dragged()).unwrap_or(false);
    let canvas_released = canvas_resp
        .as_ref()
        .map(|r| r.drag_stopped())
        .unwrap_or(false);

    let mut g = session.lock().expect("region session poisoned");
    if g.finished {
        return;
    }

    // Esc / window-close handling. In Edit phase, the first Esc cancels any
    // in-progress tool action (drag / freehand / text) and stays in the editor;
    // a second Esc (or a window-close) finishes with `Cancel`.
    if esc || close_requested {
        let in_progress = g.phase == Phase::Edit
            && !close_requested
            && (g.tool_drag_start.is_some()
                || g.tool_drag_cur.is_some()
                || !g.pencil_points.is_empty()
                || g.text_editing);
        g.drag_start = None;
        g.drag_cur = None;
        g.cancel_in_progress();
        if in_progress {
            return;
        }
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
            // 1. Apply a toolbar action, if any. Toggling a tool cancels any
            //    in-progress drag / text first.
            match action {
                Some(ToolbarAction::New) => {
                    // Back to select; KEEP the same full_image (no re-capture).
                    g.phase = Phase::Select;
                    g.doc = None;
                    g.doc_texture = None;
                    g.doc_rect = None;
                    g.undo_stack.clear();
                    g.cancel_in_progress();
                    g.active_tool = Tool::None;
                    g.drag_start = None;
                    g.drag_cur = None;
                    g.hover_rect = None;
                    return;
                }
                Some(ToolbarAction::ToggleRect) => {
                    g.cancel_in_progress();
                    g.active_tool = if g.active_tool == Tool::Rect {
                        Tool::None
                    } else {
                        Tool::Rect
                    };
                }
                Some(ToolbarAction::ToggleArrow) => {
                    g.cancel_in_progress();
                    g.active_tool = if g.active_tool == Tool::Arrow {
                        Tool::None
                    } else {
                        Tool::Arrow
                    };
                }
                Some(ToolbarAction::TogglePencil) => {
                    g.cancel_in_progress();
                    g.active_tool = if g.active_tool == Tool::Pencil {
                        Tool::None
                    } else {
                        Tool::Pencil
                    };
                }
                Some(ToolbarAction::ToggleText) => {
                    g.cancel_in_progress();
                    g.active_tool = if g.active_tool == Tool::Text {
                        Tool::None
                    } else {
                        Tool::Text
                    };
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

            // 2. Per-tool pointer interaction on the canvas.
            match g.active_tool {
                Tool::None | Tool::Text => {
                    // Text anchor placement is handled below for the Text tool.
                }
                Tool::Rect | Tool::Arrow => {
                    if let Some(drect) = g.doc_rect {
                        if canvas_is_down {
                            if let Some(p) = latest {
                                if drect.contains(p) {
                                    if g.tool_drag_start.is_none() {
                                        g.tool_drag_start = Some(p);
                                    }
                                    g.tool_drag_cur = Some(p);
                                }
                            }
                        } else if canvas_released {
                            if let (Some(ds), Some(dc)) = (g.tool_drag_start, g.tool_drag_cur) {
                                let drag = (dc.x - ds.x).abs() > MIN_DRAG
                                    && (dc.y - ds.y).abs() > MIN_DRAG;
                                if drag {
                                    if let Some(base) = g.doc.clone() {
                                        let mut nd = base;
                                        match g.active_tool {
                                            Tool::Rect => {
                                                rasterize_rect(&mut nd, drect.min, ds, dc, g.scale)
                                            }
                                            Tool::Arrow => {
                                                rasterize_arrow(&mut nd, drect.min, ds, dc, g.scale)
                                            }
                                            _ => {}
                                        }
                                        g.commit_doc(&ctx, nd);
                                    }
                                }
                                g.tool_drag_start = None;
                                g.tool_drag_cur = None;
                            }
                        }
                    }
                }
                Tool::Pencil => {
                    if let Some(drect) = g.doc_rect {
                        if canvas_is_down {
                            if let Some(p) = latest {
                                if drect.contains(p) {
                                    if g.pencil_points.is_empty() {
                                        g.pencil_points.push(p);
                                    } else {
                                        let last = *g.pencil_points.last().unwrap();
                                        if (p - last).length() > 1.0 {
                                            g.pencil_points.push(p);
                                        }
                                    }
                                }
                            }
                        } else if canvas_released && g.pencil_points.len() >= 2 {
                            if let Some(base) = g.doc.clone() {
                                let mut nd = base;
                                rasterize_pencil(&mut nd, drect.min, &g.pencil_points, g.scale);
                                g.commit_doc(&ctx, nd);
                            }
                            g.pencil_points.clear();
                        } else if canvas_released {
                            // A tap without movement: ignore (no commit).
                            g.pencil_points.clear();
                        }
                    }
                }
            }

            // Text tool: a press inside the doc opens a new in-place input.
            // (If a text was already open, the Area's focus-loss has already
            // committed it this frame; placing a fresh anchor here is the
            // natural "click to place the next one" behavior.)
            if g.active_tool == Tool::Text {
                if let Some(drect) = g.doc_rect {
                    if !g.text_editing && canvas_clicked_primary {
                        if let Some(p) = latest {
                            if drect.contains(p) {
                                g.text_anchor = Some(p);
                                g.text_buf.clear();
                                g.text_editing = true;
                                g.text_wants_focus = true;
                            }
                        }
                    }
                }
            }
        }
    }
}

// --- Live-preview painters (logical-space; mirror the rasterizers) ----------

/// Live rectangle preview: dark outer band then a white inner band inset by
/// ~1 logical pt.
fn paint_rect_preview(painter: &egui::Painter, ds: Pos2, dc: Pos2) {
    let rect = Rect::from_two_pos(ds, dc);
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(3.0, Color32::from_black_alpha(220)),
        egui::epaint::StrokeKind::Inside,
    );
    let inner = rect.shrink2(Vec2::splat(1.0));
    painter.rect_stroke(
        inner,
        0.0,
        Stroke::new(2.0, Color32::WHITE),
        egui::epaint::StrokeKind::Inside,
    );
}

/// Live arrow preview: dark+white shaft then a chevron head at the current
/// pointer (tip), pointing back toward the drag start.
fn paint_arrow_preview(painter: &egui::Painter, ds: Pos2, dc: Pos2) {
    let dark = Stroke::new(3.0, Color32::from_black_alpha(220));
    let white = Stroke::new(2.0, Color32::WHITE);
    painter.line_segment([ds, dc], dark);
    painter.line_segment([ds, dc], white);
    let dir = dc - ds;
    let len = dir.length();
    if len > 1.0 {
        let u = dir / len;
        let perp = Vec2::new(-u.y, u.x);
        let head_len = 12.0_f32;
        let spread = (30.0_f32).to_radians();
        let cos_a = spread.cos();
        let sin_a = spread.sin();
        for sign in [-1.0_f32, 1.0] {
            let back = Vec2::new(
                head_len * (cos_a * u.x + sign * sin_a * perp.x),
                head_len * (cos_a * u.y + sign * sin_a * perp.y),
            );
            let barb = dc - back;
            painter.line_segment([dc, barb], dark);
            painter.line_segment([dc, barb], white);
        }
    }
}

/// A tool-toggle button: accent fill (dark text) when active, secondary style
/// otherwise. Matches the existing Rect button treatment, generalized.
fn tool_button(ui: &mut egui::Ui, label: &'static str, active: bool) -> bool {
    let btn = if active {
        egui::Button::new(egui::RichText::new(label).color(Color32::BLACK))
            .fill(crate::ui::theme::ACCENT_BLUE_BRIGHT)
            .corner_radius(egui::CornerRadius::same(crate::ui::theme::RADIUS_SM))
            .min_size(egui::vec2(0.0, 36.0))
    } else {
        crate::ui::theme::secondary_button(label)
    };
    ui.add(btn).clicked()
}

/// Pick a toolbar position just outside the selection's bottom-right corner,
/// flipping to the opposite side (above-left) when it would run off-screen.
/// Uses a rough toolbar size estimate; it only needs to be approximately right.
fn toolbar_position(sel: Rect, screen: Rect) -> Pos2 {
    // 10 buttons * ~56pt each, ~48pt tall with padding.
    let tb_w = 10.0 * 56.0;
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

// --- Rasterizers (doc = physical pixels) ------------------------------------

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
    fill_border_ring(doc, rx0, ry0, rx1, ry1, t_white + t_dark, STROKE_DARK);
    // ...then white inner band (inset by t_dark), leaving a thin dark ring.
    if rx1 - rx0 > 2 * t_dark && ry1 - ry0 > 2 * t_dark {
        fill_border_ring(
            doc,
            rx0 + t_dark,
            ry0 + t_dark,
            rx1 - t_dark,
            ry1 - t_dark,
            t_white,
            STROKE_WHITE,
        );
    }
}

/// Rasterize an arrow (shaft + chevron head) onto `doc`. Dark backing pass
/// (thickness+1) then a white pass (thickness); the head is drawn the same way
/// at the tip (`drag_cur`), pointing back toward `drag_start`.
fn rasterize_arrow(
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
    let (x0, y0) = to_px(drag_start);
    let (x1, y1) = to_px(drag_cur);
    let t_white = ((2.0 * scale).round() as i32).max(1);
    let t_dark = t_white + 1;

    rasterize_thick_line(doc, x0, y0, x1, y1, t_dark, STROKE_DARK);
    rasterize_thick_line(doc, x0, y0, x1, y1, t_white, STROKE_WHITE);
    // Head at the tip (x1,y1); "base" direction is back toward (x0,y0).
    rasterize_arrowhead(doc, x1, y1, x0, y0, t_dark, STROKE_DARK, scale);
    rasterize_arrowhead(doc, x1, y1, x0, y0, t_white, STROKE_WHITE, scale);
}

/// Rasterize a freehand polyline (the Pencil commit core). Dark backing then
/// white, thick-lined segment by segment.
fn rasterize_pencil(doc: &mut RgbaImage, sel_min_logical: Pos2, points: &[Pos2], scale: f32) {
    let to_px = |p: Pos2| -> (i32, i32) {
        (
            ((p.x - sel_min_logical.x) * scale).round() as i32,
            ((p.y - sel_min_logical.y) * scale).round() as i32,
        )
    };
    let pts: Vec<(i32, i32)> = points.iter().map(|p| to_px(*p)).collect();
    let t_white = ((2.0 * scale).round() as i32).max(1);
    let t_dark = t_white + 1;
    rasterize_polyline(doc, &pts, t_dark, STROKE_DARK);
    rasterize_polyline(doc, &pts, t_white, STROKE_WHITE);
}

/// Bresenham thick line: walk the line from (x0,y0) to (x1,y1) and stamp a
/// filled square of side `thickness*2 + 1` at each step, clamped to doc bounds.
/// Panic-free (every pixel is bounds-checked). A single-point line stamps once.
fn rasterize_thick_line(
    doc: &mut RgbaImage,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    thickness: i32,
    color: [u8; 4],
) {
    let px = image::Rgba(color);
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    loop {
        stamp_square(doc, x, y, thickness, px);
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// Stamp a filled square of side `thickness*2 + 1` centered on `(cx, cy)`,
/// clamped to image bounds. `thickness == 0` stamps a single pixel.
fn stamp_square(img: &mut RgbaImage, cx: i32, cy: i32, thickness: i32, px: image::Rgba<u8>) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let t = thickness.max(0);
    let xs = (cx - t).max(0);
    let ys = (cy - t).max(0);
    let xe = (cx + t).min(w - 1);
    let ye = (cy + t).min(h - 1);
    for yy in ys..=ye {
        for xx in xs..=xe {
            img.put_pixel(xx as u32, yy as u32, px);
        }
    }
}

/// Rasterize an arrowhead at the tip `(tip_x, tip_y)`: two short thick barbs
/// pointing back toward `(base_x, base_y)` at ±~30° off the shaft. Reuses
/// [`rasterize_thick_line`] for each barb so it shares the dark/white pass
/// treatment. Head length is `12 * scale` physical px (matching the 12-logical
/// px live preview), clamped to `[1, shaft_length]`.
fn rasterize_arrowhead(
    doc: &mut RgbaImage,
    tip_x: i32,
    tip_y: i32,
    base_x: i32,
    base_y: i32,
    thickness: i32,
    color: [u8; 4],
    scale: f32,
) {
    let dx = (base_x - tip_x) as f32;
    let dy = (base_y - tip_y) as f32;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.5 {
        return;
    }
    let ux = dx / len;
    let uy = dy / len;
    // Perpendicular to the shaft.
    let perp_x = -uy;
    let perp_y = ux;
    let head_len = (12.0_f32 * scale).min(len).max(1.0);
    let cos_a = (30.0_f32).to_radians().cos();
    let sin_a = (30.0_f32).to_radians().sin();
    for sign in [-1.0_f32, 1.0] {
        let bx = tip_x as f32 + head_len * (cos_a * ux + sign * sin_a * perp_x);
        let by = tip_y as f32 + head_len * (cos_a * uy + sign * sin_a * perp_y);
        rasterize_thick_line(
            doc,
            tip_x,
            tip_y,
            bx.round() as i32,
            by.round() as i32,
            thickness,
            color,
        );
    }
}

/// Rasterize a polyline by thick-lining each consecutive pair. A single point
/// stamps once; all stamps are bounds-clamped via [`rasterize_thick_line`].
fn rasterize_polyline(doc: &mut RgbaImage, points: &[(i32, i32)], thickness: i32, color: [u8; 4]) {
    if points.is_empty() {
        return;
    }
    if points.len() == 1 {
        let (x, y) = points[0];
        stamp_square(doc, x, y, thickness, image::Rgba(color));
        return;
    }
    for w in points.windows(2) {
        let (x0, y0) = w[0];
        let (x1, y1) = w[1];
        rasterize_thick_line(doc, x0, y0, x1, y1, thickness, color);
    }
}

/// Fill the inclusive rect from `(x0,y0)` to `(x1,y1)` with `color`, clamped to
/// image bounds. Used for the text backing.
fn fill_solid_rect(img: &mut RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, color: [u8; 4]) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    let px = image::Rgba(color);
    let xs = x0.max(0);
    let ys = y0.max(0);
    let xe = x1.min(w - 1);
    let ye = y1.min(h - 1);
    for y in ys..=ye {
        for x in xs..=xe {
            img.put_pixel(x as u32, y as u32, px);
        }
    }
}

/// Bounds-checked `put_pixel` for the glyph renderer.
fn put_pixel_clamped(img: &mut RgbaImage, x: i32, y: i32, px: image::Rgba<u8>) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    if x >= 0 && y >= 0 && x < w && y < h {
        img.put_pixel(x as u32, y as u32, px);
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

// --- 5x7 bitmap font --------------------------------------------------------

/// Look up the 5x7 glyph (7 rows, MSB = leftmost of 5 cols) for a character.
/// Lowercase maps to uppercase; unknown characters map to space (blank).
/// Supported: A-Z, 0-9, space, and `.,!?-:/`.
fn glyph_for(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        ' ' | '\t' => [0; 7],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => [
            0b00111, 0b00010, 0b00010, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        '.' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110,
        ],
        ',' => [
            0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110, 0b00100,
        ],
        '!' => [
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100,
        ],
        '?' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b00000, 0b00100,
        ],
        '-' => [
            0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000,
        ],
        ':' => [
            0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000,
        ],
        '/' => [
            0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
        ],
        _ => [0; 7],
    }
}

/// Physical-pixel bounds of `s` rendered at `scale` (px per logical unit):
/// width = `6*scale*(n-1) + 5*scale`, height = `7*scale`. Empty string → (0,0).
fn text_bounds(s: &str, scale: i32) -> (i32, i32) {
    if s.is_empty() {
        return (0, 0);
    }
    let sc = scale.max(1);
    let n = s.chars().count() as i32;
    let w = 6 * sc * (n - 1) + 5 * sc;
    (w, 7 * sc)
}

/// Rasterize `s` at `(x0, y0)` using the 5x7 bitmap font, scaled by `scale`
/// (each font pixel becomes a `scale`×`scale` block). Advance is `6*scale` per
/// glyph. All pixels are bounds-clamped; unknown chars render as blank space.
fn rasterize_text(doc: &mut RgbaImage, x0: i32, y0: i32, s: &str, scale: i32, color: [u8; 4]) {
    let sc = scale.max(1);
    let px = image::Rgba(color);
    let mut cx = x0;
    for ch in s.chars() {
        let glyph = glyph_for(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5u32 {
                if (bits >> (4 - col)) & 1 == 1 {
                    let gx = cx + (col as i32) * sc;
                    let gy = y0 + (row as i32) * sc;
                    for dy in 0..sc {
                        for dx in 0..sc {
                            put_pixel_clamped(doc, gx + dx, gy + dy, px);
                        }
                    }
                }
            }
        }
        cx += 6 * sc;
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

    // --- fill_border_ring / rasterize_rect (existing, kept) -----------------

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

    // --- rasterize_thick_line ------------------------------------------------

    #[test]
    fn thick_line_horizontal() {
        let mut doc = image::RgbaImage::new(20, 10);
        rasterize_thick_line(&mut doc, 2, 5, 8, 5, 1, [255, 255, 255, 255]);
        // Along the line, thickness 1 → y in 4..=6 stamped.
        assert_eq!(*doc.get_pixel(5, 5), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(5, 4), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(5, 6), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(0, 0), image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn thick_line_vertical_thickness_zero() {
        let mut doc = image::RgbaImage::new(10, 20);
        rasterize_thick_line(&mut doc, 5, 2, 5, 8, 0, [255, 255, 255, 255]);
        assert_eq!(*doc.get_pixel(5, 5), image::Rgba([255, 255, 255, 255]));
        // thickness 0 → single-pixel stamp, neighbors untouched.
        assert_eq!(*doc.get_pixel(4, 5), image::Rgba([0, 0, 0, 0]));
        assert_eq!(*doc.get_pixel(6, 5), image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn thick_line_diagonal_endpoints() {
        let mut doc = image::RgbaImage::new(10, 10);
        rasterize_thick_line(&mut doc, 0, 0, 9, 9, 0, [255, 255, 255, 255]);
        assert_eq!(*doc.get_pixel(0, 0), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(9, 9), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(5, 5), image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn thick_line_thickness_gt_one() {
        let mut doc = image::RgbaImage::new(20, 10);
        rasterize_thick_line(&mut doc, 5, 5, 15, 5, 2, [255, 255, 255, 255]);
        // thickness 2 → y in 3..=7 stamped along the line.
        assert_eq!(*doc.get_pixel(10, 3), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(10, 7), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(10, 2), image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn thick_line_fully_off_canvas_no_panic() {
        let mut doc = image::RgbaImage::new(10, 10);
        let before = doc.clone();
        rasterize_thick_line(&mut doc, -50, -50, -40, -40, 3, [255, 255, 255, 255]);
        assert_eq!(doc, before);
    }

    #[test]
    fn thick_line_single_point_stamps_once() {
        let mut doc = image::RgbaImage::new(10, 10);
        rasterize_thick_line(&mut doc, 5, 5, 5, 5, 1, [255, 255, 255, 255]);
        // thickness 1 → 3x3 stamp centered on (5,5).
        assert_eq!(*doc.get_pixel(5, 5), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(4, 4), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(6, 6), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(7, 7), image::Rgba([0, 0, 0, 0]));
    }

    // --- rasterize_polyline --------------------------------------------------

    #[test]
    fn polyline_multi_segment() {
        let mut doc = image::RgbaImage::new(20, 20);
        let pts = vec![(2, 2), (2, 10), (10, 10)];
        rasterize_polyline(&mut doc, &pts, 0, [255, 255, 255, 255]);
        assert_eq!(*doc.get_pixel(2, 6), image::Rgba([255, 255, 255, 255]));
        assert_eq!(*doc.get_pixel(6, 10), image::Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn polyline_clamps_off_canvas_without_panic() {
        let mut doc = image::RgbaImage::new(5, 5);
        let pts = vec![(-10, -10), (20, 20)];
        rasterize_polyline(&mut doc, &pts, 1, [255, 255, 255, 255]);
        // The diagonal passes through (0,0); thickness 1 stamps it.
        assert_eq!(*doc.get_pixel(0, 0), image::Rgba([255, 255, 255, 255]));
    }

    // --- rasterize_text / text_bounds ----------------------------------------

    #[test]
    fn text_writes_glyph_pixels_within_bounds() {
        let mut doc = image::RgbaImage::new(60, 20);
        rasterize_text(&mut doc, 0, 0, "A", 1, [255, 255, 255, 255]);
        // 'A' top row 0b01110 → cols 1,2,3 set at row 0; col 0 blank.
        assert_eq!(*doc.get_pixel(0, 0), image::Rgba([0, 0, 0, 0]));
        assert_eq!(*doc.get_pixel(1, 0), image::Rgba([255, 255, 255, 255]));
        // 'A' middle row 0b11111 → all of cols 0..=4 set at row 3.
        for x in 0..5 {
            assert_eq!(
                *doc.get_pixel(x, 3),
                image::Rgba([255, 255, 255, 255]),
                "row 3 col {x}"
            );
        }
        // Advance is 6: nothing of this 1-char string at x=5 (col 5 row 3 blank).
        assert_eq!(*doc.get_pixel(5, 3), image::Rgba([0, 0, 0, 0]));
    }

    #[test]
    fn text_empty_is_noop() {
        let mut doc = image::RgbaImage::new(10, 10);
        let before = doc.clone();
        rasterize_text(&mut doc, 0, 0, "", 1, [255, 255, 255, 255]);
        assert_eq!(doc, before);
    }

    #[test]
    fn text_unknown_char_renders_as_space_no_panic() {
        let mut doc = image::RgbaImage::new(20, 10);
        let before = doc.clone();
        // '@' is unsupported → blank (space) glyph; no panic, no pixels.
        rasterize_text(&mut doc, 0, 0, "@", 1, [255, 255, 255, 255]);
        assert_eq!(doc, before);
    }

    #[test]
    fn text_bounds_basic() {
        assert_eq!(text_bounds("", 1), (0, 0));
        assert_eq!(text_bounds("A", 1), (5, 7));
        // 6*1*(2-1) + 5*1 = 11
        assert_eq!(text_bounds("AB", 1), (11, 7));
        // scale 2: 6*2*(2-1) + 5*2 = 22, height 14
        assert_eq!(text_bounds("AB", 2), (22, 14));
    }

    // --- rasterize_arrow (smoke) --------------------------------------------

    #[test]
    fn arrow_rasterizes_without_panic_and_writes_pixels() {
        let mut doc = image::RgbaImage::new(40, 20);
        rasterize_arrow(
            &mut doc,
            Pos2::new(0.0, 0.0),
            Pos2::new(0.0, 0.0),
            Pos2::new(30.0, 10.0),
            1.0,
        );
        // Tip region near (30,10) should have some white pixels.
        let mut found_white = false;
        for y in 7..14 {
            for x in 27..34 {
                if *doc.get_pixel(x as u32, y as u32) == image::Rgba([255, 255, 255, 255]) {
                    found_white = true;
                }
            }
        }
        assert!(found_white, "expected white pixels near the arrow tip");
    }
}
