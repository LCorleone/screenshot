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
//! Annotation styling is session-global and user-controlled via the toolbar:
//! a color (one of six presets) and a stroke width (1/2/4 logical px). Rect,
//! Arrow and Pencil use both; Text bakes glyphs in the active color (its size
//! is fixed ~16 logical px, DPI-scaled) via ab_glyph + the bundled Geist Sans;
//! Blur and Mosaic ignore both. There is no dark backing — strokes are a pure
//! single pass in the active color.

use std::sync::{Arc, Mutex};

use ab_glyph::{Font, FontVec, Point, PxScale, ScaleFont};
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
/// Default annotation color (Solarized blue — pops on the light backdrop).
const ACTIVE_COLOR_DEFAULT: Color32 = Color32::from_rgb(0x26, 0x8b, 0xd2);
/// Default annotation stroke width, in logical px.
const ACTIVE_WIDTH_DEFAULT: f32 = 2.0;
/// Logical text size baked by the ab_glyph renderer (DPI-scaled to physical).
const TEXT_LOGICAL_SIZE: f32 = 16.0;
/// Bundled Geist Sans (Regular), parsed on demand by the text rasterizer.
/// Keeping it as `&'static [u8]` avoids storing a `!Sync` `FontVec` on the
/// `Arc<Mutex<RegionSession>>` (the parse per text commit is infrequent).
const GEIST_FONT_BYTES: &[u8] = include_bytes!("../../assets/fonts/Geist-Regular.ttf");
/// Preset colors offered by the Color popover, in toolbar order. Solarized
/// accent set — reads well on the light backdrop and matches the theme.
const COLOR_PRESETS: [Color32; 6] = [
    Color32::from_rgb(0x26, 0x8b, 0xd2), // blue
    Color32::from_rgb(0xdc, 0x32, 0x2f), // red
    Color32::from_rgb(0x85, 0x99, 0x00), // green
    Color32::from_rgb(0xcb, 0x4b, 0x16), // orange
    Color32::from_rgb(0xd3, 0x36, 0x82), // magenta
    Color32::from_rgb(0x58, 0x6e, 0x75), // base01 (dark gray for text)
];

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
    /// Text tool (in-place single-line text, baked via ab_glyph Geist Sans).
    Text,
    /// Gaussian blur applied to the dragged region (drag a box, release).
    Blur,
    /// Pixelate (mosaic) applied to the dragged region (drag a box, release).
    Mosaic,
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
    ToggleBlur,
    ToggleMosaic,
    SetColor(Color32),
    SetWidth(f32),
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
    /// Session-global annotation color (applies to Rect/Arrow/Pencil/Text).
    pub active_color: Color32,
    /// Session-global annotation stroke width in logical px (Rect/Arrow/Pencil).
    pub active_width: f32,
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
            active_color: ACTIVE_COLOR_DEFAULT,
            active_width: ACTIVE_WIDTH_DEFAULT,
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

    /// Bake the in-progress text into the document at its anchor as
    /// active-color Geist glyphs (anti-aliased via ab_glyph), push it via
    /// [`Self::commit_doc`], then clear all text state. An empty buffer is a
    /// no-op (just clears state).
    fn commit_text(&mut self, ctx: &egui::Context) {
        let anchor = self.text_anchor;
        let drect = self.doc_rect;
        let scale = self.scale;
        let color = self.active_color;
        let buf = self.text_buf.trim().to_string();
        let base = self.doc.clone();
        if let (Some(anchor), Some(drect), Some(base)) = (anchor, drect, base) {
            if !buf.is_empty() {
                // Anchor → doc physical px (top-left of the text box). The live
                // preview's `TextEdit` sits inside a Frame::group with an
                // inner_margin, so the visible glyphs are offset down from the
                // anchor by that margin (+ a little row padding). Nudge the
                // baked baseline down by the frame's inner_margin (4 logical
                // px) so the committed text lines up with what the user saw.
                const TEXT_PREVIEW_INSET: f32 = 4.0;
                let ax = ((anchor.x - drect.min.x) * scale).round() as i32;
                let ay = ((anchor.y - drect.min.y + TEXT_PREVIEW_INSET) * scale).round() as i32;
                let px_size = TEXT_LOGICAL_SIZE * scale;
                let mut nd = base;
                match rasterize_text(&mut nd, ax, ay, &buf, px_size, color, GEIST_FONT_BYTES) {
                    Ok(()) => self.commit_doc(ctx, nd),
                    Err(_) => {
                        tracing::warn!("Geist font parse failed while committing text; skipping")
                    }
                }
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
    active_color: Color32,
    active_width: f32,
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
            active_color: g.active_color,
            active_width: g.active_width,
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
            // 1. frozen screenshot + 2. dim everything (light scrim so the
            //    spotlighted selection + annotations pop without pure-black ugly).
            painter.image(r.tex_id, screen, full_uv, Color32::WHITE);
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(90));

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
                    // Dark Solarized base03 — readable on the light scrim.
                    Color32::from_rgb(0x00, 0x2b, 0x36),
                    format!("{} × {}", pw, ph),
                );
            } else {
                painter.debug_text(
                    screen.center(),
                    Align2::CENTER_CENTER,
                    // Dark Solarized base03 — readable on the light scrim.
                    Color32::from_rgb(0x00, 0x2b, 0x36),
                    "Drag to select · click a window to snap · Esc to cancel",
                );
            }
        }
        Phase::Edit => {
            // Dimmed full screenshot, then the doc at full brightness in its
            // original position (spotlight). Light scrim matches the Select phase.
            painter.image(r.tex_id, screen, full_uv, Color32::WHITE);
            painter.rect_filled(screen, 0.0, Color32::from_black_alpha(90));
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
                        paint_rect_preview(&painter, ds, dc, r.active_color, r.active_width);
                    }
                }
                Tool::Arrow => {
                    if let (Some(ds), Some(dc)) = (r.tool_drag_start, r.tool_drag_cur) {
                        paint_arrow_preview(&painter, ds, dc, r.active_color, r.active_width);
                    }
                }
                Tool::Pencil => {
                    if r.pencil_points.len() >= 2 {
                        let stroke = egui::epaint::PathStroke::new(r.active_width, r.active_color);
                        painter.line(r.pencil_points.clone(), stroke);
                    }
                }
                Tool::Blur | Tool::Mosaic => {
                    if let (Some(ds), Some(dc)) = (r.tool_drag_start, r.tool_drag_cur) {
                        paint_region_preview(&painter, ds, dc);
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
            let active_color = r.active_color;
            let active_width = r.active_width;
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
                                if tool_button(ui, "Blur", is_active(Tool::Blur)) {
                                    action = Some(ToolbarAction::ToggleBlur);
                                }
                                if tool_button(ui, "Mosaic", is_active(Tool::Mosaic)) {
                                    action = Some(ToolbarAction::ToggleMosaic);
                                }
                                // Color + Size popovers (session-global annotation styling).
                                if let Some(a) = color_picker_button(ui, active_color) {
                                    action = Some(a);
                                }
                                if let Some(a) = size_picker_button(ui, active_width) {
                                    action = Some(a);
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
                                    .text_color(r.active_color),
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
                Some(ToolbarAction::ToggleBlur) => {
                    g.cancel_in_progress();
                    g.active_tool = if g.active_tool == Tool::Blur {
                        Tool::None
                    } else {
                        Tool::Blur
                    };
                }
                Some(ToolbarAction::ToggleMosaic) => {
                    g.cancel_in_progress();
                    g.active_tool = if g.active_tool == Tool::Mosaic {
                        Tool::None
                    } else {
                        Tool::Mosaic
                    };
                }
                Some(ToolbarAction::SetColor(c)) => {
                    g.active_color = c;
                }
                Some(ToolbarAction::SetWidth(w)) => {
                    g.active_width = w;
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
                Tool::Rect | Tool::Arrow | Tool::Blur | Tool::Mosaic => {
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
                                        match g.active_tool {
                                            Tool::Rect => {
                                                let mut nd = base;
                                                rasterize_rect(
                                                    &mut nd,
                                                    drect.min,
                                                    ds,
                                                    dc,
                                                    g.scale,
                                                    g.active_color,
                                                    g.active_width,
                                                );
                                                g.commit_doc(&ctx, nd);
                                            }
                                            Tool::Arrow => {
                                                let mut nd = base;
                                                rasterize_arrow(
                                                    &mut nd,
                                                    drect.min,
                                                    ds,
                                                    dc,
                                                    g.scale,
                                                    g.active_color,
                                                    g.active_width,
                                                );
                                                g.commit_doc(&ctx, nd);
                                            }
                                            Tool::Blur => {
                                                let (dw, dh) =
                                                    (base.width() as i32, base.height() as i32);
                                                if let Some((x0, y0, x1, y1)) = drag_region_physical(
                                                    drect.min, ds, dc, g.scale, dw, dh,
                                                ) {
                                                    // 5 logical px of blur, in physical px.
                                                    let sigma = (5.0 * g.scale).max(1.0);
                                                    let nd = apply_blur_region(
                                                        &base, x0, y0, x1, y1, sigma,
                                                    );
                                                    g.commit_doc(&ctx, nd);
                                                }
                                            }
                                            Tool::Mosaic => {
                                                let (dw, dh) =
                                                    (base.width() as i32, base.height() as i32);
                                                if let Some((x0, y0, x1, y1)) = drag_region_physical(
                                                    drect.min, ds, dc, g.scale, dw, dh,
                                                ) {
                                                    // 10 logical px blocks, in physical px.
                                                    let block =
                                                        ((10.0 * g.scale).round() as u32).max(2);
                                                    let nd = apply_mosaic_region(
                                                        &base, x0, y0, x1, y1, block,
                                                    );
                                                    g.commit_doc(&ctx, nd);
                                                }
                                            }
                                            _ => {}
                                        }
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
                                rasterize_pencil(
                                    &mut nd,
                                    drect.min,
                                    &g.pencil_points,
                                    g.scale,
                                    g.active_color,
                                    g.active_width,
                                );
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

/// Live rectangle preview: a single outline in the active color/width.
fn paint_rect_preview(painter: &egui::Painter, ds: Pos2, dc: Pos2, color: Color32, width: f32) {
    let rect = Rect::from_two_pos(ds, dc);
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(width, color),
        egui::epaint::StrokeKind::Inside,
    );
}

/// Live arrow preview: a single-color shaft + chevron head (tip at the current
/// pointer) in the active color/width.
fn paint_arrow_preview(painter: &egui::Painter, ds: Pos2, dc: Pos2, color: Color32, width: f32) {
    let stroke = Stroke::new(width, color);
    painter.line_segment([ds, dc], stroke);
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
            painter.line_segment([dc, barb], stroke);
        }
    }
}

/// Live preview for the Blur / Mosaic tools: a semi-transparent accent fill
/// plus a bright accent outline over the dragged region, so the user sees the
/// target box that will be affected. Painter-only (no pixel work) so it stays
/// cheap per frame.
fn paint_region_preview(painter: &egui::Painter, ds: Pos2, dc: Pos2) {
    let rect = Rect::from_two_pos(ds, dc);
    painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(71, 168, 255, 48));
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(2.0, crate::ui::theme::ACCENT_BLUE_BRIGHT),
        egui::epaint::StrokeKind::Inside,
    );
}

/// A tool-toggle button: accent fill (dark text) when active, secondary style
/// otherwise. Matches the existing Rect button treatment, generalized.
fn tool_button(ui: &mut egui::Ui, label: &'static str, active: bool) -> bool {
    let btn = if active {
        egui::Button::new(egui::RichText::new(label).color(Color32::WHITE))
            .fill(crate::ui::theme::ACCENT_BLUE_BRIGHT)
            .corner_radius(egui::CornerRadius::same(crate::ui::theme::RADIUS_SM))
            .min_size(egui::vec2(0.0, 36.0))
    } else {
        crate::ui::theme::secondary_button(label)
    };
    ui.add(btn).clicked()
}

/// Pick a legible text color (black/white) for a button filled with `c`.
fn contrast_text_color(c: Color32) -> Color32 {
    // Rec. 709 luma: dark fills → white text, light fills → black text, so the
    // Color trigger stays readable for every preset (incl. black & white).
    let luma = 0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32;
    if luma > 140.0 {
        Color32::BLACK
    } else {
        Color32::WHITE
    }
}

/// A filled square swatch used inside the Color popover; the active swatch gets
/// an accent outline.
fn color_swatch_button(c: Color32, selected: bool) -> egui::Button<'static> {
    let stroke = if selected {
        egui::Stroke::new(2.0, crate::ui::theme::ACCENT_BLUE_BRIGHT)
    } else {
        egui::Stroke::new(1.0, crate::ui::theme::BORDER_STRONG)
    };
    egui::Button::new("")
        .fill(c)
        .stroke(stroke)
        .min_size(egui::vec2(22.0, 22.0))
        .corner_radius(egui::CornerRadius::same(crate::ui::theme::RADIUS_SM))
}

/// Color popover: a trigger button filled with the active color (contrast label)
/// that opens a 6-swatch picker. Returns `SetColor(c)` if a swatch was clicked.
/// Rendered inline in the toolbar's horizontal flow; the popup floats above the
/// canvas, so its clicks never reach the editor canvas interaction.
fn color_picker_button(ui: &mut egui::Ui, active_color: Color32) -> Option<ToolbarAction> {
    let trigger =
        egui::Button::new(egui::RichText::new("Color").color(contrast_text_color(active_color)))
            .fill(active_color)
            .stroke(egui::Stroke::new(1.0, crate::ui::theme::BORDER_STRONG))
            .min_size(egui::vec2(0.0, 36.0))
            .corner_radius(egui::CornerRadius::same(crate::ui::theme::RADIUS_SM));
    let resp = ui.add(trigger);
    let mut picked = None;
    egui::Popup::from_toggle_button_response(&resp).show(|ui| {
        ui.horizontal(|ui| {
            for c in COLOR_PRESETS {
                if ui.add(color_swatch_button(c, c == active_color)).clicked() {
                    picked = Some(ToolbarAction::SetColor(c));
                }
            }
        });
    });
    picked
}

/// Size popover: a "Size" trigger opening 1/2/4-px width presets; the active
/// width is marked with a checkmark. Returns `SetWidth(w)` on selection.
fn size_picker_button(ui: &mut egui::Ui, active_width: f32) -> Option<ToolbarAction> {
    let resp = ui.add(crate::ui::theme::secondary_button("Size"));
    let mut picked = None;
    egui::Popup::from_toggle_button_response(&resp).show(|ui| {
        ui.set_min_width(54.0);
        for &w in &[1.0_f32, 2.0, 4.0] {
            let active = (active_width - w).abs() < 0.01;
            let label = if active {
                format!("{w:.0} px  ✓")
            } else {
                format!("{w:.0} px")
            };
            if ui.add(crate::ui::theme::secondary_button(label)).clicked() {
                picked = Some(ToolbarAction::SetWidth(w));
            }
        }
    });
    picked
}

/// Pick a toolbar position just outside the selection's bottom-right corner,
/// flipping to the opposite side (above-left) when it would run off-screen.
/// Uses a rough toolbar size estimate; it only needs to be approximately right.
fn toolbar_position(sel: Rect, screen: Rect) -> Pos2 {
    // ~14 buttons * ~56pt each, ~48pt tall with padding (rough, for flipping).
    let tb_w = 14.0 * 56.0;
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

/// Rasterize a rectangle stroke onto `doc` (physical pixels) as a single
/// `color` outline of `width` logical px (→ `width*scale` physical, min 1).
/// `sel_min_logical` is the doc's top-left in overlay-local logical coords;
/// `drag_start` / `drag_cur` are the stroke's corners in the same space. No
/// dark backing — a pure single-pass outline in the active color.
fn rasterize_rect(
    doc: &mut RgbaImage,
    sel_min_logical: Pos2,
    drag_start: Pos2,
    drag_cur: Pos2,
    scale: f32,
    color: Color32,
    width: f32,
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

    let t = ((width * scale).round() as i32).max(1);
    let c = [color.r(), color.g(), color.b(), color.a()];
    fill_border_ring(doc, rx0, ry0, rx1, ry1, t, c);
}

/// Rasterize an arrow (shaft + chevron head) onto `doc` as a single
/// `color`/`width`-logical-px pass. Thickness = `width*scale` physical (min 1);
/// the head is drawn the same way at the tip (`drag_cur`), pointing back toward
/// `drag_start`. No dark backing.
fn rasterize_arrow(
    doc: &mut RgbaImage,
    sel_min_logical: Pos2,
    drag_start: Pos2,
    drag_cur: Pos2,
    scale: f32,
    color: Color32,
    width: f32,
) {
    let to_px = |p: Pos2| -> (i32, i32) {
        (
            ((p.x - sel_min_logical.x) * scale).round() as i32,
            ((p.y - sel_min_logical.y) * scale).round() as i32,
        )
    };
    let (x0, y0) = to_px(drag_start);
    let (x1, y1) = to_px(drag_cur);
    let t = ((width * scale).round() as i32).max(1);
    let c = [color.r(), color.g(), color.b(), color.a()];

    rasterize_thick_line(doc, x0, y0, x1, y1, t, c);
    // Head at the tip (x1,y1); "base" direction is back toward (x0,y0).
    rasterize_arrowhead(doc, x1, y1, x0, y0, t, c, scale);
}

/// Rasterize a freehand polyline (the Pencil commit core) as a single
/// `color`/`width`-logical-px thick polyline, segment by segment. No backing.
fn rasterize_pencil(
    doc: &mut RgbaImage,
    sel_min_logical: Pos2,
    points: &[Pos2],
    scale: f32,
    color: Color32,
    width: f32,
) {
    let to_px = |p: Pos2| -> (i32, i32) {
        (
            ((p.x - sel_min_logical.x) * scale).round() as i32,
            ((p.y - sel_min_logical.y) * scale).round() as i32,
        )
    };
    let pts: Vec<(i32, i32)> = points.iter().map(|p| to_px(*p)).collect();
    let t = ((width * scale).round() as i32).max(1);
    let c = [color.r(), color.g(), color.b(), color.a()];
    rasterize_polyline(doc, &pts, t, c);
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

// --- Text (ab_glyph + bundled Geist Sans) ----------------------------------

/// Rasterize `text` at the doc-physical anchor `(doc_x0, doc_y0)` (top-left of
/// the text box) using the bundled Geist font, at `px_size` physical px, as
/// anti-aliased `color` glyphs alpha-blended over the existing pixels. Empty
/// text is a no-op. Every glyph pixel is bounds-checked (off-canvas glyphs are
/// skipped, never panic). Returns `Err` only if the font bytes fail to parse.
fn rasterize_text(
    doc: &mut RgbaImage,
    doc_x0: i32,
    doc_y0: i32,
    text: &str,
    px_size: f32,
    color: Color32,
    font_bytes: &[u8],
) -> Result<(), ab_glyph::InvalidFont> {
    if text.is_empty() {
        return Ok(());
    }
    let font = FontVec::try_from_vec_and_index(font_bytes.to_vec(), 0)?;
    let scale = PxScale::from(px_size.max(1.0));
    let scaled = font.as_scaled(scale);
    let (dw, dh) = (doc.width() as i32, doc.height() as i32);
    let cr = color.r();
    let cg = color.g();
    let cb = color.b();
    // Pen starts at the box top-left; y = ascent() places the baseline correctly
    // (glyphs hang below the pen y by their descent).
    let mut x = 0.0;
    let y = scaled.ascent();
    for ch in text.chars() {
        let glyph_id = scaled.glyph_id(ch);
        let glyph = glyph_id.with_scale_and_position(scale, Point { x, y });
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            let min_x = bounds.min.x.round() as i32;
            let min_y = bounds.min.y.round() as i32;
            outlined.draw(|gx, gy, coverage: f32| {
                if coverage <= 0.0 {
                    return;
                }
                let px = doc_x0 + min_x + gx as i32;
                let py = doc_y0 + min_y + gy as i32;
                if px >= 0 && py >= 0 && px < dw && py < dh {
                    let a = coverage.clamp(0.0, 1.0);
                    let ia = 1.0 - a;
                    let p = doc.get_pixel_mut(px as u32, py as u32);
                    // Alpha-over compositing of the glyph color over the bg.
                    p[0] = (cr as f32 * a + p[0] as f32 * ia).round() as u8;
                    p[1] = (cg as f32 * a + p[1] as f32 * ia).round() as u8;
                    p[2] = (cb as f32 * a + p[2] as f32 * ia).round() as u8;
                    p[3] = 255;
                }
            });
        }
        x += scaled.h_advance(glyph_id);
    }
    Ok(())
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

/// Map a logical drag rect (overlay-local) to a clamped physical-pixel
/// half-open region `[x0, x1) × [y0, y1)` of a `doc_w × doc_h` document, using
/// the same corner-rounding math as [`rasterize_rect`]. Returns `None` if the
/// region is degenerate (≤ 1 px wide/tall) or ends up entirely outside the doc
/// after clamping — the caller treats that as a no-op (no commit).
fn drag_region_physical(
    sel_min_logical: Pos2,
    a: Pos2,
    b: Pos2,
    scale: f32,
    doc_w: i32,
    doc_h: i32,
) -> Option<(i32, i32, i32, i32)> {
    let ax = ((a.x - sel_min_logical.x) * scale).round() as i32;
    let bx = ((b.x - sel_min_logical.x) * scale).round() as i32;
    let ay = ((a.y - sel_min_logical.y) * scale).round() as i32;
    let by = ((b.y - sel_min_logical.y) * scale).round() as i32;
    let x0 = ax.min(bx).max(0);
    let x1 = ax.max(bx).min(doc_w);
    let y0 = ay.min(by).max(0);
    let y1 = ay.max(by).min(doc_h);
    if x1 <= x0 + 1 || y1 <= y0 + 1 {
        None
    } else {
        Some((x0, y0, x1, y1))
    }
}

/// Apply a gaussian blur to the half-open region `[x0, x1) × [y0, y1)` of `doc`
/// (physical px) and return the resulting document. `sigma` is in physical px.
/// The region is clamped to doc bounds first; a degenerate region (`x1 <= x0`
/// or `y1 <= y0`, including fully-off-canvas) is a no-op returning a clone of
/// the input. Only the region's pixels change — everything outside is verbatim.
fn apply_blur_region(doc: &RgbaImage, x0: i32, y0: i32, x1: i32, y1: i32, sigma: f32) -> RgbaImage {
    let (w, h) = (doc.width() as i32, doc.height() as i32);
    let x0 = x0.max(0).min(w);
    let x1 = x1.max(0).min(w);
    let y0 = y0.max(0).min(h);
    let y1 = y1.max(0).min(h);
    if x1 <= x0 || y1 <= y0 {
        return doc.clone();
    }
    let (rw, rh) = ((x1 - x0) as u32, (y1 - y0) as u32);
    let sub = image::imageops::crop_imm(doc, x0 as u32, y0 as u32, rw, rh).to_image();
    let blurred = image::imageops::blur(&sub, sigma);
    let mut nd = doc.clone();
    image::imageops::replace(&mut nd, &blurred, x0 as i64, y0 as i64);
    nd
}

/// Pixelate (mosaic) the half-open region `[x0, x1) × [y0, y1)` of `doc`
/// (physical px) using `block`-px blocks (downsample to `rw/block × rh/block`
/// with Nearest, then upsample back with Nearest for the classic blocky look)
/// and return the resulting document. The region is clamped to doc bounds first;
/// a degenerate region (`x1 <= x0` or `y1 <= y0`, including fully-off-canvas)
/// is a no-op returning a clone of the input. Only the region changes.
fn apply_mosaic_region(
    doc: &RgbaImage,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    block: u32,
) -> RgbaImage {
    let (w, h) = (doc.width() as i32, doc.height() as i32);
    let x0 = x0.max(0).min(w);
    let x1 = x1.max(0).min(w);
    let y0 = y0.max(0).min(h);
    let y1 = y1.max(0).min(h);
    if x1 <= x0 || y1 <= y0 {
        return doc.clone();
    }
    let (rw, rh) = ((x1 - x0) as u32, (y1 - y0) as u32);
    let block = block.max(1);
    let new_w = (rw / block).max(1);
    let new_h = (rh / block).max(1);
    let sub = image::imageops::crop_imm(doc, x0 as u32, y0 as u32, rw, rh).to_image();
    let small = image::imageops::resize(&sub, new_w, new_h, image::imageops::FilterType::Nearest);
    let big = image::imageops::resize(&small, rw, rh, image::imageops::FilterType::Nearest);
    let mut nd = doc.clone();
    image::imageops::replace(&mut nd, &big, x0 as i64, y0 as i64);
    nd
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- fill_border_ring / rasterize_rect (existing, kept) -----------------

    #[test]
    fn rasterize_rect_draws_single_color_outline() {
        // 12x8 doc, rect covering the whole doc, scale 1, width 2 → a 2px red
        // ring; the interior mid-gray stays untouched (no dark backing anymore).
        let mut doc = image::RgbaImage::new(12, 8);
        for p in doc.pixels_mut() {
            *p = image::Rgba([128, 128, 128, 255]);
        }
        rasterize_rect(
            &mut doc,
            Pos2::new(0.0, 0.0),
            Pos2::new(0.0, 0.0),
            Pos2::new(12.0, 8.0),
            1.0,
            Color32::from_rgb(220, 0, 0),
            2.0,
        );
        // Border ring (width 2 at scale 1) is red; corners + edges red.
        assert_eq!(*doc.get_pixel(0, 0), image::Rgba([220, 0, 0, 255]));
        assert_eq!(*doc.get_pixel(1, 1), image::Rgba([220, 0, 0, 255]));
        assert_eq!(*doc.get_pixel(11, 7), image::Rgba([220, 0, 0, 255]));
        // Interior is untouched mid-gray (no backing pass anymore).
        assert_eq!(*doc.get_pixel(6, 4), image::Rgba([128, 128, 128, 255]));
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

    // --- rasterize_text (ab_glyph Geist) -------------------------------------

    #[test]
    fn text_writes_colored_glyph_pixels() {
        // Black doc; bake "Hello" in red at 24px. Some fully-covered (== red)
        // glyph pixels must appear; background far from glyphs stays black.
        let mut doc = image::RgbaImage::new(140, 40);
        for p in doc.pixels_mut() {
            *p = image::Rgba([0, 0, 0, 255]);
        }
        let color = Color32::from_rgb(220, 0, 0);
        rasterize_text(&mut doc, 4, 4, "Hello", 24.0, color, GEIST_FONT_BYTES).unwrap();
        let exact_red = image::Rgba([220, 0, 0, 255]);
        assert!(
            doc.pixels().any(|p| *p == exact_red),
            "expected at least one fully-covered red glyph pixel"
        );
        // Background pixels far from the glyphs (bottom-right) stay black.
        assert_eq!(*doc.get_pixel(138, 38), image::Rgba([0, 0, 0, 255]));
    }

    #[test]
    fn text_uses_active_color_not_white() {
        // A non-red color bakes in that exact color, confirming color threading
        // (the old renderer was hard-coded white).
        let mut doc = image::RgbaImage::new(140, 40);
        for p in doc.pixels_mut() {
            *p = image::Rgba([0, 0, 0, 255]);
        }
        let color = Color32::from_rgb(46, 204, 113); // green
        rasterize_text(&mut doc, 4, 4, "Hi", 24.0, color, GEIST_FONT_BYTES).unwrap();
        let exact_green = image::Rgba([46, 204, 113, 255]);
        assert!(
            doc.pixels().any(|p| *p == exact_green),
            "expected fully-covered green glyph pixels"
        );
    }

    #[test]
    fn text_empty_is_noop() {
        let mut doc = image::RgbaImage::new(40, 40);
        let before = doc.clone();
        rasterize_text(&mut doc, 0, 0, "", 24.0, Color32::WHITE, GEIST_FONT_BYTES).unwrap();
        assert_eq!(doc, before);
    }

    #[test]
    fn text_off_canvas_anchor_is_clamped_no_panic() {
        let mut doc = image::RgbaImage::new(20, 20);
        let before = doc.clone();
        // Anchor far off the top-left → every glyph pixel lands off-canvas and
        // is skipped; nothing is drawn and nothing panics.
        rasterize_text(
            &mut doc,
            -500,
            -500,
            "Hi",
            24.0,
            Color32::WHITE,
            GEIST_FONT_BYTES,
        )
        .unwrap();
        assert_eq!(doc, before);
    }

    #[test]
    fn text_keeps_pixels_inside_doc_bounds() {
        // Anchor near the right edge: some glyphs would overflow horizontally,
        // but every written pixel must stay within the doc (no panic, no OOB).
        let mut doc = image::RgbaImage::new(30, 40);
        for p in doc.pixels_mut() {
            *p = image::Rgba([0, 0, 0, 255]);
        }
        rasterize_text(
            &mut doc,
            24,
            4,
            "Hello",
            24.0,
            Color32::WHITE,
            GEIST_FONT_BYTES,
        )
        .unwrap();
        // First column (x=0) is left of the anchor and stays black.
        assert_eq!(*doc.get_pixel(0, 10), image::Rgba([0, 0, 0, 255]));
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
            Color32::from_rgb(220, 0, 0),
            2.0,
        );
        // Tip region near (30,10) should have some red pixels.
        let mut found_red = false;
        for y in 7..14 {
            for x in 27..34 {
                if *doc.get_pixel(x as u32, y as u32) == image::Rgba([220, 0, 0, 255]) {
                    found_red = true;
                }
            }
        }
        assert!(found_red, "expected red pixels near the arrow tip");
    }

    // --- apply_blur_region / apply_mosaic_region ----------------------------

    /// 20×20 black doc with a 2×2 white block at (6,6)-(7,7): a sharp feature
    /// inside the region so blur provably spreads it.
    fn block_doc() -> RgbaImage {
        let mut doc = RgbaImage::new(20, 20);
        for p in doc.pixels_mut() {
            *p = image::Rgba([0, 0, 0, 255]);
        }
        for y in 6..8 {
            for x in 6..8 {
                doc.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        doc
    }

    /// True if any pixel strictly inside the half-open region differs between
    /// `a` and `b`.
    fn differs_inside(a: &RgbaImage, b: &RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32) -> bool {
        for y in y0..y1 {
            for x in x0..x1 {
                if a.get_pixel(x, y) != b.get_pixel(x, y) {
                    return true;
                }
            }
        }
        false
    }

    /// True if EVERY pixel outside the region is identical between `a` and `b`.
    fn same_outside(a: &RgbaImage, b: &RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32) -> bool {
        let (w, h) = (a.width(), a.height());
        for y in 0..h {
            for x in 0..w {
                let inside = (x0..x1).contains(&x) && (y0..y1).contains(&y);
                if !inside && a.get_pixel(x, y) != b.get_pixel(x, y) {
                    return false;
                }
            }
        }
        true
    }

    #[test]
    fn blur_changes_region_only() {
        let doc = block_doc();
        // Blur the region [2,2,12,12) (physical px). The 2×2 white block sits
        // inside it, so the blurred output differs inside and is verbatim
        // outside.
        let out = apply_blur_region(&doc, 2, 2, 12, 12, 2.0);
        assert!(
            differs_inside(&doc, &out, 2, 2, 12, 12),
            "region should change"
        );
        assert!(
            same_outside(&doc, &out, 2, 2, 12, 12),
            "outside must be unchanged"
        );
        // The block's core (6,6) was pure white; blur softens it toward gray.
        assert_ne!(*out.get_pixel(6, 6), *doc.get_pixel(6, 6));
        // An outside pixel is untouched black.
        assert_eq!(*out.get_pixel(0, 0), *doc.get_pixel(0, 0));
    }

    #[test]
    fn blur_degenerate_region_is_noop() {
        let doc = block_doc();
        // x1 == x0 → degenerate.
        assert_eq!(apply_blur_region(&doc, 5, 5, 5, 10, 2.0), doc);
        // y1 == y0 → degenerate.
        assert_eq!(apply_blur_region(&doc, 5, 5, 10, 5, 2.0), doc);
    }

    #[test]
    fn blur_off_canvas_region_is_noop() {
        let doc = block_doc();
        // Entirely off the right/bottom of a 20×20 doc → clamps to (20,20,30,30)
        // → degenerate → no-op clone.
        assert_eq!(apply_blur_region(&doc, 100, 100, 110, 110, 2.0), doc);
    }

    #[test]
    fn blur_inverted_corners_still_works() {
        let doc = block_doc();
        // x1 < x0: the free function does NOT swap (contract is x0<=x1), so a
        // strictly inverted region is treated as degenerate (no-op). Document
        // that behavior so future refactors don't silently change it.
        assert_eq!(apply_blur_region(&doc, 11, 11, 2, 2, 2.0), doc);
    }

    #[test]
    fn mosaic_changes_region_only() {
        let mut doc = RgbaImage::new(20, 20);
        for p in doc.pixels_mut() {
            *p = image::Rgba([0, 0, 0, 255]); // black everywhere outside
        }
        // Inside region [4,4,16,16): a (x+y)-parity checkerboard of black/white.
        for y in 4..16 {
            for x in 4..16 {
                let c = if (x + y) % 2 == 0 { 255 } else { 0 };
                doc.put_pixel(x, y, image::Rgba([c, c, c, 255]));
            }
        }
        let out = apply_mosaic_region(&doc, 4, 4, 16, 16, 4);
        assert!(
            differs_inside(&doc, &out, 4, 4, 16, 16),
            "region should change"
        );
        assert!(
            same_outside(&doc, &out, 4, 4, 16, 16),
            "outside must be unchanged"
        );
        // Mosaic collapses the fine checkerboard into coarse blocks: a black
        // checker pixel inside the region must have changed.
        assert_ne!(*out.get_pixel(5, 4), *doc.get_pixel(5, 4));
    }

    #[test]
    fn mosaic_produces_uniform_blocks() {
        // A region split into 4-px blocks should have each block be a solid
        // color: horizontally-adjacent pixels in the same block row-band are
        // equal after upsampling.
        let mut doc = RgbaImage::new(20, 8);
        for p in doc.pixels_mut() {
            *p = image::Rgba([0, 0, 0, 255]);
        }
        // Left half white, right half black → strong vertical edge in region.
        for y in 0..8 {
            for x in 0..10 {
                doc.put_pixel(x, y, image::Rgba([255, 255, 255, 255]));
            }
        }
        let out = apply_mosaic_region(&doc, 0, 0, 20, 8, 4);
        // The first 4 columns are sampled from the white half (src x in 0..4)
        // → all equal (white). Columns 0..4 share one mosaic value.
        let p0 = *out.get_pixel(0, 0);
        for x in 0..4 {
            for y in 0..8 {
                assert_eq!(*out.get_pixel(x, y), p0, "block pixel ({x},{y}) differs");
            }
        }
    }

    #[test]
    fn mosaic_degenerate_region_is_noop() {
        let doc = block_doc();
        assert_eq!(apply_mosaic_region(&doc, 5, 5, 5, 10, 4), doc);
        assert_eq!(apply_mosaic_region(&doc, 5, 5, 10, 5, 4), doc);
    }

    #[test]
    fn mosaic_off_canvas_region_is_noop() {
        let doc = block_doc();
        assert_eq!(apply_mosaic_region(&doc, 100, 100, 110, 110, 4), doc);
    }

    #[test]
    fn drag_region_physical_clamps_and_detects_degenerate() {
        // 20×20 doc, scale 1. A clean 5×5 box at origin maps to (0,0,5,5).
        let r = drag_region_physical(
            Pos2::new(0.0, 0.0),
            Pos2::new(0.0, 0.0),
            Pos2::new(5.0, 5.0),
            1.0,
            20,
            20,
        );
        assert_eq!(r, Some((0, 0, 5, 5)));
        // Dragged mostly off the bottom-right: clamps to the doc edge and is
        // still a valid region.
        let r = drag_region_physical(
            Pos2::new(0.0, 0.0),
            Pos2::new(18.0, 18.0),
            Pos2::new(40.0, 40.0),
            1.0,
            20,
            20,
        );
        assert_eq!(r, Some((18, 18, 20, 20)));
        // Fully off-canvas → degenerate (None).
        let r = drag_region_physical(
            Pos2::new(0.0, 0.0),
            Pos2::new(30.0, 30.0),
            Pos2::new(40.0, 40.0),
            1.0,
            20,
            20,
        );
        assert_eq!(r, None);
        // Off-canvas origin but the drag re-enters the doc → valid clamped.
        let r = drag_region_physical(
            Pos2::new(0.0, 0.0),
            Pos2::new(-5.0, -5.0),
            Pos2::new(5.0, 5.0),
            1.0,
            20,
            20,
        );
        assert_eq!(r, Some((0, 0, 5, 5)));
    }
}
