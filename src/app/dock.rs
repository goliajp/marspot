//! The app's Dock tile, and whether it asks to come to the front.
//!
//! A shell a gauge or a benchmark starts on a runner still opens its
//! windows and draws them, but nobody is at that machine: as a regular
//! app every start put a tile in the Dock and asked to be activated
//! (which the system refused, since no person asked for it), and the
//! tiles of images that replaced themselves stayed behind.  With
//! [`NO_DOCK_ENV`] set the shell is an accessory app instead: no tile,
//! no menu bar, no activation request.  Window tests that check focus
//! or activation leave it unset and run as the regular app.

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSImage};
use objc2_foundation::NSString;

/// Set (to anything but "" or "0") by whatever starts a shell that no
/// person is going to use.
pub const NO_DOCK_ENV: &str = "MARSPOT_TEST_NO_DOCK";

/// Whether this process runs without a Dock tile.
pub fn hidden() -> bool {
    std::env::var_os(NO_DOCK_ENV).is_some_and(|v| !v.is_empty() && v != "0")
}

/// The activation policy the app's windows are opened under.
pub fn policy() -> NSApplicationActivationPolicy {
    if hidden() {
        NSApplicationActivationPolicy::Accessory
    } else {
        NSApplicationActivationPolicy::Regular
    }
}

/// Ask to become the active app, unless this process has no Dock tile.
pub fn activate(nsapp: &NSApplication) {
    if !hidden() {
        nsapp.activate();
    }
}

/// [`activate`], taking the front from whichever app has it.
pub fn activate_over_others(nsapp: &NSApplication) {
    if !hidden() {
        #[allow(deprecated)] // `activate()` only asks
        nsapp.activateIgnoringOtherApps(true);
    }
}

/// Force-refresh the Dock icon from the bundle's `Resources/AppIcon
/// .icns`: a running app otherwise shows whichever icon was cached at
/// first launch, even after an install lands a new one.
pub fn set_icon_from_bundle(nsapp: &NSApplication) {
    if hidden() {
        return;
    }
    // first launch runs as `<bundle>/Contents/MacOS/marspot-shell`; after
    // a self-exec the image lives under the state dir's `binaries/`, with
    // no Resources next to it, so the installed bundle is the fallback
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(contents) = exe.parent().and_then(|macos| macos.parent())
    {
        candidates.push(contents.join("Resources").join("AppIcon.icns"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".local/Marspot.app/Contents/Resources/AppIcon.icns"));
    }
    let Some(icon_path) = candidates.into_iter().find(|p| p.exists()) else { return };
    let Some(path_str) = icon_path.to_str() else { return };
    let img: Option<Retained<NSImage>> = NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(path_str));
    if let Some(img) = img {
        unsafe { nsapp.setApplicationIconImage(Some(&img)) };
    }
}
