//! Platform (OS-specific) helpers.
//!
//! Phase 1b: window-under-cursor detection for the region overlay's hover-snap.
//! Phase 1d: cursor position in the virtual-screen physical space + multi-
//! monitor-aware hit-testing (the overlay origin is now the virtual-desktop
//! top-left `vmin`, which may be negative).

#[cfg(windows)]
mod windows_impl;

#[cfg(windows)]
pub use windows_impl::cursor_screen_physical;

#[cfg(windows)]
pub use windows_impl::window_rect_at;

/// Physical screen coordinates of the OS cursor in the virtual screen space, or None.
#[cfg(not(windows))]
#[allow(dead_code)]
pub fn cursor_screen_physical() -> Option<(i32, i32)> {
    None
}

/// On non-Windows, there is no window detection (returns None).
#[cfg(not(windows))]
#[allow(dead_code)]
pub fn window_rect_at(
    _local_pt_logical: (f32, f32),
    _origin_physical: (i32, i32),
    _scale: f32,
) -> Option<(f32, f32, f32, f32)> {
    None
}
