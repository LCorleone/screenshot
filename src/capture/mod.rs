//! Screenshot capture.
//!
//! Phase 0 ships only the simplest path: grab the primary monitor as an
//! `image::RgbaImage`, render it in the UI, and save a timestamped PNG.
//!
//! Planned for later phases (NOT implemented here):
//! - Region selection with an overlay picker.
//! - Per-window capture (window enumeration + detection).
//! - Scrolling / full-page capture.
//! - "Pin to desktop" floating note over the captured image.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use egui::ColorImage;
use xcap::Monitor;

/// Get the primary monitor (falls back to the first available).
pub fn primary_monitor() -> anyhow::Result<xcap::Monitor> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no monitors found"))
}

/// Capture the primary monitor and return it as an RGBA image.
pub fn capture_primary_monitor() -> anyhow::Result<image::RgbaImage> {
    let monitors = Monitor::all().context("failed to enumerate monitors")?;
    let monitor = monitors
        .iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .or_else(|| monitors.first())
        .ok_or_else(|| anyhow::anyhow!("no monitors found"))?;
    let img = monitor
        .capture_image()
        .with_context(|| format!("failed to capture monitor {:?}", monitor.name()))?;
    Ok(img)
}

/// Convert an `image::RgbaImage` into an `egui::ColorImage` for display.
pub fn rgba_image_to_color_image(img: &image::RgbaImage) -> ColorImage {
    let size = [img.width() as usize, img.height() as usize];
    ColorImage::from_rgba_unmultiplied(size, img.as_raw())
}

/// Write the image as a PNG to `path`.
pub fn save_png(path: &Path, img: &image::RgbaImage) -> anyhow::Result<()> {
    img.save(path)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Encode the image as PNG bytes (in-memory).
#[allow(dead_code)]
pub fn encode_png_bytes(img: &image::RgbaImage) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .context("failed to encode PNG")?;
    Ok(buf)
}

/// Build a timestamped output path for a new screenshot under the OS pictures
/// dir (falls back to the current dir when that is unavailable).
pub fn default_save_path() -> PathBuf {
    let dir = directories::UserDirs::new()
        .and_then(|ud| ud.picture_dir().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("screenshot-{ts}.png"))
}
