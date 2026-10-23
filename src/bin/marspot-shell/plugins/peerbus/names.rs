//! Every agent session in a marspot pane is addressed by that pane's
//! name -- the one drawn on the pane (`spg`, `spg#2`) -- whether it is
//! a Codex session or a Claude one.  The names live in a table marspot
//! writes (`marspot_peer::panes`), read by the Codex MCP tools and the
//! Claude Code hooks; no session's own record is renamed.  The Codex
//! records are marspot's own, so they carry the pane's name too, and
//! the name a Codex session had stays among its former names.
//!
//! A pane running two agents (one started inside the other) would give
//! both the same name, so such a pane is left out of the table.

use std::collections::{HashMap, HashSet};

use marspot_peer::panes::{self, Pane, Table};
use marspot_peer::registry::{self, FormerName};
use marspot_term::{lx_info, lx_warn};

use super::worker::{now_ms, write_record, Worker};

/// What naming needs from one look at the panes.
pub(super) struct Panes {
    /// Pane id → its name.
    pub names: HashMap<u64, String>,
    /// Pane id → every process under its shell.
    pub pids: HashMap<u64, HashSet<i32>>,
}

impl Panes {
    pub fn look() -> Panes {
        let obs = crate::observe::shared().latest();
        let names = marspot::pane_name::assign(&crate::ShellApp::pane_refs_in(&obs)).into_iter().collect();
        let pids = obs.live().map(|s| (s.entry.id, s.descendants.iter().map(|p| p.row.id().pid).collect())).collect();
        Panes { names, pids }
    }

    fn pane_of(&self, pid: i32) -> Option<u64> {
        self.pids.iter().find(|(_, ps)| ps.contains(&pid)).map(|(sid, _)| *sid)
    }
}

/// Which Claude sessions go in the table: `(pane, pid)` pairs, and the
/// panes holding more than one agent.  Pure, for the tests.
pub(super) fn plan(claude_pids: &[i32], codex_panes: &[u64], pane_of: impl Fn(i32) -> Option<u64>) -> (Vec<(u64, i32)>, HashSet<u64>) {
    let claude: Vec<(u64, i32)> = claude_pids.iter().filter_map(|&pid| pane_of(pid).map(|sid| (sid, pid))).collect();
    let mut count: HashMap<u64, usize> = HashMap::new();
    for (sid, _) in &claude {
        *count.entry(*sid).or_default() += 1;
    }
    for sid in codex_panes {
        *count.entry(*sid).or_default() += 1;
    }
    let crowded: HashSet<u64> = count.into_iter().filter(|(_, n)| *n > 1).map(|(sid, _)| sid).collect();
    (claude.into_iter().filter(|(sid, _)| !crowded.contains(sid)).collect(), crowded)
}

impl Worker {
    /// Bring every name in line with its pane: the table, and the Codex
    /// records.
    pub(super) fn rename_pass(&mut self) {
        let look = Panes::look();
        let dirs = registry::default_dirs();
        let mut claude: Vec<(i32, Option<String>)> = Vec::new();
        for d in &dirs {
            for e in registry::list_live(d) {
                if !e.ours && !claude.iter().any(|(p, _)| *p == e.pid) {
                    claude.push((e.pid, e.socket));
                }
            }
        }
        let pids: Vec<i32> = claude.iter().map(|(p, _)| *p).collect();
        let codex: Vec<u64> = self.bindings.keys().copied().collect();
        let (named, crowded) = plan(&pids, &codex, |pid| look.pane_of(pid));
        for sid in &crowded {
            if self.crowded.insert(*sid) {
                lx_info!("peerbus.names.crowded", "a pane runs two agents; it is left out of the names", sid = *sid);
            }
        }
        self.crowded.retain(|sid| crowded.contains(sid));
        let now = now_ms();

        let mut table: Vec<Pane> = Vec::new();
        for (sid, pid) in named {
            let (Some(name), Some((_, Some(socket)))) = (look.names.get(&sid), claude.iter().find(|(p, _)| *p == pid)) else {
                continue;
            };
            table.push(Pane { name: name.clone(), pid, socket: socket.clone() });
        }
        for (sid, b) in self.bindings.iter_mut() {
            if crowded.contains(sid) {
                continue;
            }
            let Some(want) = look.names.get(sid) else { continue };
            table.push(Pane { name: want.clone(), pid: b.record.pid, socket: b.record.socket.clone() });
            if b.record.name == *want {
                continue;
            }
            let old = FormerName { name: b.record.name.clone(), until_ms: now, session_id: b.thread.clone() };
            registry::push_former(&mut b.record.former_names, old, want);
            lx_info!("peerbus.names.codex", "codex session renamed after its pane", sid = *sid, old = b.record.name.clone(), new = want.clone());
            b.record.name = want.clone();
            b.record.name_since_ms = now;
            write_record(&dirs, &b.record);
        }
        table.sort_by(|a, b| (&a.name, a.pid).cmp(&(&b.name, b.pid)));
        if table != self.pane_table {
            self.write_pane_table(&table);
        }
    }

    fn write_pane_table(&mut self, table: &[Pane]) {
        let me = std::process::id() as i32;
        let Some(proc_start) = marspot_peer::proc::lstart(me) else { return };
        let t = Table { owner: self.owner.clone(), pid: me, proc_start, panes: table.to_vec() };
        match panes::write(&t) {
            Ok(()) => {
                lx_info!("peerbus.names.table", "pane names written", panes = table.len());
                self.pane_table = table.to_vec();
            }
            Err(e) => lx_warn!("peerbus.names.write_failed", &e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_with_two_agents_renames_neither() {
        let pane_of = |pid: i32| match pid {
            10 | 11 => Some(1), // two claude sessions in pane 1
            20 => Some(2),      // claude in pane 2, which also runs codex
            30 => Some(3),
            _ => None, // not in any marspot pane
        };
        let (rename, crowded) = plan(&[10, 11, 20, 30, 99], &[2, 4], pane_of);
        assert_eq!(rename, vec![(3, 30)]);
        assert_eq!(crowded, HashSet::from([1, 2]));
    }
}
