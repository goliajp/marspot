//! What each pane last ran claude as, kept on disk so a pane whose
//! session process started afresh (the machine restarted, the app was
//! closed and reopened) can have its claude put back.
//!
//! A row is written when the scan binds a pane, and dropped when the
//! person leaves claude in that same shell: the shell's identity tells
//! "claude exited here" from "this shell is new and claude never ran
//! in it".  A new shell is owed its claude only for `WINDOW` after it
//! started and only while nobody has typed into the pane since.

use super::*;
pub(super) use crate::plugins::cold_restore::{forget, owed, owed_shell, pane_open, started, ColdRow, PaneNow, Unbound, WINDOW};
#[cfg(test)]
use crate::plugins::cold_restore::GONE_GRACE;
#[cfg(test)]
use std::time::{Duration, Instant, SystemTime};

/// Most rows kept; the oldest goes.  One per pane, and the session
/// population is capped well below this.
pub(super) const ROWS_CAP: usize = 128;

/// The last claude a pane ran, and the shell it ran under.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct LastBound {
    pub(super) sid: u64,
    pub(super) uuid: String,
    pub(super) profile: u8,
    pub(super) config_dir: Option<String>,
    pub(super) project_dir: Option<String>,
    pub(super) choice: CliChoice,
    pub(super) shell: pidtree::ProcId,
}

pub(super) fn encode(rows: &[LastBound]) -> String {
    let mut s = String::new();
    for r in rows {
        let opt = |v: &Option<String>| v.clone().unwrap_or_default();
        s.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            r.sid,
            r.uuid,
            r.profile,
            r.shell.pid,
            r.shell.start_us,
            opt(&r.config_dir),
            opt(&r.project_dir),
            opt(&r.choice.model),
            opt(&r.choice.effort)
        ));
    }
    s
}

/// The rows of `text`; a row with a field that could not go on a
/// command line is dropped, since every field ends up on one.
pub(super) fn decode(text: &str) -> Vec<LastBound> {
    let mut rows = Vec::new();
    for row in text.lines().filter_map(decode_row) {
        // a session is one pane's: the later row is the newer binding
        rows.retain(|r: &LastBound| r.uuid != row.uuid && r.sid != row.sid);
        rows.push(row);
    }
    let over = rows.len().saturating_sub(ROWS_CAP);
    rows.drain(..over);
    rows
}

fn decode_row(line: &str) -> Option<LastBound> {
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != 9 {
        return None;
    }
    let uuid = f[1].to_string();
    let uuid_ok = !uuid.is_empty() && uuid.len() <= 64 && uuid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
    let some = |v: &str| (!v.is_empty()).then(|| v.to_string());
    let config_dir = some(f[5]);
    if !uuid_ok || config_dir.as_deref().is_some_and(|d| !pty_op::shell_safe(d)) {
        return None;
    }
    Some(LastBound {
        sid: f[0].parse().ok()?,
        uuid,
        profile: f[2].parse().ok()?,
        shell: pidtree::ProcId { pid: f[3].parse().ok()?, start_us: f[4].parse().ok()? },
        config_dir,
        project_dir: some(f[6]).filter(|d| pty_op::quotable(d)),
        choice: CliChoice { model: some(f[7]).filter(|m| cli_model_ok(m)), effort: some(f[8]).filter(|e| cli_effort_ok(e)) },
    })
}

/// The row a binding makes; None for one that cannot be put back.
pub(super) fn row_of(sid: u64, meta: &BindMeta, choice: Option<&CliChoice>, shell: pidtree::ProcId) -> Option<LastBound> {
    if meta.uuid.is_empty() || meta.profile_num == PROFILE_UNKNOWN {
        return None;
    }
    Some(LastBound {
        sid,
        uuid: meta.uuid.clone(),
        profile: meta.profile_num,
        config_dir: meta.config_dir.clone(),
        project_dir: meta.project_dir.clone(),
        choice: choice.cloned().unwrap_or_default(),
        shell,
    })
}

/// Put `new` in place of the pane's row; true when anything changed.
/// A model already known for the same session is kept while the hook
/// has not recorded one yet.  Another pane's row for the same session
/// goes: the session moved here.
pub(super) fn upsert(rows: &mut Vec<LastBound>, mut new: LastBound) -> bool {
    let before = rows.len();
    rows.retain(|r| r.sid == new.sid || r.uuid != new.uuid);
    let moved = rows.len() != before;
    moved | match rows.iter().position(|r| r.sid == new.sid) {
        Some(i) => {
            if rows[i].uuid == new.uuid && new.choice.model.is_none() {
                new.choice = rows[i].choice.clone();
            }
            let changed = rows[i] != new;
            rows[i] = new;
            changed
        }
        None => {
            if rows.len() >= ROWS_CAP {
                rows.remove(0);
            }
            rows.push(new);
            true
        }
    }
}

impl ColdRow for LastBound {
    fn sid(&self) -> u64 {
        self.sid
    }
    fn shell(&self) -> pidtree::ProcId {
        self.shell
    }
}

#[cfg(test)]
#[path = "cold_record_tests.rs"]
mod tests;
