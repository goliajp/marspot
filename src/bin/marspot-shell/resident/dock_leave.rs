//! When the resident takes the app's tile out of the Dock.
//!
//! The resident leaves the Dock by turning accessory.  The Dock does not
//! drop the tile then: it drops it about half a second later, on one
//! timer per app that every later leave starts again, and when that
//! timer fires it drops the tile of the latest leave only.  Opening the
//! resident turns the process regular before its image is replaced
//! (LaunchServices does that itself, as it hands over the open), and the
//! Dock puts in a new tile at once.  So a quit that follows within that
//! half second restarts the timer, and the tile of the leave before is
//! never dropped: it stays in the Dock after the process is gone, listed
//! by no app list and no Dock preference, until the Dock restarts.
//!
//! Any earlier leave of this process happened before the image that is
//! the app started.  So that image leaves the Dock no sooner than
//! [`SETTLE`] after it started: by then the Dock has dropped the earlier
//! tile.  Until then the resident keeps its tile and stays regular, and
//! opening it again needs no new tile at all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use marspot::lx_event;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

/// Twice the half second the Dock was seen to wait (0.53 to 0.55 s on
/// macOS 27.2) before it drops a tile that left.
pub(crate) const SETTLE: Duration = Duration::from_millis(1_000);

static IMAGE_START: OnceLock<Instant> = OnceLock::new();

/// Set while the resident keeps its tile, waiting to leave the Dock.
static PENDING: AtomicBool = AtomicBool::new(false);

/// Called first thing in `main`: when this image started.
pub(crate) fn mark_image_start() {
    let _ = IMAGE_START.set(Instant::now());
}

/// How long this image has run; a start never marked counts as now,
/// which only makes the wait longer.
pub(super) fn image_age() -> Duration {
    IMAGE_START.get().map_or(Duration::ZERO, Instant::elapsed)
}

/// How long an image of `age` waits before it leaves the Dock.
pub(super) fn delay(age: Duration) -> Duration {
    SETTLE.saturating_sub(age)
}

/// Note that the resident keeps its tile until [`leave_now`].
pub(super) fn hold() {
    PENDING.store(true, Ordering::SeqCst);
}

/// Whether the resident still has its tile.
pub(super) fn pending() -> bool {
    PENDING.load(Ordering::SeqCst)
}

/// Take the tile out of the Dock.
pub(super) fn leave_now(app: &NSApplication) {
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    PENDING.store(false, Ordering::SeqCst);
    lx_event!("SHELL_RESIDENT_REGISTERED", "registered with no Dock icon", image_age_ms = image_age().as_millis() as u64);
}

/// Whether a request to quit the resident is turned down: one from the
/// tile it still has is the user quitting an app that already quit.
/// Only the end of the login session, which says so, ends it then.
pub(super) fn turn_down_quit(tile_held: bool, ends_login_session: bool) -> bool {
    tile_held && !ends_login_session
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_young_image_waits_out_the_rest_of_the_settle_time() {
        assert_eq!(delay(Duration::ZERO), SETTLE);
        assert_eq!(delay(Duration::from_millis(300)), Duration::from_millis(700));
        assert_eq!(delay(Duration::from_millis(999)), Duration::from_millis(1));
    }

    #[test]
    fn an_image_older_than_the_settle_time_leaves_at_once() {
        assert_eq!(delay(SETTLE), Duration::ZERO);
        assert_eq!(delay(Duration::from_secs(3600)), Duration::ZERO);
    }

    #[test]
    fn the_wait_is_longer_than_the_docks_own() {
        // the Dock dropped a left tile 0.531 to 0.546 s after the leave
        assert!(SETTLE >= Duration::from_millis(546) * 3 / 2);
    }

    #[test]
    fn a_quit_while_the_tile_is_held_is_turned_down_unless_the_login_session_ends() {
        assert!(turn_down_quit(true, false));
        assert!(!turn_down_quit(true, true));
        assert!(!turn_down_quit(false, false));
        assert!(!turn_down_quit(false, true));
    }

    #[test]
    fn the_image_age_grows_from_its_mark() {
        mark_image_start();
        let a = image_age();
        std::thread::sleep(Duration::from_millis(5));
        assert!(image_age() > a);
        assert!(delay(image_age()) <= SETTLE);
    }

    #[test]
    fn holding_and_leaving_set_the_pending_mark() {
        hold();
        assert!(pending());
        PENDING.store(false, Ordering::SeqCst);
        assert!(!pending());
    }
}
