//! What each pane last ran claude as, kept on disk so a pane whose
//! session process started afresh (the machine restarted, the app was
//! closed and reopened) can have its claude put back.
//!
//! A row is written when the scan binds a pane, and dropped when the
//! person leaves claude in that same shell: the shell's identity tells
//! "claude exited here" from "this shell is new and claude never ran
//! in it".  A new shell is owed its claude only for `WINDOW` after it
//! started and only while nobody has typed into the pane since.

use std::time::{Duration, Instant, SystemTime};

use super::*;

/// Most rows kept; the oldest goes.  One per pane, and the session
/// population is capped well below this.
pub(super) const ROWS_CAP: usize = 128;

/// How long a pane's own shell must be without claude before its row
/// goes.  A switch or a reclaim takes claude down for seconds, and a
/// machine shutting down kills claude a moment before the shell: the
/// row has to outlive both.
pub(super) const GONE_GRACE: Duration = Duration::from_secs(30);

/// How long after a pane's new shell started its claude is put back.
/// The restores of a restarted machine are started 4 s apart, at most
/// 32 of them, so the last starts about two minutes after the panes
/// came back; past five minutes the pane is the person's, who may come
/// back to it at any time and must not find a line typed under them.
pub(super) const WINDOW: Duration = Duration::from_secs(5 * 60);

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

/// When the process `id` started.
pub(super) fn started(id: pidtree::ProcId) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_micros(id.start_us)
}

/// Whether a pane whose shell is `shell` may still have a line typed
/// into it at `now`: the shell is younger than `WINDOW`, and the pane
/// has had no input since it started.
pub(super) fn restorable(shell: pidtree::ProcId, now: SystemTime, last_input: Option<SystemTime>) -> bool {
    let from = started(shell);
    now.duration_since(from).unwrap_or_default() < WINDOW && last_input.is_none_or(|t| t < from)
}

/// What a scan says about panes with no claude bound now.
pub(super) struct Unbound<'a> {
    pub(super) bound: &'a dyn Fn(u64) -> bool,
    /// The registry has an entry for the pane.  A closed pane's entry
    /// is removed; a session process that ended with the machine keeps
    /// it, so the pane is not taken for closed when its process is
    /// gone before this one.
    pub(super) listed: &'a dyn Fn(u64) -> bool,
    /// The pane's shell, when its session process runs.
    pub(super) shell: &'a dyn Fn(u64) -> Option<pidtree::ProcId>,
    pub(super) last_input: &'a dyn Fn(u64) -> Option<SystemTime>,
    pub(super) now: Instant,
    pub(super) wall: SystemTime,
}

/// Drop the rows of closed panes, of panes whose own shell has been
/// without claude for `GONE_GRACE`, and of new shells past `WINDOW` or
/// typed into; the sids dropped.
pub(super) fn forget(rows: &mut Vec<LastBound>, gone_since: &mut HashMap<u64, Instant>, u: &Unbound) -> Vec<u64> {
    let mut dropped = Vec::new();
    rows.retain(|r| {
        if (u.bound)(r.sid) {
            gone_since.remove(&r.sid);
            return true;
        }
        let keep = match (u.shell)(r.sid) {
            _ if !(u.listed)(r.sid) => false,
            // the session process is not back yet
            None => true,
            Some(sh) if sh == r.shell => {
                let since = *gone_since.entry(r.sid).or_insert(u.now);
                u.now.duration_since(since) < GONE_GRACE
            }
            Some(sh) => restorable(sh, u.wall, (u.last_input)(r.sid)),
        };
        if !keep {
            dropped.push(r.sid);
        }
        keep
    });
    gone_since.retain(|sid, _| rows.iter().any(|r| r.sid == *sid));
    dropped
}

/// The pane as the kernel has it now.
pub(super) struct PaneNow {
    pub(super) shell: pidtree::ProcId,
    /// The shell holds its terminal and no claude runs anywhere in it.
    pub(super) idle_shell: bool,
    pub(super) last_input: Option<SystemTime>,
}

/// Whether `pane` may be typed into at `now`: idle, young, untouched.
pub(super) fn owed_shell(p: &PaneNow, now: SystemTime) -> bool {
    p.idle_shell && restorable(p.shell, now, p.last_input)
}

/// Whether row `r` is owed a restore into `pane` at `now`: a new shell,
/// idle, young and untouched.
pub(super) fn owed(r: &LastBound, pane: Option<&PaneNow>, now: SystemTime) -> bool {
    pane.is_some_and(|p| p.shell != r.shell && owed_shell(p, now))
}

#[cfg(test)]
#[path = "cold_record_tests.rs"]
mod tests;
