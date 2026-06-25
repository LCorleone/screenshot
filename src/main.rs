//! screenshot-dai entry point.

mod app;
mod clipboard;
mod config;
mod capture;
mod llm;
mod ocr;
mod ui;

use eframe::egui;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let settings = config::Settings::load().unwrap_or_else(|e| {
        tracing::warn!("failed to load settings ({e:#}); using defaults");
        config::Settings::default()
    });

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenshot-dai")
            .with_inner_size([800.0, 600.0]),
        ..Default::default()
    };

    eframe::run_native(
        "screenshot-dai",
        native_options,
        Box::new(move |_cc| Ok(Box::new(app::ScreenshotDaiApp::new(settings)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe exited with error: {e}"))?;

    Ok(())
}
