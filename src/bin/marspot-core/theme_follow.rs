//! The core's part in a theme change: notice it, and draw every
//! window again.
//!
//! Nothing here chooses the theme.  The settings file names it; when
//! it says `system`, the shell's answer is in a file beside it.  Both
//! are looked at here once a second — two `stat`s at most, on a wake
//! the loop already has — which is also what makes a hand edit of the
//! settings file reach the core at all: until now it re-read the file
//! only when the settings panel opened or wrote.  Either way the
//! change lands in the palette's version counter, and that is what
//! every pass of the loop checks: one atomic load.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

#[path = "language_follow.rs"]
mod language_follow;

/// When the files are next looked at, in milliseconds of uptime.  Zero
/// at start: the first pass of the loop looks, so the first frame is
/// already drawn in the theme the file names.
static LOOK_AT_MS: AtomicU64 = AtomicU64::new(0);
const LOOK_EVERY_MS: u64 = 1000;

fn uptime_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

impl super::CoreApp {
    /// Look at the settings once a second, and redraw every window if
    /// the theme in force has changed since the last call.  A pane's
    /// cached instances are keyed by the theme, so the frame this asks
    /// for rebuilds them.
    pub(super) fn follow_theme(&mut self) {
        let now = uptime_ms();
        let looked = now >= LOOK_AT_MS.load(Ordering::Relaxed);
        if looked {
            LOOK_AT_MS.store(now + LOOK_EVERY_MS, Ordering::Relaxed);
            marspot::settings::reload_if_changed();
        }
        // the language rides the same look
        self.follow_language(looked);
        if marspot::system_theme::changed("every window is drawn again in the theme now in force") {
            for w in self.windows.iter_mut() {
                w.needs_render = true;
            }
        }
    }
}
