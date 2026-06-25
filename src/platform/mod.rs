//! Platform (OS-specific) helpers.
//!
//! Phase 1b: window-under-cursor detection for the region overlay's hover-snap.

#[cfg(windows)]
mod windows_impl;

#[cfg(windows)]
pub use windows_impl::window_rect_at;

/// On non-Windows, there is no window detection (returns None).
#[cfg(not(windows))]
#[allow(dead_code)]
pub fn window_rect_at(_screen_pt: (f32, f32)) -> Option<(f32, f32, f32, f32)> {
    None
}
