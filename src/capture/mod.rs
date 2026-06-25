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
#[allow(dead_code)]
pub fn capture_primary_monitor() -> anyhow::Result<image::RgbaImage> {
    let monitor = primary_monitor()?;
    capture_monitor(&monitor)
}

/// All monitors, as xcap reports them.
pub fn all_monitors() -> anyhow::Result<Vec<xcap::Monitor>> {
    Monitor::all().context("failed to enumerate monitors")
}

/// The monitor whose physical screen rect contains `pt` (virtual-screen px);
/// falls back to the primary monitor (then the first).
pub fn monitor_at_physical(pt: (i32, i32)) -> anyhow::Result<xcap::Monitor> {
    let monitors = all_monitors()?;
    for m in &monitors {
        let (x, y) = (m.x().unwrap_or(0), m.y().unwrap_or(0));
        let (w, h) = (
            m.width().unwrap_or(0) as i32,
            m.height().unwrap_or(0) as i32,
        );
        if pt.0 >= x && pt.0 < x + w && pt.1 >= y && pt.1 < y + h {
            return Ok(m.clone());
        }
    }
    primary_monitor()
}

/// Capture a specific monitor.
pub fn capture_monitor(monitor: &xcap::Monitor) -> anyhow::Result<image::RgbaImage> {
    monitor
        .capture_image()
        .with_context(|| format!("failed to capture monitor {:?}", monitor.name()))
}

/// Capture the monitor under the OS cursor; falls back to primary when the
/// cursor position can't be read (non-Windows). Returns the image and the
/// captured monitor's scale factor.
pub fn capture_monitor_under_cursor() -> anyhow::Result<(image::RgbaImage, f32)> {
    let monitor = match crate::platform::cursor_screen_physical() {
        Some(pt) => monitor_at_physical(pt)?,
        None => primary_monitor()?,
    };
    let scale = monitor.scale_factor().unwrap_or(1.0).max(0.0001);
    Ok((capture_monitor(&monitor)?, scale))
}

/// Virtual-desktop bounding rect in physical px: `(vmin_x, vmin_y, vwidth, vheight)`.
pub fn virtual_desktop_rect(monitors: &[xcap::Monitor]) -> (i32, i32, u32, u32) {
    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for m in monitors {
        let x = m.x().unwrap_or(0);
        let y = m.y().unwrap_or(0);
        let w = m.width().unwrap_or(0) as i32;
        let h = m.height().unwrap_or(0) as i32;
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
    }
    if min_x > max_x || min_y > max_y {
        return (0, 0, 0, 0); // no monitors
    }
    (min_x, min_y, (max_x - min_x) as u32, (max_y - min_y) as u32)
}

/// Paste `src` into `dst` at offset `(dx, dy)` (physical px), clipping to dst bounds.
fn paste(dst: &mut image::RgbaImage, src: &image::RgbaImage, dx: i32, dy: i32) {
    let (dw, dh) = (dst.width() as i32, dst.height() as i32);
    let (sw, sh) = (src.width(), src.height());
    for sy in 0..sh {
        for sx in 0..sw {
            let x = dx + sx as i32;
            let y = dy + sy as i32;
            if x >= 0 && y >= 0 && x < dw && y < dh {
                let px = *src.get_pixel(sx, sy);
                dst.put_pixel(x as u32, y as u32, px);
            }
        }
    }
}

/// Pure composite: build a virtual-desktop image (origin = `vmin`) from already-
/// captured monitor images placed at their physical offsets. Returned image
/// size = `vsize`. Gaps between monitors are transparent (0,0,0,0).
pub fn composite_virtual_desktop(
    captures: &[(xcap::Monitor, image::RgbaImage)],
    vmin: (i32, i32),
    vsize: (u32, u32),
) -> image::RgbaImage {
    let mut out = image::RgbaImage::new(vsize.0, vsize.1);
    for (m, img) in captures {
        let dx = m.x().unwrap_or(0) - vmin.0;
        let dy = m.y().unwrap_or(0) - vmin.1;
        paste(&mut out, img, dx, dy);
    }
    out
}

/// Capture the full virtual desktop (all monitors) as one composite image.
/// Returns `(composite, vmin, vsize)`.
pub fn capture_virtual_desktop() -> anyhow::Result<(image::RgbaImage, (i32, i32), (u32, u32))> {
    let monitors = all_monitors()?;
    if monitors.is_empty() {
        anyhow::bail!("no monitors found");
    }
    let vmin_vsize = virtual_desktop_rect(&monitors);
    let (vmin, vsize) = ((vmin_vsize.0, vmin_vsize.1), (vmin_vsize.2, vmin_vsize.3));
    let mut captures: Vec<(xcap::Monitor, image::RgbaImage)> = Vec::with_capacity(monitors.len());
    for m in &monitors {
        match capture_monitor(m) {
            Ok(img) => captures.push((m.clone(), img)),
            Err(e) => tracing::warn!("skipping monitor {:?}: {e:#}", m.name()),
        }
    }
    if captures.is_empty() {
        anyhow::bail!("failed to capture any monitor");
    }
    Ok((
        composite_virtual_desktop(&captures, vmin, vsize),
        vmin,
        vsize,
    ))
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
        .map(|d| d.as_millis())
        .unwrap_or(0);
    dir.join(format!("screenshot-{ts}.png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_virtual_desktop_composite_places_and_clips() {
        // Two "monitors": A at (0,0) 2x2 all-red, B at (2,0) 2x2 all-green.
        // vmin=(0,0), vsize=(4,2). Composite: left half red, right half green.
        let (w, h) = (4u32, 2u32);
        let mut expected = image::RgbaImage::new(w, h);
        for x in 0..2 {
            for y in 0..2 {
                expected.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
            }
        }
        for x in 2..4 {
            for y in 0..2 {
                expected.put_pixel(x, y, image::Rgba([0, 255, 0, 255]));
            }
        }

        // Build directly with paste to avoid needing a real Monitor.
        let mut out = image::RgbaImage::new(w, h);
        let red = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let green = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 255, 0, 255]));
        paste(&mut out, &red, 0, 0);
        paste(&mut out, &green, 2, 0);
        assert_eq!(out, expected);

        // Clipping: pasting off the right edge doesn't panic and is clipped.
        paste(&mut out, &red, 3, 0); // only x=3 column lands
        // column x=3 becomes red (was green)
        assert_eq!(out.get_pixel(3, 0), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(out.get_pixel(2, 0), &image::Rgba([0, 255, 0, 255]));
    }
}
