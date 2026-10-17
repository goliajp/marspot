//! Theme — semantic colors + spacing + radius tokens.
//!
//! All components reference
//! these by name; literal `Color::rgba(...)` calls in component
//! code are an anti-pattern and will be cleaned up in P3i.
//!
//! ## ThemeId (P3v)
//!
//! `ThemeId::{Dark, Light, HighContrast}` global is meant to let users
//! flip the active palette.  Nothing reads `current()` yet: every
//! component uses the Dark constants, so `set_current()` changes no
//! colour; it only bumps `version()`, which the shell polls to
//! schedule a redraw.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

pub mod panel_type;
pub mod token;

pub use panel_type::PanelText;
pub use token::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ThemeId {
    Dark = 0,
    Light = 1,
    HighContrast = 2,
}

static CURRENT_THEME: AtomicU8 = AtomicU8::new(ThemeId::Dark as u8);
static THEME_VERSION: AtomicU64 = AtomicU64::new(0);

/// Read the currently-active theme.
pub fn current() -> ThemeId {
    match CURRENT_THEME.load(Ordering::Relaxed) {
        1 => ThemeId::Light,
        2 => ThemeId::HighContrast,
        _ => ThemeId::Dark,
    }
}

/// Set the active theme.  Bumps `version()` so observers can
/// invalidate caches + trigger redraw.  v1 host wiring:
///
/// ```ignore
/// let mut last_theme_v = 0;
/// fn redraw(&mut self, ctx) {
///     let v = theme::version();
///     if v != last_theme_v { ctx.request_redraw(); last_theme_v = v; }
///     // ... build / layout / paint ...
/// }
/// ```
pub fn set_current(id: ThemeId) {
    CURRENT_THEME.store(id as u8, Ordering::Relaxed);
    THEME_VERSION.fetch_add(1, Ordering::Relaxed);
}

/// Theme-data version counter.  Bumped by `set_current()`.  Hosts
/// poll this to detect a theme swap and invalidate themed caches +
/// schedule a redraw.
pub fn version() -> u64 {
    THEME_VERSION.load(Ordering::Relaxed)
}
