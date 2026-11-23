//! Shadow panes at boot: the registry says which sessions are shadows
//! (`shadow_of != 0`), and a core starting up puts the live ones back
//! into `shadows` instead of into the first window.
//!
//! A session that a saved window layout also names is that window's:
//! the window wins, and the core tells the session it is a shadow no
//! longer.

use super::shadow_place::{L2_REQ_UNTRACKED, session_shadow_frame};
use super::*;
use std::collections::HashSet;

/// Session ids of registry entries that say they are shadow panes.
pub(super) fn shadow_ids(entries: &[marspot_term::session_registry::SessionEntry]) -> HashSet<u64> {
    entries.iter().filter(|e| e.shadow_of != 0).map(|e| e.id).collect()
}

/// A session reattached into a window while its entry calls it a
/// shadow: it is told it is one no longer.  Its answer goes nowhere.
pub(super) fn unshadow_in_window(pane: &Pane, sid: u64) {
    if pane.session().send_pane_control(session_shadow_frame(L2_REQ_UNTRACKED, 0)) {
        lx_event!(
            "SHADOW_WINDOW_WINS",
            "a saved window holds a session that said it was a shadow; told it is not",
            session = sid
        );
    }
}

/// Reattach every live shadow session not in `taken` (the panes the
/// windows hold or will hold).  One whose reattach fails is left on
/// disk, as an orphan window pane would be.
pub(super) fn adopt_shadows_at_boot(taken: &HashSet<u64>, event_tx: &Sender<CoreEvent>) -> Vec<Pane> {
    let mut shadows = Vec::new();
    let entries = list_session_entries();
    for e in entries.iter().filter(|e| e.shadow_of != 0 && !taken.contains(&e.id)) {
        if !session_registry::entry_is_live(e) {
            continue;
        }
        unwatched::set(e.id, true);
        match reattach_l3_pane(e.id, event_tx) {
            Ok(pane) => {
                lx_event!("SHADOW_ADOPTED", "a live shadow pane is back in the shadows", session = e.id,
                    parent = e.shadow_of);
                shadows.push(pane);
            }
            Err(err) => {
                unwatched::set(e.id, false);
                lx_warn!("core.reattach.shadow_failed", &format!("{err} — left on disk for next boot"),
                    session = e.id);
            }
        }
    }
    shadows
}
