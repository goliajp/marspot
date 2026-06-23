//! Theme — semantic colors + spacing + radius tokens.
//!
//! See `docs/ui-system-model.md` §2.3.  All components reference
//! these by name; literal `Color::rgba(...)` calls in component
//! code are an anti-pattern and will be cleaned up in P3i.
//!
//! ## ThemeId (P3v)
//!
//! `ThemeId::{Dark, Light, HighContrast}` global lets users (future)
//! flip the active palette.  v1 only `Dark` token data exists;
//! `current()` always returns `ThemeId::Dark`.  Light + HC will
//! ship as additional token data files swapped at lookup time
//! (P3v-2 follow-up).

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

pub mod token;

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
