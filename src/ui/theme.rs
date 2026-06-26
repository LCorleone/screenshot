//! Theme system for screenshot-dai's egui UI.
//!
//! Installs Geist fonts, text styles, spacing and a **Solarized Light** color
//! palette onto an [`egui::Context`]. Also provides styled primary/secondary
//! button builders so call sites stay declarative.
//!
//! Colors are sourced from a switchable [`Palette`] struct: [`Palette::light`]
//! holds the Solarized Light values (the active theme) and [`Palette::dark`]
//! keeps the previous Geist Dark values for a future Dark/System toggle.
//! Switching the whole app to dark is a one-line change in [`install`] (call
//! `Palette::dark()` instead of `Palette::light()`). The existing `pub const`
//! color tokens remain as aliases for the light values so call sites
//! (`theme::BG`, `theme::ACCENT_BLUE`, …) are unaffected.
//!
//! Only colors change between themes — Geist fonts, typography, spacing and
//! corner radius are theme-independent and stay constant.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle,
};
use std::sync::Arc;

// --- Solarized Light tokens (active theme) ----------------------------------
//
// Canonical Solarized values:
//   base03 #002b36  base02 #073642  base01 #586e75  base00 #657b83
//   base0  #839496  base1   #93a1a1 base2   #eee8d5  base3   #fdf6e3
//   yellow #b58900  orange #cb4b16  red     #dc322f  magenta #d33682
//   violet #6c71c4  blue   #268bd2  cyan    #2aa198  green   #859900

/// Panel / window fill = Solarized **base3** `#fdf6e3` (warm paper).
pub const BG: Color32 = Color32::from_rgb(0xfd, 0xf6, 0xe3);
/// Card / input / secondary surface = Solarized **base2** `#eee8d5`.
pub const SURFACE: Color32 = Color32::from_rgb(0xee, 0xe8, 0xd5);
/// Primary body text = Solarized **base00** `#657b83`.
pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0x65, 0x7b, 0x83);
/// Secondary text = Solarized **base0** `#839496`.
pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0x83, 0x94, 0x96);
/// Disabled text = Solarized **base1** `#93a1a1`.
#[allow(dead_code)]
pub const TEXT_DISABLED: Color32 = Color32::from_rgb(0x93, 0xa1, 0xa1);
/// Default border — a subtle translucent black that reads on the light bg.
pub const BORDER: Color32 = Color32::from_rgba_unmultiplied_const(0, 0, 0, 0x1f);
/// Emphasized border — a stronger translucent black.
pub const BORDER_STRONG: Color32 = Color32::from_rgba_unmultiplied_const(0, 0, 0, 0x55);
/// Selection / active / link accent = Solarized **blue** `#268bd2`.
/// (Name kept for call-site compatibility; both accents are the Solarized blue
/// on the light theme.)
pub const ACCENT_BLUE: Color32 = Color32::from_rgb(0x26, 0x8b, 0xd2);
/// Bright accent (active fg strokes) = Solarized **blue** `#268bd2`.
pub const ACCENT_BLUE_BRIGHT: Color32 = Color32::from_rgb(0x26, 0x8b, 0xd2);
/// Errors = Solarized **red** `#dc322f`.
pub const ERROR: Color32 = Color32::from_rgb(0xdc, 0x32, 0x2f);
/// Success = Solarized **green** `#859900`.
pub const SUCCESS: Color32 = Color32::from_rgb(0x85, 0x99, 0x00);
/// Warnings = Solarized **yellow** `#b58900`.
pub const WARN: Color32 = Color32::from_rgb(0xb5, 0x89, 0x00);

/// Geist `sm` — controls/buttons/inputs. `CornerRadius` uses `u8` in egui 0.34.
pub const RADIUS_SM: u8 = 6;
/// Geist `lg` — fullscreen surfaces (preview frame).
#[allow(dead_code)]
pub const RADIUS_LG: u8 = 16;

/// Name of the named `FontFamily` resolving to Geist Medium (button text).
pub const FAMILY_MEDIUM: &str = "GeistSansMedium";
/// Name of the named `FontFamily` resolving to Geist SemiBold (headings).
pub const FAMILY_SEMIBOLD: &str = "GeistSansSemiBold";

// --- Palette struct (future Dark/System toggle scaffolding) -----------------

/// A complete color palette. All visuals + button builders are derived from a
/// `&Palette`, so swapping themes is a single call in [`install`].
#[derive(Clone, Copy)]
pub struct Palette {
    /// Panel / window fill.
    pub bg: Color32,
    /// Card / input / secondary surface.
    pub surface: Color32,
    /// Primary body text.
    pub text_primary: Color32,
    /// Secondary text.
    pub text_secondary: Color32,
    /// Disabled text.
    #[allow(dead_code)]
    pub text_disabled: Color32,
    /// Default border.
    pub border: Color32,
    /// Emphasized border.
    pub border_strong: Color32,
    /// Selection / active / link accent.
    pub accent_blue: Color32,
    /// Bright accent (active fg strokes).
    pub accent_blue_bright: Color32,
    /// Error text.
    pub error: Color32,
    /// Success text.
    #[allow(dead_code)]
    pub success: Color32,
    /// Warning text.
    pub warn: Color32,
    /// `true` for a dark theme (drives `Visuals::dark_mode`).
    pub dark_mode: bool,
}

impl Palette {
    /// Solarized Light — the active theme. Values mirror the `pub const`
    /// tokens above.
    pub fn light() -> Self {
        Self {
            bg: BG,
            surface: SURFACE,
            text_primary: TEXT_PRIMARY,
            text_secondary: TEXT_SECONDARY,
            text_disabled: TEXT_DISABLED,
            border: BORDER,
            border_strong: BORDER_STRONG,
            accent_blue: ACCENT_BLUE,
            accent_blue_bright: ACCENT_BLUE_BRIGHT,
            error: ERROR,
            success: SUCCESS,
            warn: WARN,
            dark_mode: false,
        }
    }

    /// Geist Dark — the previous theme, retained for a future Dark/System
    /// toggle. Not wired into `install` yet.
    #[allow(dead_code)]
    pub fn dark() -> Self {
        Self {
            bg: Color32::from_rgb(0x0e, 0x0e, 0x10),
            surface: Color32::from_rgb(0x16, 0x16, 0x19),
            text_primary: Color32::from_rgb(0xed, 0xed, 0xed),
            text_secondary: Color32::from_rgb(0xa0, 0xa0, 0xa0),
            text_disabled: Color32::from_rgb(0x8f, 0x8f, 0x8f),
            border: Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x24),
            border_strong: Color32::from_rgba_unmultiplied_const(255, 255, 255, 0x82),
            accent_blue: Color32::from_rgb(0x00, 0x6e, 0xfe),
            accent_blue_bright: Color32::from_rgb(0x47, 0xa8, 0xff),
            error: Color32::from_rgb(0xf3, 0x2e, 0x40),
            success: Color32::from_rgb(0x4c, 0xe1, 0x5e),
            warn: Color32::from_rgb(0xff, 0xae, 0x00),
            dark_mode: true,
        }
    }
}

/// `FontId` for a small section header (Geist SemiBold 15px).
/// Use this directly instead of a named `TextStyle`, which can panic if the
/// style isn't registered on the active `Ui`.
#[allow(dead_code)] // public design-token helper; reused by future UI surfaces
pub fn section_font() -> egui::FontId {
    egui::FontId::new(15.0, egui::FontFamily::Name(FAMILY_SEMIBOLD.into()))
}

/// `FontId` for a caption (Geist Regular 12px).
#[allow(dead_code)] // public design-token helper; reused by future UI surfaces
pub fn caption_font() -> egui::FontId {
    egui::FontId::new(12.0, egui::FontFamily::Proportional)
}

/// Install Geist fonts → text styles/spacing → Solarized Light visuals onto
/// `ctx`. To switch to dark later, change `Palette::light()` to `Palette::dark()`
/// below — nothing else needs to move.
pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    install_style(ctx);
    install_visuals(ctx, &Palette::light());
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

/// Apply the visuals for `palette`. Light theme: faint darkening on hover,
/// Solarized blue selection/accents, dark text on a warm-paper fill.
fn install_visuals(ctx: &egui::Context, palette: &Palette) {
    // Make the active egui theme match the palette so our visuals land in (and
    // render from) the correct theme slot. Without this, egui defaults to the
    // Dark theme and our light visuals can be overridden by the dark defaults.
    ctx.set_theme(if palette.dark_mode {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });

    let mut v = if palette.dark_mode {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    v.dark_mode = palette.dark_mode;
    v.panel_fill = palette.bg;
    v.window_fill = palette.bg;
    v.extreme_bg_color = palette.surface;
    v.faint_bg_color = palette.surface;
    v.text_edit_bg_color = Some(palette.surface);
    v.override_text_color = Some(palette.text_primary);
    v.weak_text_color = Some(palette.text_secondary);
    v.hyperlink_color = palette.accent_blue;
    v.selection.bg_fill = palette.accent_blue;
    v.selection.stroke = Stroke::new(1.0, palette.accent_blue);
    v.error_fg_color = palette.error;
    v.warn_fg_color = palette.warn;
    v.code_bg_color = palette.surface;
    v.window_stroke = Stroke::new(1.0, palette.border);

    // All five widget states share Geist's small corner radius.
    let radius = CornerRadius::same(RADIUS_SM);
    v.widgets.noninteractive.corner_radius = radius;
    v.widgets.inactive.corner_radius = radius;
    v.widgets.hovered.corner_radius = radius;
    v.widgets.active.corner_radius = radius;
    v.widgets.open.corner_radius = radius;

    v.widgets.noninteractive.bg_fill = palette.surface;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.border);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, palette.text_secondary);

    v.widgets.inactive.weak_bg_fill = palette.surface;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, palette.border);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, palette.text_primary);

    // Light theme: hover/active darken the surface subtly (black alpha) instead
    // of the white-alpha brightening used on dark.
    v.widgets.hovered.weak_bg_fill = Color32::from_black_alpha(0x08);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, palette.border_strong);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, palette.text_primary);

    v.widgets.active.weak_bg_fill = Color32::from_black_alpha(0x12);
    v.widgets.active.bg_stroke = Stroke::new(1.0, palette.accent_blue);
    v.widgets.active.fg_stroke = Stroke::new(1.0, palette.accent_blue_bright);

    // "open" mirrors hovered per spec.
    v.widgets.open.weak_bg_fill = Color32::from_black_alpha(0x08);
    v.widgets.open.bg_stroke = Stroke::new(1.0, palette.border_strong);
    v.widgets.open.fg_stroke = Stroke::new(1.0, palette.text_primary);

    ctx.set_visuals(v);
}

// --- Button builders --------------------------------------------------------

/// Primary call-to-action button: Solarized blue fill with white text — reads
/// as the primary CTA on the light background.
pub fn primary_button(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    let text: egui::WidgetText = label.into();
    egui::Button::new(text.color(Color32::WHITE))
        .fill(ACCENT_BLUE)
        .corner_radius(CornerRadius::same(RADIUS_SM))
        .min_size(egui::vec2(0.0, 36.0))
}

/// Secondary button: base2 fill with a subtle border and base00 text.
pub fn secondary_button(label: impl Into<egui::WidgetText>) -> egui::Button<'static> {
    let text: egui::WidgetText = label.into();
    egui::Button::new(text.color(TEXT_PRIMARY))
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(RADIUS_SM))
        .min_size(egui::vec2(0.0, 36.0))
}
