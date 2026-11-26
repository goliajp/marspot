//! The processes closed panes left running, for the process monitor:
//! the shell's zombie watch records them (`marspot::zombie::orphans`),
//! the monitor lists the ones still running as the last entry of its
//! panes, for a person to end.  Nothing here ends them on its own.

use marspot::pidtree::{ProcNode, ProcRow};
use marspot::zombie::orphans;

/// The trees of what closed panes left, from the record on disk; None
/// when nothing they left runs.  A local file read, like the panes'
/// entry files, once a refresh.
pub(super) fn left(all: &[ProcRow]) -> Option<Vec<(String, ProcNode)>> {
    let body = std::fs::read_to_string(orphans::path(&marspot::paths::state_root())).ok()?;
    left_in(&body, all)
}

/// [`left`] from the record's text `body`: every line whose process
/// still runs as the one recorded, with its descendants, labelled
/// with the pane's name before it closed and the command.  The oldest
/// first, as the record has them.
pub(super) fn left_in(body: &str, all: &[ProcRow]) -> Option<Vec<(String, ProcNode)>> {
    let mut out = Vec::new();
    for o in orphans::parse(body) {
        let Some(row) = all.iter().find(|r| r.pid == o.id.pid) else { continue };
        if row.start_us != o.id.start_us || out.iter().any(|(_, t): &(String, ProcNode)| t.pid == o.id.pid) {
            continue;
        }
        let Some(tree) = marspot::pidtree::tree_rooted_at(row.pid, all) else { continue };
        let pane = if o.name.is_empty() { format!("sid {}", o.sid) } else { o.name };
        out.push((format!("{pane} · {}", row.comm), tree));
    }
    (!out.is_empty()).then_some(out)
}
