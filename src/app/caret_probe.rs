//! Where the IME candidate window would go, printed for a script.

use objc2_app_kit::NSTextInputClient;
use objc2_foundation::{NSRange, NSRect};

use super::MarspotAppCtx;

/// Dev seam (`MARSPOT_DEV_CARET_PROBE=1`) — print where the IME
/// candidate window would be anchored for `ctx`, asking the view the
/// same question AppKit asks it.
///
/// A script cannot see an IME candidate window, so this is how
/// `bin/test-multi-window.sh` proves each window anchors the candidate
/// box against its OWN caret: the screen rect printed here has to fall
/// inside that window's own frame.  Off unless the env var is set —
/// it runs one Objective-C round-trip per caret move.
pub(super) fn caret_probe(ctx: &MarspotAppCtx, rect: Option<(f64, f64, f64, f64)>) {
    // Read the env once: a caret lands at most once per rendered
    // frame, and `env::var_os` locks the environment and scans it.
    thread_local! {
        static ON: bool = std::env::var_os("MARSPOT_DEV_CARET_PROBE").is_some();
    }
    if !ON.with(|on| *on) {
        return;
    }
    // SAFETY: our own NSTextInputClient method on our own view, on the
    // main thread; a null `actualRange` is what AppKit itself passes
    // when it does not want the resolved range back.
    let screen: NSRect = unsafe {
        ctx.inner.firstRectForCharacterRange_actualRange(
            NSRange::new(0, 0),
            std::ptr::null_mut(),
        )
    };
    let f = ctx.nswindow.frame();
    eprintln!(
        "[marspot] ime.caret window_id={} phys={} screen={:.1},{:.1},{:.1},{:.1} \
         window={:.1},{:.1},{:.1},{:.1}",
        ctx.window_id,
        rect.is_some(),
        screen.origin.x,
        screen.origin.y,
        screen.size.width,
        screen.size.height,
        f.origin.x,
        f.origin.y,
        f.size.width,
        f.size.height,
    );
}
