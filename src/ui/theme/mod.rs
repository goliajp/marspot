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

use std::sync::atomic::{AtomicU8, Ordering};

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

/// Read the currently-active theme.
pub fn current() -> ThemeId {
    match CURRENT_THEME.load(Ordering::Relaxed) {
        1 => ThemeId::Light,
        2 => ThemeId::HighContrast,
        _ => ThemeId::Dark,
    }
}

/// Set the active theme.  v1 only `Dark` has real token data, so
/// switching to Light / HighContrast currently produces no visual
/// change (the `token::color::*` constants are Dark only).  This
/// API ships now so callers can wire up the menu / preference; the
/// per-theme color tables ship in P3v-2.
pub fn set_current(id: ThemeId) {
    CURRENT_THEME.store(id as u8, Ordering::Relaxed);
}
