//! One snapshot of the process table, indexed once.

use super::{ProcRow, list_all_procs};

/// Row indices sorted by pid and by ppid.  Two allocations per table,
/// no hashing; lookups are binary searches.
struct Index {
    by_pid: Vec<(i32, u32)>,
    by_ppid: Vec<(i32, u32)>,
}

impl Index {
    fn build(rows: &[ProcRow]) -> Index {
        let mut by_pid: Vec<(i32, u32)> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.pid, i as u32))
            .collect();
        let mut by_ppid: Vec<(i32, u32)> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.ppid, i as u32))
            .collect();
        by_pid.sort_unstable();
        by_ppid.sort_unstable();
        Index { by_pid, by_ppid }
    }

    fn get(&self, pid: i32) -> Option<usize> {
        let at = self.by_pid.partition_point(|&(p, _)| p < pid);
        match self.by_pid.get(at) {
            Some(&(p, i)) if p == pid => Some(i as usize),
            _ => None,
        }
    }

    fn children(&self, pid: i32) -> impl Iterator<Item = usize> + '_ {
        let from = self.by_ppid.partition_point(|&(p, _)| p < pid);
        self.by_ppid[from..]
            .iter()
            .take_while(move |&&(p, _)| p == pid)
            .map(|&(_, i)| i as usize)
    }

    /// Indices of every descendant of `root`, in discovery order, root
    /// excluded.  Each row is visited at most once, so a ppid cycle in a
    /// table torn by concurrent forks and exits terminates.
    fn descendants(&self, rows: &[ProcRow], root: i32) -> Vec<usize> {
        // launchd's subtree is the whole host, which is never what a caller means
        if root <= 1 {
            return Vec::new();
        }
        let mut seen = vec![false; rows.len()];
        if let Some(i) = self.get(root) {
            seen[i] = true;
        }
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(parent) = stack.pop() {
            for i in self.children(parent) {
                if !seen[i] {
                    seen[i] = true;
                    out.push(i);
                    stack.push(rows[i].pid);
                }
            }
        }
        out
    }
}

pub struct ProcTable {
    rows: Vec<ProcRow>,
    index: Index,
}

impl ProcTable {
    /// Every process on the host now, indexed.
    pub fn take() -> ProcTable {
        ProcTable::from_rows(list_all_procs())
    }

    pub fn from_rows(rows: Vec<ProcRow>) -> ProcTable {
        let index = Index::build(&rows);
        ProcTable { rows, index }
    }

    pub fn rows(&self) -> &[ProcRow] {
        &self.rows
    }

    pub fn get(&self, pid: i32) -> Option<&ProcRow> {
        self.index.get(pid).map(|i| &self.rows[i])
    }

    pub fn children(&self, pid: i32) -> impl Iterator<Item = &ProcRow> + '_ {
        self.index.children(pid).map(|i| &self.rows[i])
    }

    /// Every descendant of `root_pid`, root excluded.  Empty for
    /// `root_pid <= 1`.
    pub fn descendants(&self, root_pid: i32) -> Vec<&ProcRow> {
        self.index
            .descendants(&self.rows, root_pid)
            .into_iter()
            .map(|i| &self.rows[i])
            .collect()
    }
}

/// Every descendant of `root_pid` in `procs`, root excluded; empty for
/// `root_pid <= 1`.
///
/// Slow path: indexes the whole slice on every call.  A caller walking
/// several roots of one snapshot should build a `ProcTable` once.
pub fn descendants_of(root_pid: i32, procs: &[ProcRow]) -> Vec<ProcRow> {
    let index = Index::build(procs);
    index
        .descendants(procs, root_pid)
        .into_iter()
        .map(|i| procs[i].clone())
        .collect()
}

/// F3+1 — one node of a process tree.  Same data as `ProcRow` plus
/// children list (depth-first nesting).  Used by the L2 process-tree
/// panel renderer to indent rows by depth.
#[derive(Debug, Clone)]
pub struct ProcNode {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub children: Vec<ProcNode>,
}

/// F3+1 — build a nested tree rooted at `root_pid` from a pre-fetched
/// proc table.  Returns None if `root_pid` itself isn't in the table
/// (process gone since the snapshot).  Children are ordered by pid
/// (deterministic across refresh ticks so the UI doesn't reshuffle
/// rows under the cursor on a frame where nothing changed).
pub fn tree_rooted_at(root_pid: i32, procs: &[ProcRow]) -> Option<ProcNode> {
    let index = Index::build(procs);
    let root = index.get(root_pid)?;
    let mut seen = vec![false; procs.len()];
    seen[root] = true;
    Some(build_node(root, procs, &index, &mut seen))
}

fn build_node(at: usize, procs: &[ProcRow], index: &Index, seen: &mut [bool]) -> ProcNode {
    let row = &procs[at];
    let kids: Vec<usize> = index.children(row.pid).filter(|&i| !seen[i]).collect();
    for &i in &kids {
        seen[i] = true;
    }
    // by_ppid is sorted by (ppid, row index), not pid
    let mut children: Vec<ProcNode> = kids
        .into_iter()
        .map(|i| build_node(i, procs, index, seen))
        .collect();
    children.sort_by_key(|n| n.pid);
    ProcNode {
        pid: row.pid,
        ppid: row.ppid,
        comm: row.comm.clone(),
        children,
    }
}

/// F3+1 — flatten a nested tree to a depth-tagged sequence for UI
/// row rendering.  `(depth, &node)` pairs in pre-order so root is
/// first, then children left-to-right.
pub fn flatten_pre_order(root: &ProcNode) -> Vec<(usize, &ProcNode)> {
    let mut out: Vec<(usize, &ProcNode)> = Vec::new();
    fn walk<'a>(node: &'a ProcNode, depth: usize, out: &mut Vec<(usize, &'a ProcNode)>) {
        out.push((depth, node));
        for c in &node.children {
            walk(c, depth + 1, out);
        }
    }
    walk(root, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: i32, ppid: i32) -> ProcRow {
        ProcRow {
            pid,
            ppid,
            comm: format!("p{pid}"),
            start_unix: 0,
            start_us: 0,
            pgid: pid,
            tty_dev: 0,
            tty_fg_pgid: 0,
            status: libc::SSLEEP,
        }
    }

    fn pids(rows: &[&ProcRow]) -> Vec<i32> {
        let mut v: Vec<i32> = rows.iter().map(|r| r.pid).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn descendants_walks_the_index() {
        let t = ProcTable::from_rows(vec![
            row(10, 1),
            row(11, 10),
            row(12, 11),
            row(13, 10),
            row(20, 1),
        ]);
        assert_eq!(pids(&t.descendants(10)), vec![11, 12, 13]);
        assert_eq!(pids(&t.descendants(11)), vec![12]);
        assert!(t.descendants(12).is_empty());
        assert!(t.descendants(99).is_empty());
        assert_eq!(t.get(13).map(|r| r.ppid), Some(10));
        assert!(t.get(14).is_none());
        assert_eq!(pids(&t.children(10).collect::<Vec<_>>()), vec![11, 13]);
    }

    /// A table read while processes fork and exit can show a ppid loop
    /// (a parent's pid reused by its own descendant); the walk must end.
    #[test]
    fn descendants_terminates_on_a_ppid_cycle() {
        let t = ProcTable::from_rows(vec![row(10, 12), row(11, 10), row(12, 11), row(13, 13)]);
        assert_eq!(pids(&t.descendants(10)), vec![11, 12]);
        assert!(
            t.descendants(13).is_empty(),
            "a self-parented row is not its own child"
        );
        let slow: Vec<i32> = descendants_of(11, t.rows()).iter().map(|r| r.pid).collect();
        assert_eq!(slow.len(), 2);
        assert!(tree_rooted_at(10, t.rows()).is_some());
    }

    #[test]
    fn descendants_of_launchd_and_pid_zero_are_empty() {
        let t = ProcTable::from_rows(vec![row(1, 0), row(0, 0), row(50, 1), row(51, 50)]);
        assert!(t.descendants(1).is_empty());
        assert!(t.descendants(0).is_empty());
        assert!(t.descendants(-1).is_empty());
        assert!(descendants_of(0, t.rows()).is_empty());
        assert!(descendants_of(1, t.rows()).is_empty());
        assert_eq!(pids(&t.descendants(50)), vec![51]);
    }

    /// A fork burst while the table is read must not push existing
    /// processes out of it.
    #[test]
    fn a_table_taken_during_a_fork_burst_still_lists_us_and_our_parent() {
        let forker = std::thread::spawn(|| {
            let mut n = 0;
            for _ in 0..200 {
                if let Ok(mut c) = std::process::Command::new("/usr/bin/true").spawn() {
                    let _ = c.wait();
                    n += 1;
                }
            }
            n
        });
        let me = std::process::id() as i32;
        let parent = super::super::proc_row(me).expect("own row").ppid;
        let mut takes = 0;
        loop {
            let done = forker.is_finished();
            let t = ProcTable::take();
            takes += 1;
            assert!(
                t.get(me).is_some(),
                "own pid missing from a table of {}",
                t.rows().len()
            );
            assert!(
                t.get(parent).is_some(),
                "parent {parent} missing from a table of {}",
                t.rows().len()
            );
            if done {
                break;
            }
        }
        assert_eq!(
            forker.join().expect("forker thread"),
            200,
            "the burst did not spawn"
        );
        assert!(takes > 1, "no table was taken while the burst ran");
    }
}
