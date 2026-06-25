//! eframe app: system-tray-driven capture tool with a hidden-by-default
//! Settings window.
//!
//! Phase E0 behaviour:
//! - The Settings window starts hidden; it is shown via the tray menu or after
//!   a capture.
//! - A global hotkey (`Ctrl+Shift+S`) and the tray "Capture Region" item start
//!   the existing region-overlay engine.
//! - After a capture we do NOT auto-save. We hold the image in memory and show
//!   a small preview + a "Copy" button (copies to the system clipboard via
//!   `arboard`).

use std::sync::{Arc, Mutex};

use eframe::egui;
use global_hotkey::GlobalHotKeyEvent;
use global_hotkey::GlobalHotKeyManager;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use image::RgbaImage;
use tray_icon::TrayIcon;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};

use crate::config::Settings;
use crate::ui::region_overlay;

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
    /// Status / message line shown to the user.
    message: String,
    /// Visual classification of `message`.
    message_kind: MsgKind,
    /// Texture handle for the most recent capture preview (in-window thumbnail).
    texture: Option<egui::TextureHandle>,
    /// Active region-selection overlay session, if any.
    region_session: Option<Arc<Mutex<region_overlay::RegionSession>>>,
    /// Most recent capture (kept in memory; never auto-saved in E0).
    last_image: Option<RgbaImage>,
    /// Physical px / logical point for `last_image` (the capturing monitor's
    /// scale factor). Retained for later phases that reintroduce the pin.
    last_scale: f32,
    /// Whether the Settings window should currently be visible.
    window_visible: bool,
    /// The most recent capture, shown transiently with a Copy button.
    just_captured: Option<RgbaImage>,
    /// Set (hotkey / tray) when a region capture has been requested but not
    /// yet started. Captures must start from within `ui()` where we have the
    /// egui ctx, so the global channel handlers just flip this flag.
    capture_requested: bool,

    // --- Owned system-tray / global-hotkey state. These live as long as the
    //     App, which lives as long as eframe. Dropping them would unregister
    //     the tray/hotkey, so they are stored (never taken).
    /// Tray icon; kept alive to keep the icon showing.
    #[allow(dead_code)]
    tray: Option<TrayIcon>,
    /// Tray menu item handles; kept alive AND to compare ids in the menu-event
    /// channel.
    settings_item: MenuItem,
    capture_item: MenuItem,
    quit_item: MenuItem,
    /// Global-hotkey manager; kept alive to keep the hotkey registered.
    #[allow(dead_code)]
    hotkey_manager: Option<GlobalHotKeyManager>,
}

impl ScreenshotDaiApp {
    /// Build the app. The tray icon + menu, and the global-hotkey manager are
    /// constructed here so they live for the whole app lifetime. Failures are
    /// logged but non-fatal: the app still runs (just without that feature).
    pub fn new(settings: Settings) -> Self {
        let settings_buf = settings.clone();

        // --- Tray menu ---
        let menu = Menu::new();
        let settings_item = MenuItem::new("Settings", true, None);
        let capture_item = MenuItem::new("Capture Region", true, None);
        let sep = PredefinedMenuItem::separator();
        let quit_item = MenuItem::new("Quit", true, None);
        let _ = menu.append(&settings_item);
        let _ = menu.append(&capture_item);
        let _ = menu.append(&sep);
        let _ = menu.append(&quit_item);

        // --- Tray icon: a 16x16 solid accent-blue RGBA image, built in code. ---
        let tray = (|| {
            let icon = tray_icon::Icon::from_rgba(
                std::iter::repeat([0x00u8, 0x6e, 0xfe, 0xff])
                    .flatten()
                    .take(16 * 16 * 4)
                    .collect::<Vec<u8>>(),
                16,
                16,
            )
            .ok()?;
            tray_icon::TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("screenshot-dai")
                .with_icon(icon)
                .build()
                .ok()
        })();
        if tray.is_none() {
            tracing::warn!("failed to create tray icon (continuing without it)");
        }

        // --- Global hotkey manager + default Ctrl+Shift+S registration. ---
        let hotkey_manager = GlobalHotKeyManager::new()
            .map_err(|e| {
                tracing::warn!("failed to create global hotkey manager: {e}");
                e
            })
            .ok();
        if let Some(gm) = &hotkey_manager {
            let hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyS);
            if let Err(e) = gm.register(hk) {
                tracing::warn!("failed to register global hotkey Ctrl+Shift+S: {e}");
            }
        }

        Self {
            settings,
            settings_buf,
            message: String::new(),
            message_kind: MsgKind::Info,
            texture: None,
            region_session: None,
            last_image: None,
            last_scale: 1.0,
            window_visible: false,
            just_captured: None,
            capture_requested: false,
            tray,
            settings_item,
            capture_item,
            quit_item,
            hotkey_manager,
        }
    }

    /// Set the status message and its visual classification.
    fn set_message(&mut self, kind: MsgKind, text: impl Into<String>) {
        self.message = text.into();
        self.message_kind = kind;
    }

    /// Mark the Settings window as visible and push the matching viewport
    /// command so eframe actually shows the (initially hidden) window.
    fn show_window(&mut self, ctx: &egui::Context) {
        self.window_visible = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }

    /// Hide the Settings window (without quitting the app).
    fn hide_window(&mut self, ctx: &egui::Context) {
        self.window_visible = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
}

impl eframe::App for ScreenshotDaiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // --- 1. Poll the global-hotkey channel (just sets a flag). ---
        while GlobalHotKeyEvent::receiver().try_recv().is_ok() {
            self.capture_requested = true;
        }

        // --- 2. Poll the tray-menu channel. ---
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == *self.settings_item.id() {
                self.show_window(&ctx);
            } else if ev.id == *self.capture_item.id() {
                self.capture_requested = true;
            } else if ev.id == *self.quit_item.id() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // --- 3. Start a region capture if one was requested (needs the ctx). ---
        if self.capture_requested {
            self.capture_requested = false;
            match region_overlay::start_session(&ctx) {
                Ok(s) => {
                    self.region_session = Some(Arc::new(Mutex::new(s)));
                    self.message.clear();
                }
                Err(e) => self.set_message(MsgKind::Error, format!("Region capture failed: {e:#}")),
            }
        }

        // --- 4. Show the region overlay while a session is active. ---
        if let Some(session) = self.region_session.clone() {
            let (origin_logical, size_logical) = {
                let g = session.lock().expect("poisoned");
                let ol = egui::pos2(
                    g.origin_physical.0 as f32 / g.scale,
                    g.origin_physical.1 as f32 / g.scale,
                );
                (ol, g.size_logical)
            };
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("region_overlay"),
                egui::ViewportBuilder::default()
                    .with_decorations(false)
                    .with_resizable(false)
                    .with_always_on_top()
                    .with_position([origin_logical.x, origin_logical.y])
                    .with_inner_size(size_logical)
                    .with_title("screenshot-dai region"),
                move |ui, _class| {
                    region_overlay::draw_overlay(ui, &session);
                },
            );
        }

        // --- 5. Harvest the region result (no auto-save in E0). ---
        let mut new_texture: Option<egui::TextureHandle> = None;
        let mut new_message: Option<(MsgKind, String)> = None;
        let mut new_last_scale: Option<f32> = None;
        let mut new_captured: Option<RgbaImage> = None;
        let taken = self.region_session.take();
        if let Some(session) = taken {
            let finished = session.lock().expect("poisoned").finished;
            if finished {
                let (result, session_scale) = {
                    let mut g = session.lock().expect("poisoned");
                    (g.result.take(), g.scale)
                };
                match result {
                    Some(region_overlay::RegionResult::Cropped(img)) => {
                        let ci = crate::capture::rgba_image_to_color_image(&img);
                        new_texture =
                            Some(ctx.load_texture("captured", ci, egui::TextureOptions::LINEAR));
                        new_captured = Some(img.clone());
                        self.last_image = Some(img);
                        new_last_scale = Some(session_scale);
                        new_message = Some((MsgKind::Info, "Captured. Copy or close.".to_string()));
                        self.show_window(&ctx);
                    }
                    Some(region_overlay::RegionResult::Cancelled) => {
                        new_message = Some((MsgKind::Info, "Capture cancelled.".to_string()));
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
        if let Some(img) = new_captured {
            self.just_captured = Some(img);
        }

        // --- 6. Window visibility + close handling. ---
        // On OS close-request of the Settings window, HIDE (don't quit) —
        // quitting is only via the tray Quit item.
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested {
            // Tell eframe to cancel the close, then hide instead.
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_window(&ctx);
        }

        // When hidden, draw nothing else.
        if !self.window_visible {
            return;
        }

        // --- 7. Draw the Settings window contents. ---
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("screenshot-dai Settings");
            ui.add_space(8.0);

            // --- Just-captured preview + Copy button (transient, post-capture). ---
            if self.just_captured.is_some() {
                egui::Frame::group(ui.style())
                    .fill(crate::ui::theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0, crate::ui::theme::BORDER))
                    .corner_radius(egui::CornerRadius::same(crate::ui::theme::RADIUS_LG))
                    .inner_margin(egui::Margin::same(12))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Most Recent Capture")
                                .font(crate::ui::theme::caption_font())
                                .color(crate::ui::theme::TEXT_SECONDARY),
                        );
                        if let Some(handle) = &self.texture {
                            let avail = ui.available_width();
                            let size = handle.size_vec2();
                            let scale = if size.x > 0.0 {
                                (avail / size.x).min(1.0)
                            } else {
                                1.0
                            };
                            // Cap the preview height at ~200 logical points.
                            let scaled = size * scale;
                            let height_cap = scaled.y.min(200.0);
                            let width_cap = scaled.x * (height_cap / scaled.y.max(1.0));
                            ui.image(egui::load::SizedTexture::new(
                                handle.id(),
                                egui::vec2(width_cap, height_cap),
                            ));
                        }
                        if ui.add(crate::ui::theme::secondary_button("Copy")).clicked() {
                            match copy_image_to_clipboard(self.last_image.as_ref()) {
                                Ok(()) => self.set_message(MsgKind::Success, "Copied to clipboard"),
                                Err(e) => {
                                    self.set_message(MsgKind::Error, format!("Copy failed: {e:#}"))
                                }
                            }
                        }
                    });
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(8.0);
            }

            // --- Settings form ---
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

                    ui.label("Hotkey (modifiers)");
                    ui.text_edit_singleline(&mut self.settings_buf.hotkey_modifiers);
                    ui.end_row();

                    ui.label("Hotkey (key)");
                    ui.text_edit_singleline(&mut self.settings_buf.hotkey_key);
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

            ui.add_space(8.0);
            if !self.message.is_empty() {
                let msg_color = match self.message_kind {
                    MsgKind::Error => crate::ui::theme::ERROR,
                    MsgKind::Success => crate::ui::theme::SUCCESS,
                    MsgKind::Info => crate::ui::theme::TEXT_SECONDARY,
                };
                ui.label(egui::RichText::new(&self.message).color(msg_color));
            }

            ui.add_space(16.0);
            ui.label(
                egui::RichText::new("Press Ctrl+Shift+S to capture a region.")
                    .color(crate::ui::theme::TEXT_SECONDARY),
            );
        });
    }
}

/// Copy an RGBA image to the system clipboard via `arboard`.
///
/// arboard 3.x exposes `ImageData { width, height, bytes }` (no `from_rgba`);
/// we build it directly from the image's raw RGBA bytes.
fn copy_image_to_clipboard(img: Option<&RgbaImage>) -> anyhow::Result<()> {
    let img = img.ok_or_else(|| anyhow::anyhow!("no captured image to copy"))?;
    let mut cb = arboard::Clipboard::new()?;
    let data = arboard::ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: std::borrow::Cow::from(img.as_raw().clone()),
    };
    cb.set_image(data.to_owned_img())?;
    Ok(())
}
