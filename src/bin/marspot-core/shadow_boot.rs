//! Panes in no window at boot.  Whose child a pane is (`shadow_of`) and
//! whether it is shown are two things, so the core keeps a record of
//! the panes in no window (`hidden-panes` in the state directory,
//! written as `shadows` changes) and a core starting up puts the live
//! ones back into `shadows` instead of into the first window:
//!
//! - a top pane in the record stays hidden, as it was;
//! - a child in the record, or any child when there is no record yet
//!   (a core from before it), goes back to the shadows;
//! - a child the record leaves out was shown: a saved window that names
//!   it shows it again, and one whose slot was lost is adopted into the
//!   boot window like any pane whose slot was lost.

use super::*;
use std::collections::HashSet;

const RECORD: &str = "hidden-panes";

fn record_path() -> std::path::PathBuf {
    marspot_term::paths::state_root().join(RECORD)
}

/// The panes the last core had in no window; `None` when it kept no
/// record.  One session id per line; a line that does not read is
/// skipped.
pub(super) fn hidden_record() -> Option<HashSet<u64>> {
    let text = std::fs::read_to_string(record_path()).ok()?;
    Some(text.lines().filter_map(|l| l.trim().parse().ok()).collect())
}

/// Write the panes in no window now.
pub(super) fn write_hidden_record(sids: &[u64]) {
    let path = record_path();
    let tmp = path.with_extension("tmp");
    let body: String = sids.iter().map(|s| format!("{s}\n")).collect();
    if let Err(e) = std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, &path)) {
        lx_warn!("core.shadow.record_unwritten", &format!("{e}"), n = sids.len());
    }
}

/// Session ids that stay in no window at boot, by the rules above.
pub(super) fn stay_hidden(entries: &[marspot_term::session_registry::SessionEntry]) -> HashSet<u64> {
    let record = hidden_record();
    entries.iter().filter(|e| hidden_at_boot(e.id, e.shadow_of, record.as_ref())).map(|e| e.id).collect()
}

fn hidden_at_boot(sid: u64, shadow_of: u64, record: Option<&HashSet<u64>>) -> bool {
    match record {
        Some(r) => r.contains(&sid),
        None => shadow_of != 0,
    }
}

/// Reattach every live session that stays hidden and is not in `taken`
/// (the panes the windows hold or will hold).  One whose reattach fails
/// is left on disk, as an orphan window pane would be.
pub(super) fn adopt_shadows_at_boot(taken: &HashSet<u64>, event_tx: &Sender<CoreEvent>) -> Vec<Pane> {
    let mut shadows = Vec::new();
    let entries = list_session_entries();
    let record = hidden_record();
    for e in entries.iter().filter(|e| hidden_at_boot(e.id, e.shadow_of, record.as_ref()) && !taken.contains(&e.id)) {
        if !session_registry::entry_is_live(e) {
            continue;
        }
        if record.is_none() {
            lx_warn!("core.boot.shadow_unrecorded",
                "no record of which panes were in no window; a child no saved window holds goes to the shadows",
                session = e.id, parent = e.shadow_of);
        }
        unwatched::set(e.id, true);
        match reattach_l3_pane(e.id, event_tx) {
            Ok(pane) => {
                lx_event!("SHADOW_ADOPTED", "a live pane in no window is back in the shadows", session = e.id,
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

#[cfg(test)]
#[path = "shadow_boot_tests.rs"]
mod tests;

#[cfg(test)]
#[test]
fn shadow_boot_hidden_by_the_record_or_by_parent_without_one() {
    let record: HashSet<u64> = [1, 2].into();
    assert!(hidden_at_boot(1, 0, Some(&record)), "a top pane in no window stays there");
    assert!(hidden_at_boot(2, 9, Some(&record)));
    assert!(!hidden_at_boot(3, 9, Some(&record)), "a child that was shown is shown again");
    assert!(!hidden_at_boot(4, 0, Some(&record)));
    assert!(hidden_at_boot(3, 9, None), "with no record, a child goes to the shadows");
    assert!(!hidden_at_boot(4, 0, None));
}
