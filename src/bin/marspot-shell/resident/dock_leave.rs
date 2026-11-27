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
//! opening it again needs no new tile at all.  A resident that replaces
//! its image while it keeps the tile (a lost Apple event registration)
//! hands the time it may leave to the image after it, which waits for
//! it in turn.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use marspot::lx_event;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

/// Twice the half second the Dock was seen to wait (0.53 to 0.55 s on
/// macOS 27.2) before it drops a tile that left.
pub(crate) const SETTLE: Duration = Duration::from_millis(1_000);

/// Carries, across a replacement of the resident's image, the time on
/// [`now_ns`]'s clock before which the process may not leave the Dock.
pub(super) const LEAVE_AT_ENV: &str = "MARSPOT_DOCK_LEAVE_AT";

/// 0 until marked.
static IMAGE_START_NS: AtomicU64 = AtomicU64::new(0);

/// Set while the resident keeps its tile, waiting to leave the Dock.
static PENDING: AtomicBool = AtomicBool::new(false);

/// When the held tile may leave, on [`now_ns`]'s clock.
static LEAVE_AT_NS: AtomicU64 = AtomicU64::new(0);

/// Time since boot that an exec does not restart, unlike `Instant`'s
/// reference in a new image.
fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: fills the struct it is given; this clock always exists
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Called first thing in `main`: when this image started.
pub(crate) fn mark_image_start() {
    let _ = IMAGE_START_NS.compare_exchange(0, now_ns(), Ordering::SeqCst, Ordering::SeqCst);
}

/// How long this image has run; a start never marked counts as now,
/// which only makes the wait longer.
pub(super) fn image_age() -> Duration {
    match IMAGE_START_NS.load(Ordering::SeqCst) {
        0 => Duration::ZERO,
        start => Duration::from_nanos(now_ns().saturating_sub(start)),
    }
}

/// How long an image of `age` waits before it leaves the Dock.
pub(super) fn delay(age: Duration) -> Duration {
    SETTLE.saturating_sub(age)
}

/// What is left at `now` of the wait an earlier image handed over as
/// `carried`; nothing for a value that is not one.
fn left_of(carried: Option<&str>, now: u64) -> Duration {
    let at = carried.and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    Duration::from_nanos(at.saturating_sub(now))
}

/// The wait an earlier image of this process handed over, if any.
/// Consumes the mark.
pub(super) fn take_carried() -> Duration {
    let carried = std::env::var(LEAVE_AT_ENV).ok();
    // SAFETY: startup path in main(), before any thread is spawned.
    unsafe { std::env::remove_var(LEAVE_AT_ENV) };
    left_of(carried.as_deref(), now_ns())
}

/// Note that the resident keeps its tile for `keep` more, until
/// [`leave_now`].
pub(super) fn hold(keep: Duration) {
    LEAVE_AT_NS.store(now_ns().saturating_add(keep.as_nanos() as u64), Ordering::SeqCst);
    PENDING.store(true, Ordering::SeqCst);
}

/// Whether the resident still has its tile.
pub(super) fn pending() -> bool {
    PENDING.load(Ordering::SeqCst)
}

/// For the image that replaces this one: when it may leave the Dock,
/// while this one still has its tile.
pub(super) fn carry() -> Option<String> {
    pending().then(|| LEAVE_AT_NS.load(Ordering::SeqCst).to_string())
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
    fn a_carried_deadline_leaves_the_rest_of_its_wait() {
        assert_eq!(left_of(Some("5000000000"), 4_300_000_000), Duration::from_millis(700));
        assert_eq!(left_of(Some("5000000000"), 5_000_000_000), Duration::ZERO);
        assert_eq!(left_of(Some("5000000000"), 9_000_000_000), Duration::ZERO);
        assert_eq!(left_of(None, 1), Duration::ZERO);
        assert_eq!(left_of(Some("soon"), 1), Duration::ZERO);
    }

    // the one test that touches the process-wide hold and the
    // environment, so tests running beside it cannot disturb either
    #[test]
    fn a_held_tile_hands_its_deadline_to_the_next_image_through_the_environment() {
        assert_eq!(carry(), None);
        hold(Duration::from_millis(800));
        assert!(pending());
        let carried = carry().expect("a held tile is carried");
        // SAFETY: no other test reads or writes this variable
        unsafe { std::env::set_var(LEAVE_AT_ENV, &carried) };
        let left = take_carried();
        assert!(left > Duration::from_millis(700) && left <= Duration::from_millis(800), "{left:?}");
        assert!(std::env::var_os(LEAVE_AT_ENV).is_none());
        assert_eq!(take_carried(), Duration::ZERO);
        PENDING.store(false, Ordering::SeqCst);
        assert_eq!(carry(), None);
    }
}
