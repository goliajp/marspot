//! The shell's part in a theme change.
//!
//! Two jobs, both on the supervisor tick.  The shell is the process
//! with an application object, so it is the one that can ask which
//! appearance the system is in; it publishes the answer for the core
//! and the sessions whenever the answer changes.  And it owns three
//! things drawn outside the core's frames — the window's background,
//! the present layer's background and the status banner — which have
//! to follow whatever theme is now in force.
//!
//! The appearance is read on the tick rather than observed: the tick
//! already runs once a second on the main thread, which is as soon as
//! anything else here reacts, and a read cannot miss a notification.

use std::sync::atomic::{AtomicU8, Ordering};

use marspot::lx_warn;
use marspot_term::palette::{self, ThemeId};
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAppearanceNameAccessibilityHighContrastAqua,
    NSAppearanceNameAccessibilityHighContrastDarkAqua, NSAppearanceNameAqua,
    NSAppearanceNameDarkAqua, NSApplication, NSColor,
};
use objc2_foundation::{MainThreadMarker, NSArray};

#[path = "language_follow.rs"]
mod language_follow;
#[path = "pane_app.rs"]
mod pane_app;

/// What was last published as the system's theme; `u8::MAX` before
/// the first tick.
static PUBLISHED: AtomicU8 = AtomicU8::new(u8::MAX);

/// The theme the system's appearance asks for.  Increased contrast on
/// a dark appearance is the high-contrast theme; on a light one it is
/// the light theme, which is the only light one there is.
fn system_theme(mtm: MainThreadMarker) -> ThemeId {
    let appearance = NSApplication::sharedApplication(mtm).effectiveAppearance();
    // SAFETY: AppKit's own constant strings, valid for the process.
    let (aqua, dark, hc_aqua, hc_dark) = unsafe {
        (
            NSAppearanceNameAqua,
            NSAppearanceNameDarkAqua,
            NSAppearanceNameAccessibilityHighContrastAqua,
            NSAppearanceNameAccessibilityHighContrastDarkAqua,
        )
    };
    let names = NSArray::from_slice(&[aqua, dark, hc_aqua, hc_dark]);
    match appearance.bestMatchFromAppearancesWithNames(&names) {
        Some(name) if *name == *hc_dark => ThemeId::Contrast,
        Some(name) if *name == *dark => ThemeId::Dark,
        Some(_) => ThemeId::Light,
        None => ThemeId::Dark,
    }
}

/// Sandbox only: a file that stands in for the system's appearance, so
/// a test can change it without touching the machine's real setting.
fn sandbox_system_theme() -> Option<ThemeId> {
    std::env::var_os("MARSPOT_DEV_SANDBOX")?;
    let path = std::env::var_os("MARSPOT_DEV_SYSTEM_APPEARANCE")?;
    ThemeId::parse(&std::fs::read_to_string(path).ok()?)
}

/// What has to be in force before the first window and the menu bar
/// exist: the interface language, and the ground the window opens on.
/// The watch for applications that panes start begins here too.
/// For the shell that stays after the app was put away: nothing the
/// app watched the system for is watched any more.
pub(crate) fn put_away() {
    pane_app::unwatch();
}

pub(crate) fn at_launch() -> (f64, f64, f64) {
    language_follow::at_launch();
    pane_app::watch();
    marspot::system_theme::startup_palette().bg
}

impl super::ShellApp {
    /// Publish the system's theme if it changed, then bring the
    /// shell's own surfaces to the theme in force if that changed.
    pub(super) fn follow_theme(&mut self) {
        // the language is followed on the same tick
        self.follow_language();
        let Some(mtm) = MainThreadMarker::new() else { return };
        let system = sandbox_system_theme().unwrap_or_else(|| system_theme(mtm));
        if PUBLISHED.swap(system as u8, Ordering::Relaxed) != system as u8
            && let Err(e) = marspot_term::system_theme::publish(system)
        {
            lx_warn!("shell.system_theme.publish_failed", &e.to_string());
        }

        if !marspot_term::system_theme::changed("the shell's windows, present layer and banner follow the theme") {
            return;
        }
        let (r, g, b) = palette::current().bg;
        let bg = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
        let windows = NSApplication::sharedApplication(mtm).windows();
        for w in self.windows.iter_mut() {
            let Some(p) = w.presenter.as_ref() else { continue };
            crate::present::set_layer_background(&p.layer);
            // The window this presenter draws into is the one whose
            // content view carries its layer.
            let ours = Retained::as_ptr(&p.layer) as *const std::ffi::c_void;
            for window in windows.iter() {
                let layer = window.contentView().and_then(|v| v.layer());
                if layer.is_some_and(|l| Retained::as_ptr(&l) as *const std::ffi::c_void == ours) {
                    window.setBackgroundColor(Some(&bg));
                }
            }
            w.frame_pending = true;
        }
        // A banner on screen is a raster in the old theme's colours;
        // forgetting which one is up makes the tick draw it again.
        self.banner_kind = None;
    }
}
