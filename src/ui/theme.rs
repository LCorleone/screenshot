//! Geist design system theme for screenshot-dai's egui UI.
//!
//! Installs Geist fonts, text styles, spacing and a dark Geist color palette
//! onto an [`egui::Context`]. Also provides styled primary/secondary button
//! builders so call sites stay declarative.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
};
use std::sync::Arc;

// --- Geist Dark tokens ------------------------------------------------------

/// background-100
pub const BG: Color32 = Color32::from_rgb(0x00, 0x00, 0x00);
/// subtle card surface
pub const SURFACE: Color32 = Color32::from_rgb(0x0a, 0x0a, 0x0a);
/// gray-1000 — primary text
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xed, 0xed, 0xed);
/// gray-900 — secondary text
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xa0, 0xa0, 0xa0);
/// gray-700 — disabled text
#[allow(dead_code)]
pub const TEXT_DISABLED: Color32 = Color32::from_rgb(0x8f, 0x8f, 0x8f);
/// gray-alpha-400 — default border (translucent white)
pub const BORDER: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x24);
/// gray-alpha-600 — emphasized border (translucent white)
pub const BORDER_STRONG: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x82);
/// blue-700 (dark) — selection / active accent
pub const ACCENT_BLUE: Color32 = Color32::from_rgb(0x00, 0x6e, 0xfe);
/// blue-900 (dark) — bright accent (links, active fg)
pub const ACCENT_BLUE_BRIGHT: Color32 = Color32::from_rgb(0x47, 0xa8, 0xff);
/// red-600 — errors
pub const ERROR: Color32 = Color32::from_rgb(0xf3, 0x2e, 0x40);
/// green-600 — success
pub const SUCCESS: Color32 = Color32::from_rgb(0x4c, 0xe1, 0x5e);

/// Geist `sm` — controls/buttons/inputs. `CornerRadius` uses `u8` in egui 0.34.
pub const RADIUS_SM: u8 = 6;
/// Geist `lg` — fullscreen surfaces (preview frame).
pub const RADIUS_LG: u8 = 16;

/// Name of the named `FontFamily` resolving to Geist Medium (button text).
pub const FAMILY_MEDIUM: &str = "GeistSansMedium";
/// Name of the named `FontFamily` resolving to Geist SemiBold (headings).
pub const FAMILY_SEMIBOLD: &str = "GeistSansSemiBold";

/// `FontId` for a small section header (Geist SemiBold 15px).
/// Use this directly instead of a named `TextStyle`, which can panic if the
/// style isn't registered on the active `Ui`.
pub fn section_font() -> egui::FontId {
    egui::FontId::new(15.0, egui::FontFamily::Name(FAMILY_SEMIBOLD.into()))
}

/// `FontId` for a caption (Geist Regular 12px).
pub fn caption_font() -> egui::FontId {
    egui::FontId::new(12.0, egui::FontFamily::Proportional)
}

/// Install Geist fonts → text styles/spacing → dark visuals onto `ctx`.
pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    install_style(ctx);
    install_visuals(ctx);
}

fn install_fonts(ctx: &egui::Context) {
    let mut fd = FontDefinitions::default();

    fd.font_data.insert(
        "Geist-Regular".into(),
        Arc::new(FontData::from_owned(
            include_bytes!("../../assets/fonts/Geist-Regular.ttf").to_vec(),
        )),
    );
    fd.font_data.insert(
        "Geist-Medium".into(),
        Arc::new(FontData::from_owned(
            include_bytes!("../../assets/fonts/Geist-Medium.ttf").to_vec(),
        )),
    );
    fd.font_data.insert(
        "Geist-SemiBold".into(),
        Arc::new(FontData::from_owned(
            include_bytes!("../../assets/fonts/Geist-SemiBold.ttf").to_vec(),
        )),
    );
    fd.font_data.insert(
        "GeistMono-Regular".into(),
        Arc::new(FontData::from_owned(
            include_bytes!("../../assets/fonts/GeistMono-Regular.ttf").to_vec(),
        )),
    );

    // Override the default Proportional / Monospace stacks so Geist wins.
    fd.families
        .get_mut(&FontFamily::Proportional)
        .unwrap()
        .insert(0, "Geist-Regular".into());
    fd.families
        .get_mut(&FontFamily::Monospace)
        .unwrap()
        .insert(0, "GeistMono-Regular".into());

    // Named families for weight-specific text styles.
    fd.families
        .entry(FontFamily::Name(FAMILY_MEDIUM.into()))
        .or_default()
        .push("Geist-Medium".into());
    fd.families
        .entry(FontFamily::Name(FAMILY_SEMIBOLD.into()))
        .or_default()
        .push("Geist-SemiBold".into());

    ctx.set_fonts(fd);
}

fn install_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();

    style
        .text_styles
        .insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(
        TextStyle::Small,
        FontId::new(12.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        TextStyle::Button,
        FontId::new(14.0, FontFamily::Name(FAMILY_MEDIUM.into())),
    );
    style.text_styles.insert(
        TextStyle::Heading,
        FontId::new(22.0, FontFamily::Name(FAMILY_SEMIBOLD.into())),
    );
    style.text_styles.insert(
        TextStyle::Monospace,
        FontId::new(13.0, FontFamily::Monospace),
    );
    style.text_styles.insert(
        TextStyle::Name("Section".into()),
        FontId::new(15.0, FontFamily::Name(FAMILY_SEMIBOLD.into())),
    );
    style.text_styles.insert(
        TextStyle::Name("Caption".into()),
        FontId::new(12.0, FontFamily::Proportional),
    );

    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(16);

    ctx.set_global_style(style);
}

fn install_visuals(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    v.dark_mode = true;
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = BG;
    v.faint_bg_color = SURFACE;
    v.text_edit_bg_color = Some(SURFACE);
    v.override_text_color = Some(TEXT_PRIMARY);
    v.weak_text_color = Some(TEXT_SECONDARY);
    v.hyperlink_color = ACCENT_BLUE_BRIGHT;
    v.selection.bg_fill = ACCENT_BLUE;
    v.selection.stroke = Stroke::new(1.0, ACCENT_BLUE_BRIGHT);
    v.error_fg_color = ERROR;
    v.warn_fg_color = Color32::from_rgb(0xff, 0xae, 0x00);
    v.code_bg_color = SURFACE;
    v.window_stroke = Stroke::new(1.0, BORDER);

    // All five widget states share Geist's small corner radius.
    let radius = CornerRadius::same(RADIUS_SM);
    v.widgets.noninteractive.corner_radius = radius;
    v.widgets.inactive.corner_radius = radius;
    v.widgets.hovered.corner_radius = radius;
    v.widgets.active.corner_radius = radius;
    v.widgets.open.corner_radius = radius;

    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_SECONDARY);

    v.widgets.inactive.weak_bg_fill = SURFACE;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);

    v.widgets.hovered.weak_bg_fill = Color32::from_white_alpha(0x12);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);

    v.widgets.active.weak_bg_fill = Color32::from_white_alpha(0x1f);
    v.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT_BLUE);
    v.widgets.active.fg_stroke = Stroke::new(1.0, ACCENT_BLUE_BRIGHT);

    // "open" mirrors hovered per spec.
    v.widgets.open.weak_bg_fill = Color32::from_white_alpha(0x12);
    v.widgets.open.bg_stroke = Stroke::new(1.0, BORDER_STRONG);
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT_PRIMARY);

    ctx.set_visuals(v);
}

// --- Button builders --------------------------------------------------------

/// Primary call-to-action button: bright surface, dark text.
pub fn primary_button(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    let text: egui::WidgetText = label.into();
    egui::Button::new(text.color(Color32::BLACK))
        .fill(TEXT_PRIMARY)
        .corner_radius(CornerRadius::same(RADIUS_SM))
        .min_size(egui::vec2(0.0, 36.0))
}

/// Secondary button: surface fill with a subtle border.
pub fn secondary_button(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    let text: egui::WidgetText = label.into();
    egui::Button::new(text.color(TEXT_PRIMARY))
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(RADIUS_SM))
        .min_size(egui::vec2(0.0, 36.0))
}
