//! A closed session's slot stays empty across every core that starts
//! before its window is restored.

use super::*;
use marspot::state::{PANE_FLAG_DORMANT, SavedState};

/// What a starting core does with the saved layout: settle it, then
/// assemble the boot window with the sweep.  Returns the windows past
/// the boot one, as read.
fn boot(tx: &Sender<CoreEvent>) -> Vec<SavedWindowLayout> {
    let mut state = marspot::state::read();
    let sweep = crate::saved_slots::forget_closed_slots(&mut state);
    let mut windows = state.expect("a saved layout").windows;
    let first = windows.remove(0);
    let reserved = windows.iter().flat_map(|w| w.panes.iter()).map(|p| p.sid).filter(|s| *s != 0).collect();
    assemble_panes_at_boot(Some(&first), first.panes.len(), 60, 16, tx, sweep, &reserved);
    windows
}

/// The core that finished the close exits before the second window is
/// restored; the next core restores it without a session in the slot.
#[test]
fn a_closed_session_is_not_restored_by_the_core_after_the_one_that_finished_its_close() {
    let mut sb = Sandbox::new();
    sb.break_session_bin();
    let (tx, _rx) = marspot_term::mailbox::channel();
    let closed = reg::allocate_next_session_id().unwrap();
    let kept = reg::allocate_next_session_id().unwrap();
    for id in [closed, kept] {
        write_session_entry(&dead_entry(id)).unwrap();
    }
    reg::mark_closed(closed).unwrap();
    let state = SavedState { windows: vec![saved(&[(0, "")]), saved(&[(closed, "c"), (kept, "k")])], key_window: 0 };
    marspot::state::write(&state).unwrap();
    boot(&tx);
    assert!(!reg::session_dir(closed).exists(), "the boot sweep finished the close");
    let later = boot(&tx).remove(0);
    let (panes, _) = assemble_panes_at_boot(Some(&later), 2, 60, 16, &tx, Sweep::Off, &Default::default());
    assert_eq!(panes.iter().map(Pane::is_dormant).collect::<Vec<_>>(), [true, false]);
    assert_eq!(pane_sids(&panes)[1], kept);
    assert!(!reg::session_dir(closed).exists(), "nothing started in the closed session's slot");
}

/// The layout could not be written back: the close stays unfinished,
/// and the core after it, which can write, empties the slot and
/// finishes it.
#[test]
fn a_close_is_not_finished_by_a_core_that_could_not_write_the_layout_back() {
    let mut sb = Sandbox::new();
    sb.break_session_bin();
    let (tx, _rx) = marspot_term::mailbox::channel();
    let closed = reg::allocate_next_session_id().unwrap();
    write_session_entry(&dead_entry(closed)).unwrap();
    reg::mark_closed(closed).unwrap();
    let state = SavedState { windows: vec![saved(&[(0, "")]), saved(&[(closed, "c")])], key_window: 0 };
    marspot::state::write(&state).unwrap();
    // the write goes through this temp file, which cannot be made over a dir
    let blocked = marspot::state::state_file_path().with_extension("bin.tmp");
    std::fs::create_dir(&blocked).unwrap();
    boot(&tx);
    assert!(reg::is_closed(closed), "the close was finished while the layout still names the session");
    std::fs::remove_dir(&blocked).unwrap();
    assert_eq!(boot(&tx).remove(0).panes[0].sid, 0);
    assert!(!reg::session_dir(closed).exists(), "the next core finished the close");
}

/// A window whose every slot was closed keeps one that starts a shell.
#[test]
fn a_window_of_closed_slots_keeps_one_that_starts_a_shell() {
    let mut w = saved(&[(5, ""), (6, ""), (7, "")]);
    w.panes[2].flags = PANE_FLAG_DORMANT;
    assert!(crate::saved_slots::empty_closed_slots(&mut w, &[5, 6].into_iter().collect()));
    let flags: Vec<_> = w.panes.iter().map(|p| (p.sid, p.flags)).collect();
    assert_eq!(flags, [(0, 0), (0, PANE_FLAG_DORMANT), (7, PANE_FLAG_DORMANT)]);
    let mut untouched = saved(&[(5, "")]);
    assert!(!crate::saved_slots::empty_closed_slots(&mut untouched, &[6].into_iter().collect()));
    assert_eq!(untouched.panes[0].sid, 5);
}
