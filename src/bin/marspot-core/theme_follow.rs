//! The core's part in a theme change: notice it, and draw every
//! window again.
//!
//! Nothing here chooses the theme.  The settings file names it; when
//! it says `system`, the shell's answer is in a file beside it.  Both
//! are watched, with the language file that rides the same look, and
//! read when one of them changes — which is also what makes a hand
//! edit of the settings file reach the core at all.  Either way the
//! change lands in the palette's version counter, and that is what
//! every pass of the loop checks: one atomic load.
//!
//! They used to be looked at once a second, two `stat`s on a wake the
//! loop took for nothing else.  That beat is kept only for when the
//! files cannot be watched.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use marspot::fs_watch::Watcher;

#[path = "language_follow.rs"]
mod language_follow;

/// When the files are next looked at, in milliseconds of uptime.  Zero
/// at start: the first pass of the loop looks, so the first frame is
/// already drawn in the theme the file names.
static LOOK_AT_MS: AtomicU64 = AtomicU64::new(0);
const LOOK_EVERY_MS: u64 = 1000;

/// Set by the watcher; set at start so the first pass of the loop looks.
static CHANGED: AtomicBool = AtomicBool::new(true);
static WATCHER: OnceLock<Option<Watcher>> = OnceLock::new();

/// Watch the files; `wake` is called, from the watcher's thread, after
/// one of them changed.
pub(super) fn watch(wake: impl Fn() + Send + 'static) {
    let files = [
        marspot::settings::path(),
        marspot::system_theme::path(),
        marspot::system_language::path(),
    ];
    WATCHER.get_or_init(|| {
        let w = Watcher::spawn(&files, move || {
            CHANGED.store(true, Ordering::Release);
            wake();
        });
        if let Err(e) = &w {
            marspot::lx_warn!("core.settings.unwatched", &format!("{e}; looking once a second instead"));
        }
        w.ok()
    });
}

pub(super) fn watching() -> bool {
    WATCHER.get().and_then(Option::as_ref).is_some_and(Watcher::alive)
}

fn uptime_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

impl super::CoreApp {
    /// Look at the settings when their files changed, and redraw every
    /// window if the theme in force has changed since the last call.  A
    /// pane's cached instances are keyed by the theme, so the frame this
    /// asks for rebuilds them.
    pub(super) fn follow_theme(&mut self) {
        let now = uptime_ms();
        let changed = CHANGED.swap(false, Ordering::AcqRel);
        let looked = changed || (!watching() && now >= LOOK_AT_MS.load(Ordering::Relaxed));
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
