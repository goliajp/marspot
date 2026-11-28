//! The backing scale a window draws at.

use objc2_app_kit::NSWindow;

/// The backing scale of `w`'s screen.  Dev seam: a sandbox can name
/// another with `MARSPOT_DEV_BACKING_SCALE` (1 or 2) and draw every
/// window as a display that dense would, on a runner whose only screen
/// is not; the window server shows the surface scaled to the window.
/// Honoured only by a dev build or under a sandbox state dir.
pub fn backing(w: &NSWindow) -> f64 {
    static DEV: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    let dev = DEV.get_or_init(|| {
        let sandbox = std::env::var_os("MARSPOT_STATE_DIR").is_some_and(|d| !d.is_empty());
        if !cfg!(debug_assertions) && !sandbox {
            return None;
        }
        std::env::var("MARSPOT_DEV_BACKING_SCALE").ok().and_then(|v| v.parse().ok()).filter(|s| *s == 1.0 || *s == 2.0)
    });
    dev.unwrap_or_else(|| w.backingScaleFactor())
}
