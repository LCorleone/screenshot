//! Win32 implementation of window-under-cursor detection.
//!
//! We use `EnumWindows` + `GetWindowRect` hit-testing rather than
//! `WindowFromPoint`, because our own fullscreen always-on-top overlay would
//! occlude every other window for `WindowFromPoint`. Enumerating top-level
//! windows and hit-testing their rects against the cursor avoids that.
//!
//! All public inputs/outputs are in the overlay's **logical points** space
//! (origin = the overlay's top-left), with `origin_physical` providing the
//! offset from overlay-local to absolute virtual-screen physical px (= `vmin`).
//! `scale` is the primary monitor's scale factor (uniform-DPI assumption).

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetCursorPos, GetWindowRect, GetWindowTextW, IsIconic, IsWindowVisible,
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

/// Physical screen coordinates of the OS cursor (virtual-screen space), or None.
pub fn cursor_screen_physical() -> Option<(i32, i32)> {
    let mut pt = POINT { x: 0, y: 0 };
    // SAFETY: out-pointer is valid.
    if unsafe { GetCursorPos(&mut pt) }.is_ok() {
        Some((pt.x, pt.y))
    } else {
        None
    }
}

/// Returns the rect `(x, y, w, h)` of the topmost/most-specific window under
/// `local_pt_logical`, expressed in the SAME overlay-local logical space as
/// the input (i.e. the window's physical screen rect converted back to the
/// overlay's coordinate frame). `origin_physical` is the overlay's top-left in
/// physical virtual-screen pixels (= `vmin`), and `scale` is physical px per
/// logical point (primary monitor's scale factor; uniform-DPI assumption).
/// Returns `None` if the cursor isn't inside any candidate window. Never panics.
pub fn window_rect_at(
    local_pt_logical: (f32, f32),
    origin_physical: (i32, i32),
    scale: f32,
) -> Option<(f32, f32, f32, f32)> {
    if !scale.is_finite() || scale <= 0.0 {
        tracing::warn!("invalid scale ({scale}) in window_rect_at; assuming 1.0");
        return None;
    }

    // Input: overlay-local logical → physical virtual-screen px.
    let cx = (local_pt_logical.0 * scale).round() as i32 + origin_physical.0;
    let cy = (local_pt_logical.1 * scale).round() as i32 + origin_physical.1;

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
    // Output: physical virtual-screen px → overlay-local logical. Subtract the
    // overlay's physical origin first, then divide by scale. This is the
    // symmetric inverse of the input conversion above.
    let x = ((l - origin_physical.0) as f32) / scale;
    let y = ((t - origin_physical.1) as f32) / scale;
    let w = ((r - l) as f32) / scale;
    let h = ((b - t) as f32) / scale;
    tracing::trace!(
        "window_rect_at: hover rect=({x:.1},{y:.1},{w:.1},{h:.1}) scale={scale:.2} origin=({:?})",
        origin_physical
    );
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
