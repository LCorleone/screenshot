//! Win32 implementation of window-under-cursor detection.
//!
//! We use `EnumWindows` + `GetWindowRect` hit-testing rather than
//! `WindowFromPoint`, because our own fullscreen always-on-top overlay would
//! occlude every other window for `WindowFromPoint`. Enumerating top-level
//! windows and hit-testing their rects against the cursor avoids that.
//!
//! All public inputs/outputs are in **logical screen points** (the same
//! coordinate space the egui overlay uses). Internally we convert to/from
//! physical pixels using the system DPI.

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextW, IsIconic, IsWindowVisible,
};
use windows::core::BOOL;

/// Collector used across the `EnumWindows` callback.
struct Collector {
    rects: Vec<(i32, i32, i32, i32)>,
}

impl Collector {
    fn new() -> Self {
        Self { rects: Vec::new() }
    }
}

/// Returns the rect `(x, y, w, h)` in **logical screen points** of the
/// topmost/most-specific window under `screen_pt_logical`, or `None` if the
/// cursor isn't inside any candidate window. Never panics.
pub fn window_rect_at(screen_pt_logical: (f32, f32)) -> Option<(f32, f32, f32, f32)> {
    // System DPI scale (logical = physical / scale). Per-monitor DPI is assumed
    // uniform for v1; if we cannot read it, fall back to 1.0.
    let scale: f32 = match unsafe { GetDpiForSystem() } {
        dpi if dpi > 0 => dpi as f32 / 96.0,
        _ => {
            tracing::warn!("GetDpiForSystem returned 0; assuming scale 1.0");
            1.0
        }
    };
    if !scale.is_finite() || scale <= 0.0 {
        tracing::warn!("invalid system DPI scale ({scale}); assuming 1.0");
        return None;
    }

    // Cursor in physical screen pixels.
    let cx = (screen_pt_logical.0 * scale).round() as i32;
    let cy = (screen_pt_logical.1 * scale).round() as i32;

    // Gather visible, non-minimized top-level window rects (physical px).
    let mut collector = Collector::new();
    let lparam = LPARAM(&mut collector as *mut Collector as isize);
    if let Err(e) = unsafe { EnumWindows(Some(enum_proc), lparam) } {
        tracing::warn!("EnumWindows failed: {e}");
        return None;
    }

    // Among candidates that CONTAIN the cursor, pick the smallest area.
    let mut best: Option<(i32, i32, i32, i32)> = None;
    let mut best_area = i64::MAX;
    for &(l, t, r, b) in &collector.rects {
        if cx < l || cx >= r || cy < t || cy >= b {
            continue;
        }
        let area = (r as i64 - l as i64) * (b as i64 - t as i64);
        if area < best_area {
            best_area = area;
            best = Some((l, t, r, b));
        }
    }

    let (l, t, r, b) = best?;
    let (x, y) = (l as f32 / scale, t as f32 / scale);
    let (w, h) = ((r - l) as f32 / scale, (b - t) as f32 / scale);
    tracing::trace!("window_rect_at: hover rect=({x:.1},{y:.1},{w:.1},{h:.1}) scale={scale:.2}");
    Some((x, y, w, h))
}

/// `EnumWindows` callback. Gather candidate rects; never abort enumeration.
unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    enum_proc_inner(hwnd, lparam)
}

fn enum_proc_inner(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let p = lparam.0 as *mut Collector;
    // Defensive: shouldn't happen, but never panic inside a Win32 callback.
    if p.is_null() {
        return BOOL::from(true);
    }
    // SAFETY: `p` is a valid `&mut Collector` passed via LPARAM by the caller
    // of `EnumWindows`; it outlives the enumeration.
    let c = unsafe { &mut *p };

    // SAFETY: `hwnd` is provided by EnumWindows.
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL::from(true);
    }
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return BOOL::from(true);
    }

    let mut rect = RECT::default();
    // SAFETY: `rect` is a valid out-pointer.
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return BOOL::from(true);
    }
    let (l, t, r, b) = (rect.left, rect.top, rect.right, rect.bottom);
    if (r - l) <= 0 || (b - t) <= 0 {
        return BOOL::from(true);
    }

    // Skip windows with no title; they're usually not user-targetable.
    let mut buf = [0u16; 512];
    // SAFETY: `buf` is a valid writable slice.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if n <= 0 {
        return BOOL::from(true);
    }

    c.rects.push((l, t, r, b));
    BOOL::from(true)
}
