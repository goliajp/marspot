//! Which sessions nobody is looking at: the shadow panes.
//!
//! Each session's control reader asks here, on its own thread, before
//! it wakes the loop for a new frame, writes the clipboard or passes a
//! bell on.  A reader is respawned on every reconnect and knows only
//! its session id, so the answer is kept by id where every reader can
//! read it without a lock or an allocation.  There are never more live
//! sessions than the process cap, so a table that size holds them all.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

const SLOTS: usize = 64;

static SHADOWS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// How many slots hold a session: with none, every frame of every pane
/// is answered with one load instead of a scan.
static MARKED: AtomicUsize = AtomicUsize::new(0);

/// Mark session `sid` as watched (`false`) or not (`true`).
pub(super) fn set(sid: u64, unwatched: bool) {
    if sid == 0 {
        return;
    }
    if !unwatched {
        for slot in &SHADOWS {
            if slot.compare_exchange(sid, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                MARKED.fetch_sub(1, Ordering::AcqRel);
            }
        }
        return;
    }
    if is(sid) {
        return;
    }
    // counted before the slot is written, so a reader that sees the
    // slot never sees a count of 0
    MARKED.fetch_add(1, Ordering::AcqRel);
    let took = SHADOWS
        .iter()
        .any(|slot| slot.compare_exchange(0, sid, Ordering::AcqRel, Ordering::Relaxed).is_ok());
    if !took {
        MARKED.fetch_sub(1, Ordering::AcqRel);
        // more shadows than live sessions can exist: only a leak gets here
        marspot::lx_warn!(
            "core.shadow.unwatched_table_full",
            "no slot left to mark a shadow pane; it wakes the loop like a drawn one",
            session = sid
        );
    }
}

/// Session `sid` is a shadow pane.
pub(super) fn is(sid: u64) -> bool {
    sid != 0 && MARKED.load(Ordering::Acquire) != 0 && SHADOWS.iter().any(|slot| slot.load(Ordering::Acquire) == sid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_unwatched_marks_and_clears_by_session() {
        set(0x5A00_0001, true);
        set(0x5A00_0001, true);
        assert!(is(0x5A00_0001));
        assert!(!is(0x5A00_0002));
        assert_eq!(SHADOWS.iter().filter(|s| s.load(Ordering::Relaxed) == 0x5A00_0001).count(), 1, "once");
        let marked = MARKED.load(Ordering::Relaxed);
        assert!(marked >= 1);
        set(0x5A00_0001, false);
        set(0x5A00_0001, false);
        assert!(!is(0x5A00_0001));
        assert_eq!(MARKED.load(Ordering::Relaxed), marked - 1, "cleared once");
    }
}
