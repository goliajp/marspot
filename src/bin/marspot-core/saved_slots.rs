//! What the saved layout says of sessions, settled before the boot
//! assembly reads it.

use super::*;

/// Empty, in the saved layout, the slots of the sessions whose close was
/// not finished, and write the layout back, before the boot sweep
/// finishes those closes: the sweep removes what marks them closed, and
/// a window restored from a layout still naming them, by this core or
/// one after it, would start their slots afresh.
pub(super) fn forget_closed_slots(saved: &mut Option<marspot::state::SavedState>) {
    let closed: std::collections::HashSet<u64> = session_registry::closed_session_ids().into_iter().collect();
    let Some(state) = saved.as_mut() else { return };
    let mut emptied = false;
    for w in &mut state.windows {
        emptied |= empty_closed_slots(w, &closed);
    }
    if emptied && let Err(e) = marspot::state::write(state) {
        lx_warn!("core.state_file.write_failed", &format!("{e}; closed slots not persisted"));
    }
}

/// A slot whose session was closed keeps its place and nothing else; a
/// window left with only such slots keeps one that starts a shell.
pub(super) fn empty_closed_slots(
    w: &mut marspot::state::SavedWindowLayout,
    closed: &std::collections::HashSet<u64>,
) -> bool {
    use marspot::state::PANE_FLAG_DORMANT;
    let mut first = None;
    for (i, p) in w.panes.iter_mut().enumerate() {
        if p.sid != 0 && closed.contains(&p.sid) {
            *p = marspot::state::SavedPane { flags: PANE_FLAG_DORMANT, ..Default::default() };
            first.get_or_insert(i);
        }
    }
    if let Some(i) = first
        && w.panes.iter().all(|p| p.flags & PANE_FLAG_DORMANT != 0)
    {
        w.panes[i].flags &= !PANE_FLAG_DORMANT;
    }
    first.is_some()
}

/// Every sid the saved windows other than the boot one own: those
/// sessions are neither orphans to adopt nor junk to retire, they wait
/// for their own window to be opened.
pub(super) fn reserved_sids(
    saved_windows: &std::collections::VecDeque<(usize, marspot::state::SavedWindowLayout)>,
) -> std::collections::HashSet<u64> {
    let panes = saved_windows.iter().flat_map(|(_, w)| w.panes.iter());
    panes.map(|p| p.sid).filter(|sid| *sid != 0).collect()
}
