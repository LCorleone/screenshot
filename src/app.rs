//! eframe app: system-tray-driven capture tool with a hidden-by-default
//! Settings window.
//!
//! Phase E1 behaviour:
//! - The Settings window starts hidden; it is shown only via the tray menu.
//!   (E0 used to auto-pop it after a capture — that stopgap is gone.)
//! - A global hotkey (`Ctrl+Shift+S`) and the tray "Capture Region" item start
//!   the region-overlay engine, which now flows into the in-place editor.
//! - The editor yields an [`region_overlay::EditorOutcome`] the app harvests:
//!   `Pin` floats the edited image as a pinned window, `Save` opens an rfd
//!   save dialog and writes a PNG, `Copy` copies to the clipboard, `Cancel`
//!   does nothing.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use eframe::egui;
use global_hotkey::GlobalHotKeyEvent;
use global_hotkey::GlobalHotKeyManager;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use image::RgbaImage;
use tray_icon::TrayIcon;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};

use crate::config::Settings;
use crate::ui::pin_window;
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
    /// Active region-selection overlay session, if any.
    region_session: Option<Arc<Mutex<region_overlay::RegionSession>>>,
    /// Physical px / logical point of the active capture (the capturing
    /// monitor's scale factor). Used to size pinned windows.
    last_scale: f32,
    /// Whether the Settings window should currently be visible.
    window_visible: bool,
    /// True once eframe has confirmed the window is actually hidden after the
    /// initial launch (the root viewport can briefly paint on Windows before
    /// `Visible(false)` is processed, showing an empty black frame).
    hidden_confirmed: bool,
    /// Set (hotkey / tray) when a region capture has been requested but not
    /// yet started. Captures must start from within `ui()` where we have the
    /// egui ctx, so the global channel handlers just flip this flag.
    capture_requested: bool,
    /// True when the user asked to quit via the tray. Distinguishes a Quit
    /// (actually exit) from a Settings-window X close (hide only).
    quitting: bool,
    /// Floating pinned-screenshot windows created via the editor's Pin action.
    pins: Vec<Arc<Mutex<pin_window::PinSession>>>,
    /// Monotonic id source for the next pin window.
    next_pin_id: u64,

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

        // --- Tray icon: a 32x32 glyph drawn in code — accent-blue rounded
        //     square with white screenshot-selection corner marks. ---
        let tray = (|| {
            let icon = tray_icon::Icon::from_rgba(tray_icon_rgba(), 32, 32).ok()?;
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

        // --- Global hotkey manager + hotkey parsed from Settings at startup. ---
        // Build the combo string (e.g. "Ctrl+Shift+S") from the configured
        // modifier + key fields; parse via HotKey::from_str. On any parse or
        // registration failure, fall back to the default Ctrl+Shift+S and log.
        // Applied only at startup; changing the fields takes effect next launch.
        let hotkey_manager = GlobalHotKeyManager::new()
            .map_err(|e| {
                tracing::warn!("failed to create global hotkey manager: {e}");
                e
            })
            .ok();
        if let Some(gm) = &hotkey_manager {
            let mods = settings.hotkey_modifiers.trim();
            let key = settings.hotkey_key.trim();
            // A modifier-less global hotkey (e.g. "S") is valid and parses fine,
            // but it will swallow that key system-wide while the app runs, so a
            // user who clears the modifiers field does so deliberately.
            let combo = if mods.is_empty() {
                key.to_string()
            } else {
                format!("{mods}+{key}")
            };
            let fallback = || HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyS);
            let hk = match HotKey::from_str(&combo) {
                Ok(h) => h,
                Err(e) => {
                    if key.is_empty() {
                        tracing::info!("no hotkey key configured; using Ctrl+Shift+S");
                    } else {
                        tracing::warn!(
                            "failed to parse configured hotkey {combo:?}: {e}; using Ctrl+Shift+S"
                        );
                    }
                    fallback()
                }
            };
            if let Err(e) = gm.register(hk) {
                tracing::warn!(
                    "failed to register hotkey {combo:?}: {e}; retrying with Ctrl+Shift+S"
                );
                if let Err(e) = gm.register(fallback()) {
                    tracing::warn!("fallback hotkey registration also failed: {e}");
                }
            }
        }

        Self {
            settings,
            settings_buf,
            message: String::new(),
            message_kind: MsgKind::Info,
            region_session: None,
            last_scale: 1.0,
            window_visible: false,
            hidden_confirmed: false,
            capture_requested: false,
            quitting: false,
            pins: Vec::new(),
            next_pin_id: 0,
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
        self.hidden_confirmed = false;
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
                self.quitting = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // --- 3. Start a region capture if one was requested (needs the ctx). ---
        // Ignore the request if an overlay is already open (re-entrant hotkey /
        // double Capture-menu click) so we don't discard the in-progress session.
        if self.capture_requested {
            self.capture_requested = false;
            if self.region_session.is_some() {
                tracing::info!("capture requested but a session is already open; ignoring");
            } else {
                match region_overlay::start_session(&ctx) {
                    Ok(s) => {
                        self.region_session = Some(Arc::new(Mutex::new(s)));
                        self.message.clear();
                    }
                    Err(e) => {
                        self.set_message(MsgKind::Error, format!("Region capture failed: {e:#}"))
                    }
                }
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

        // --- 5. Harvest the editor outcome. ---
        if let Some(session) = self.region_session.take() {
            let finished = session.lock().expect("poisoned").finished;
            if finished {
                let (result, session_scale) = {
                    let mut g = session.lock().expect("poisoned");
                    (g.result.take(), g.scale)
                };
                self.last_scale = session_scale;
                match result {
                    Some(region_overlay::EditorOutcome::Pin(img)) => {
                        let id = self.next_pin_id;
                        self.next_pin_id += 1;
                        let pin = Arc::new(Mutex::new(pin_window::PinSession::new(
                            &ctx,
                            id,
                            &img,
                            session_scale,
                        )));
                        self.pins.push(pin);
                        self.set_message(MsgKind::Info, "Pinned.");
                    }
                    Some(region_overlay::EditorOutcome::Save(img)) => match save_via_dialog(&img) {
                        Ok(Some(p)) => {
                            self.set_message(MsgKind::Success, format!("Saved to {}", p.display()))
                        }
                        Ok(None) => self.set_message(MsgKind::Info, "Save cancelled"),
                        Err(e) => self.set_message(MsgKind::Error, format!("Save failed: {e:#}")),
                    },
                    Some(region_overlay::EditorOutcome::Copy(img)) => {
                        match copy_image_to_clipboard(&img) {
                            Ok(()) => self.set_message(MsgKind::Success, "Copied to clipboard"),
                            Err(e) => {
                                self.set_message(MsgKind::Error, format!("Copy failed: {e:#}"))
                            }
                        }
                    }
                    Some(region_overlay::EditorOutcome::Cancel) | None => {
                        self.set_message(MsgKind::Info, "Capture cancelled.");
                    }
                }
            } else {
                self.region_session = Some(session);
            }
        }

        // --- 6. Show pinned screenshots, then reap closed ones. ---
        self.pins.retain(|p| !p.lock().expect("poisoned").closed);
        for pin in &self.pins {
            pin_window::show_pin(&ctx, pin);
        }

        // --- 7. Window visibility + close handling. ---
        // On OS close-request of the Settings window, HIDE (don't quit) —
        // quitting is only via the tray Quit item.
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if close_requested {
            if self.quitting {
                // Tray Quit: let the close proceed so eframe exits.
            } else {
                // Settings-window X: hide instead of quitting.
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide_window(&ctx);
            }
        }

        // When hidden, draw nothing else.
        if !self.window_visible {
            // eframe quirk: the root viewport can briefly paint a black frame
            // on launch before `Visible(false)` is processed. Keep pushing
            // `Visible(false)` every frame until the window reports it's
            // actually hidden, then stop (avoids spamming the command).
            if !self.hidden_confirmed {
                let actually_hidden = ctx.input(|i| i.viewport().visible() == Some(false));
                if actually_hidden {
                    self.hidden_confirmed = true;
                } else {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
            }
            return;
        }

        // --- 8. Draw the Settings window contents. ---
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("screenshot-dai Settings");
            ui.add_space(8.0);

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
fn copy_image_to_clipboard(img: &RgbaImage) -> anyhow::Result<()> {
    let mut cb = arboard::Clipboard::new()?;
    let data = arboard::ImageData {
        width: img.width() as usize,
        height: img.height() as usize,
        bytes: std::borrow::Cow::from(img.as_raw().clone()),
    };
    cb.set_image(data.to_owned_img())?;
    Ok(())
}

/// Open a native "Save as PNG" dialog (blocking) and write `img` to the chosen
/// path. Returns `Ok(Some(path))` on a successful write, `Ok(None)` if the user
/// cancelled the dialog, or `Err` on write failure. The blocking modal is fine
/// for E1: it's the user's explicit action and they're waiting on it anyway.
fn save_via_dialog(img: &RgbaImage) -> anyhow::Result<Option<PathBuf>> {
    let dir = directories::UserDirs::new()
        .and_then(|ud| ud.picture_dir().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let file_name = format!("screenshot-{millis}.png");
    let path = rfd::FileDialog::new()
        .add_filter("PNG image", &["png"])
        .set_directory(&dir)
        .set_file_name(&file_name)
        .save_file();
    match path {
        Some(p) => {
            img.save(&p)
                .with_context(|| format!("write {}", p.display()))?;
            Ok(Some(p))
        }
        None => Ok(None),
    }
}

/// Build a 32x32 RGBA tray icon in code: an accent-blue rounded square with
/// white screenshot-selection corner marks (a crop-frame motif). No asset
/// files required.
fn tray_icon_rgba() -> Vec<u8> {
    const S: usize = 32;
    // Accent blue — Solarized blue (matches crate::ui::theme::ACCENT_BLUE).
    let (br, bg, bb) = (0x26u8, 0x8bu8, 0xd2u8);
    let (wr, wg, wb) = (0xffu8, 0xffu8, 0xffu8);
    let mut rgba = vec![0u8; S * S * 4];

    // Helper: set a pixel (x,y) to an opaque color.
    let mut set = |x: i32, y: i32, (r, g, b): (u8, u8, u8)| {
        if (0..S as i32).contains(&x) && (0..S as i32).contains(&y) {
            let i = ((y as usize) * S + x as usize) * 4;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = 0xff;
        }
    };

    // 1. Filled rounded square (inset margin = 4, corner radius = 6).
    let m = 4i32;
    let r = 6i32;
    for y in 0..S as i32 {
        for x in 0..S as i32 {
            // distance-from-corner for the rounding test
            let inside = x >= m && x < S as i32 - m && y >= m && y < S as i32 - m && {
                // round the 4 corners
                let dx = (x - m).min((S as i32 - 1 - m) - x).max(0);
                let dy = (y - m).min((S as i32 - 1 - m) - y).max(0);
                !(dx < r && dy < r && (r - dx).pow(2) + (r - dy).pow(2) < r * r && (dx.min(dy) < r))
            };
            if inside {
                set(x, y, (br, bg, bb));
            }
        }
    }

    // 2. White crop-frame corner marks (L-shapes) at the four inner corners,
    //    suggesting a screenshot selection. Inner region is [8..24].
    let (x0, x1, y0, y1) = (8i32, 23i32, 8i32, 23i32);
    let arm = 6i32; // length of each corner arm
    let th = 2i32; // stroke thickness
    // top-left corner
    for i in 0..arm {
        for t in 0..th {
            set(x0 + i, y0 + t, (wr, wg, wb));
            set(x0 + t, y0 + i, (wr, wg, wb));
        }
    }
    // top-right corner
    for i in 0..arm {
        for t in 0..th {
            set(x1 - i, y0 + t, (wr, wg, wb));
            set(x1 - t, y0 + i, (wr, wg, wb));
        }
    }
    // bottom-left corner
    for i in 0..arm {
        for t in 0..th {
            set(x0 + i, y1 - t, (wr, wg, wb));
            set(x0 + t, y1 - i, (wr, wg, wb));
        }
    }
    // bottom-right corner
    for i in 0..arm {
        for t in 0..th {
            set(x1 - i, y1 - t, (wr, wg, wb));
            set(x1 - t, y1 - i, (wr, wg, wb));
        }
    }

    rgba
}
