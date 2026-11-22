use super::{live_frames_by_slot, merge_window_frames};
use marspot::state::SavedWindow;

fn win(x: f64) -> SavedWindow {
    SavedWindow { display_id: 1, x, y: 10.0, w: 1200.0, h: 800.0 }
}

/// The 2026-08-03 half of the same bug: close the FIRST of two
/// windows and the survivor used to write itself into slot 0,
/// overwriting the closed window's geometry — so when the closed
/// window came back (it is parked, not discarded) the two had
/// swapped places.
#[test]
fn a_closed_window_keeps_its_slot_for_the_window_still_open() {
    let prev = vec![win(0.0), win(872.0)];
    // Window 0 closed; window 1 is still open and still slot 1.
    let live = live_frames_by_slot(
        [(1usize, Some(win(880.0)))].into_iter(),
        prev.len(),
    );
    let out = merge_window_frames(&prev, &live);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].x, 0.0, "the closed window's geometry is untouched");
    assert_eq!(out[1].x, 880.0, "the open one wrote its own slot");
}

/// A window opened past everything on disk grows the list.
#[test]
fn a_window_in_a_new_slot_extends_the_list() {
    let live = live_frames_by_slot(
        [(0usize, Some(win(1.0))), (2usize, Some(win(2.0)))].into_iter(),
        1,
    );
    assert_eq!(live.len(), 3);
    assert!(live[1].is_none(), "the gap stays a gap");
    assert_eq!(live[2].as_ref().unwrap().x, 2.0);
}

/// The silent-update regression: one live window, two on disk.
/// The second entry is the geometry the boot restore is about to
/// ask for, so it has to survive window 0's save.
#[test]
fn a_single_live_window_does_not_delete_the_other_saved_frames() {
    let prev = vec![win(0.0), win(872.0)];
    let live = vec![Some(win(5.0))];
    let out = merge_window_frames(&prev, &live);
    assert_eq!(out.len(), 2, "saved tail must survive");
    assert_eq!(out[0].x, 5.0, "live window wins its own slot");
    assert_eq!(out[1].x, 872.0, "window 1 keeps its saved geometry");
}

/// A live window with no frame yet keeps its slot pointing at the
/// saved value — the old `filter_map` dropped it and shifted the
/// windows after it up by one.
#[test]
fn a_frameless_live_window_keeps_its_slot_instead_of_shifting() {
    let prev = vec![win(0.0), win(100.0), win(200.0)];
    let live = vec![Some(win(1.0)), None, Some(win(201.0))];
    let out = merge_window_frames(&prev, &live);
    assert_eq!(
        out.iter().map(|w| w.x).collect::<Vec<_>>(),
        vec![1.0, 100.0, 201.0],
    );
}

/// Nothing live and nothing saved for a slot: a hole, not a shift.
#[test]
fn a_slot_with_no_history_becomes_a_hole_not_a_shift() {
    let live = vec![None, Some(win(9.0))];
    let out = merge_window_frames(&[], &live);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].w, 0.0, "hole is zero-sized (restore rejects it)");
    assert_eq!(out[1].x, 9.0, "later window keeps index 1");
}

#[test]
fn a_new_window_past_the_saved_list_is_appended() {
    let prev = vec![win(0.0)];
    let live = vec![Some(win(0.0)), Some(win(500.0))];
    let out = merge_window_frames(&prev, &live);
    assert_eq!(out.len(), 2);
    assert_eq!(out[1].x, 500.0);
}

#[test]
fn no_windows_and_no_history_writes_nothing() {
    assert!(merge_window_frames(&[], &[]).is_empty());
}
