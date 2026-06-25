//! screenshot-dai entry point.

// On Windows release builds, use the GUI subsystem so no console window
// pops up when the app is launched. Kept in debug builds for log visibility.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod capture;
mod clipboard;
mod config;
mod llm;
mod ocr;
mod platform;
mod ui;

use std::fs::OpenOptions;

use eframe::egui;

fn main() -> anyhow::Result<()> {
    // --- Logging: stderr + a rotating-ish log file under the OS data dir.
    //     This exists so "nothing happens" on Windows can always be diagnosed.
    init_logging()?;
    tracing::info!(
        "screenshot-dai starting (version {})",
        env!("CARGO_PKG_VERSION")
    );

    // Install a panic hook so any panic is written to the log/stderr too,
    // instead of vanishing silently under the windows subsystem.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        default_hook(info);
    }));

    let settings = config::Settings::load().unwrap_or_else(|e| {
        tracing::warn!("failed to load settings ({e:#}); using defaults");
        config::Settings::default()
    });
    tracing::info!("settings loaded");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenshot-dai")
            .with_inner_size([800.0, 600.0]),
        ..Default::default()
    };
    tracing::info!("launching eframe");

    let result = eframe::run_native(
        "screenshot-dai",
        native_options,
        Box::new(move |cc| {
            ui::theme::install(&cc.egui_ctx);
            Ok(Box::new(app::ScreenshotDaiApp::new(settings)))
        }),
    );

    match result {
        Ok(()) => {
            tracing::info!("screenshot-dai exited cleanly");
            Ok(())
        }
        Err(e) => {
            tracing::error!("eframe exited with error: {e}");
            Err(anyhow::anyhow!("eframe exited with error: {e}"))
        }
    }
}

/// Configure `tracing_subscriber` to write to stderr AND a log file located
/// in the OS per-user data dir (`screenshot-dai/app.log`). Each launch
/// overwrites the file (we keep only the latest run). Failing to open the
/// log file is non-fatal — stderr still works.
fn init_logging() -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let file_layer = log_file().and_then(|path| {
        // Make sure the directory exists.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            Ok(file) => {
                tracing::info!("logging to {}", path.display());
                Some(file)
            }
            Err(e) => {
                eprintln!("warning: could not open log file {}: {e}", path.display());
                None
            }
        }
    });

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    if let Some(file) = file_layer {
        let file_layer = tracing_subscriber::fmt::layer().with_writer(file);
        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(stderr_layer)
            .init();
    }
    Ok(())
}

/// Resolve the log file path under the OS data dir.
fn log_file() -> Option<std::path::PathBuf> {
    // Windows: %APPDATA%\screenshot-dai\app.log
    // macOS:   ~/Library/Application Support/screenshot-dai/app.log
    // Linux:   ~/.local/share/screenshot-dai/app.log
    let proj = directories::ProjectDirs::from("ai", "dai", "screenshot-dai")?;
    Some(proj.data_dir().join("app.log"))
}
