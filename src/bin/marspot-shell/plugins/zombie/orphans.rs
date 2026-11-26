//! Processes that closed panes left running.  Each check records the
//! ones it can place in `<state>/zombie/orphans.tsv` (the format is
//! `marspot::zombie::orphans`), for the process monitor to list and a
//! person to end.  The watch never ends them: no session is left to
//! ask, and nobody would catch a wrong guess.
//!
//! A closed pane is named by what it was called while it ran, so the
//! watch keeps the names of the panes it sees, and each helper's parent
//! (a closed helper's processes are its parent's while the parent
//! runs), for a week, in `names.tsv`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use marspot::pidtree::{ProcId, ProcRow};
use marspot::zombie::check::{OutsideIn, Scene, Watch};
use marspot::zombie::orphans::{self, Orphan};
use marspot::zombie::probe::Probe;

use super::attrib::{Attributor, Known, Owner};
use crate::observe::Observation;
use crate::plugins::claudecode::pane_of_session;

/// Panes remembered at most, and for how long after they were last seen.
pub(super) const NAMES_MAX: usize = 256;
pub(super) const NAMES_FOR: u64 = 7 * 86_400;

struct Seen {
    name: String,
    parent: Option<u64>,
    last: u64,
}

/// The panes seen lately: their names, and the parent of each helper.
#[derive(Default)]
pub(super) struct Names {
    rows: HashMap<u64, Seen>,
}

impl Names {
    pub fn load(dir: &Path) -> Names {
        let body = std::fs::read_to_string(dir.join("names.tsv")).unwrap_or_default();
        let mut rows = HashMap::new();
        for l in body.lines() {
            let mut f = l.split('\t');
            let (Some(sid), Some(last), Some(parent), Some(name)) = (f.next(), f.next(), f.next(), f.next()) else { continue };
            let (Ok(sid), Ok(last)) = (sid.parse(), last.parse()) else { continue };
            rows.insert(sid, Seen { name: name.to_string(), parent: parent.parse().ok(), last });
        }
        Names { rows }
    }

    /// Note the panes running now, named `names`, and the helpers'
    /// parents; forget what was not seen for [`NAMES_FOR`].
    pub fn note(&mut self, names: impl IntoIterator<Item = (u64, String)>, parents: &[(u64, u64)], now: u64) {
        for (sid, name) in names {
            let e = self.rows.entry(sid).or_insert_with(|| Seen { name: String::new(), parent: None, last: now });
            (e.name, e.last) = (name, now);
        }
        for &(sid, parent) in parents {
            let e = self.rows.entry(sid).or_insert_with(|| Seen { name: String::new(), parent: None, last: now });
            (e.parent, e.last) = (Some(parent), now);
        }
        self.rows.retain(|_, s| now.saturating_sub(s.last) < NAMES_FOR);
        if self.rows.len() > NAMES_MAX {
            let mut by_age: Vec<(u64, u64)> = self.rows.iter().map(|(sid, s)| (s.last, *sid)).collect();
            by_age.sort_unstable();
            for (_, sid) in &by_age[..by_age.len() - NAMES_MAX] {
                self.rows.remove(sid);
            }
        }
    }

    pub fn name(&self, sid: u64) -> &str {
        self.rows.get(&sid).map_or("", |s| s.name.as_str())
    }

    pub fn parents(&self) -> HashMap<u64, u64> {
        self.rows.iter().filter_map(|(sid, s)| Some((*sid, s.parent?))).collect()
    }

    pub fn tsv(&self) -> String {
        let mut sids: Vec<&u64> = self.rows.keys().collect();
        sids.sort_unstable();
        let mut out = String::new();
        for sid in sids {
            let s = &self.rows[sid];
            let parent = s.parent.map_or("-".to_string(), |p| p.to_string());
            out.push_str(&format!("{sid}\t{}\t{parent}\t{}\n", s.last, orphans::field(&s.name, 128)));
        }
        out
    }
}

/// The processes outside the panes placed with a closed pane, each
/// the first of its line started by launchd or a codex app-server; and
/// the ones whose placement waits for a later check.
pub(super) fn closed<'a>(
    obs: &'a Observation,
    in_pane: &HashSet<i32>,
    known: &Known,
    attrib: &mut Attributor,
) -> (Vec<OutsideIn<'a>>, HashSet<ProcId>) {
    let outside = |r: &&ProcRow| r.pid > 1 && !in_pane.contains(&r.pid);
    let (mut out, mut later) = (Vec::new(), HashSet::new());
    for r in obs.table.rows().iter().filter(outside) {
        if r.comm.starts_with("marspot") || !attrib.is_root(obs, r) {
            continue;
        }
        let group: Vec<&ProcRow> = if r.comm == "tail" {
            obs.table.rows().iter().filter(outside).filter(|o| o.pgid == r.pgid && o.pid != r.pid).collect()
        } else {
            Vec::new()
        };
        // a tail of a system binary keeps its environment to itself; its pipe partner does not
        let mut placed = None;
        for m in std::iter::once(r).chain(group.iter().copied()) {
            match attrib.owner(obs, m) {
                Owner::Closed(sid) => placed = Some((sid, m)),
                Owner::Later => {
                    later.insert(r.id());
                }
                _ => {}
            }
            if placed.is_some() {
                break;
            }
        }
        let Some((sid, by)) = placed else { continue };
        let session = attrib.session(obs, by);
        let gone = session.as_deref().is_some_and(|s| pane_of_session(&known.claude, s).is_none());
        let why = format!("它继承的窗格令牌是已关闭的窗格 {sid}");
        out.push(OutsideIn { sid, row: r, group, session_gone: gone, why });
    }
    (out, later)
}

/// What the record says, for the processes in `found`: each with the
/// rules it meets now, the first time it was seen kept from `before`.
/// A process of `before` whose placement waits keeps its line while it
/// lives.
pub(super) fn record(
    before: &[Orphan],
    found: &[OutsideIn],
    later: &HashSet<ProcId>,
    scene: &Scene,
    watch: &mut Watch,
    probe: &mut dyn Probe,
    names: &Names,
) -> Vec<Orphan> {
    let hits = watch.hits(scene, probe);
    let first: HashMap<ProcId, u64> = before.iter().map(|o| (o.id, o.first_seen)).collect();
    let mut rows: Vec<Orphan> = found
        .iter()
        .map(|o| {
            let id = o.row.id();
            let key = format!("{}@{}", id.pid, id.start_us);
            let rules: Vec<&str> = hits.iter().filter(|h| h.key.id == key).map(|h| h.key.rule.name()).collect();
            let argv = probe.argv(o.row).map_or_else(|| o.row.comm.clone(), |a| a.join(" "));
            Orphan {
                first_seen: first.get(&id).copied().unwrap_or(scene.now),
                id,
                sid: o.sid,
                name: names.name(o.sid).to_string(),
                rules: rules.join(","),
                argv: orphans::field(&argv, orphans::ARGV_BYTES),
            }
        })
        .collect();
    let have: HashSet<ProcId> = rows.iter().map(|o| o.id).collect();
    let lives = |id: ProcId| scene.table.get(id.pid).is_some_and(|r| r.id() == id);
    rows.extend(before.iter().filter(|o| later.contains(&o.id) && !have.contains(&o.id) && lives(o.id)).cloned());
    rows
}

#[cfg(test)]
#[path = "orphans_tests.rs"]
mod tests;
