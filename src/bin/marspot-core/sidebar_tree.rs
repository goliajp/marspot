//! The rows a window's sidebar lists: each pane of the window, followed
//! by its children wherever they are shown or in no window, and a
//! child shown here whose parent is elsewhere under a grey row of that
//! parent.  A row is no slot of the window: a child of another window
//! or of none has one too.
//!
//! The rows are worked out again only when what they are made of
//! changes.  Each frame folds the windows' panes, the hidden panes and
//! the parents into a fingerprint, which allocates nothing, and keeps
//! the rows while it is the same.

use std::hash::{Hash, Hasher};

use marspot::render::SidebarTree;
use marspot_term::fast_hash::FxHasher;

use super::*;

/// Rows a sidebar lists at most: past it a pane is still reached from
/// its window and the helper panel.
pub(crate) const ROW_CAP: usize = 128;

/// What a row stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowPane {
    /// Slot `idx` of this window, a placeholder too.
    Slot(usize),
    /// A pane of another window, or of none.
    Other(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) pane: RowPane,
    pub(crate) tree: SidebarTree,
    /// In no cell of a window, or only in a minimized one.
    pub(crate) hollow: bool,
}

/// The rows of a window whose slots hold `here`, of all panes `all`
/// (shown or not), each pane's parent as `parent` says and on screen
/// as `shown` says.
pub(crate) fn build(
    here: &[Option<u64>],
    all: &[u64],
    parent: impl Fn(u64) -> Option<u64>,
    shown: impl Fn(u64) -> bool,
) -> Vec<Row> {
    let slot_of = |s: u64| here.iter().position(|h| *h == Some(s));
    let top_here = |s: u64| slot_of(s).is_some() && parent(s).is_none();
    let row = |s: u64, tree| Row { pane: slot_of(s).map_or(RowPane::Other(s), RowPane::Slot), tree, hollow: !shown(s) };
    let mut rows = Vec::new();
    let mut grey: Vec<u64> = Vec::new();
    for (idx, sid) in here.iter().enumerate() {
        if rows.len() >= ROW_CAP {
            break;
        }
        let Some(sid) = *sid else {
            rows.push(Row { pane: RowPane::Slot(idx), tree: SidebarTree::Top, hollow: false });
            continue;
        };
        match parent(sid) {
            // listed under its parent
            Some(p) if top_here(p) => {}
            Some(p) if grey.contains(&p) => {}
            Some(p) => {
                grey.push(p);
                rows.push(row(p, SidebarTree::Parent));
                let kids: Vec<u64> = here.iter().flatten().copied().filter(|c| parent(*c) == Some(p)).collect();
                push_children(&mut rows, kids.into_iter().map(|c| row(c, SidebarTree::Child)));
            }
            None => {
                rows.push(row(sid, SidebarTree::Top));
                let mut kids: Vec<u64> = all.iter().copied().filter(|c| *c != sid && parent(*c) == Some(sid)).collect();
                kids.sort_unstable();
                kids.dedup();
                push_children(&mut rows, kids.into_iter().map(|c| row(c, SidebarTree::Child)));
            }
        }
    }
    rows.truncate(ROW_CAP);
    rows
}

/// `kids` after their parent's row, the last one marked so.
fn push_children(rows: &mut Vec<Row>, kids: impl Iterator<Item = Row>) {
    let from = rows.len();
    rows.extend(kids);
    if rows.len() > from
        && let Some(last) = rows.last_mut()
    {
        last.tree = SidebarTree::LastChild;
    }
}

/// Each window's rows, by window id, and what they were built from.
#[derive(Default)]
pub(crate) struct Trees(Vec<(u32, u64, Vec<Row>)>);

impl CoreApp {
    /// What window `wi`'s rows are made of, folded.
    fn sidebar_key(&self, wi: usize) -> u64 {
        let mut h = FxHasher::default();
        wi.hash(&mut h);
        self.pane_control.shadows.minimized.hash(&mut h);
        for w in &self.windows {
            (w.window_id, w.layout.cells.len(), w.panes.len()).hash(&mut h);
            for p in &w.panes {
                (p.shelld_session_id(), p.peeked()).hash(&mut h);
            }
        }
        for p in &self.shadows {
            p.shelld_session_id().hash(&mut h);
        }
        let parents = &self.pane_control.shadows.family.parents;
        // the same parents in any order fold the same
        let mixed = parents.iter().fold(0u64, |acc, kv| {
            let mut e = FxHasher::default();
            kv.hash(&mut e);
            acc ^ e.finish()
        });
        (mixed, parents.len()).hash(&mut h);
        h.finish()
    }

    /// Whether `sid` is in a cell of some window, or viewed in one.
    pub(crate) fn on_screen(&self, sid: u64) -> bool {
        self.windows.iter().any(|w| Self::in_cells(w, sid))
    }

    /// As [`Self::on_screen`], counting only windows not minimized: what
    /// the sidebar's solid dot means.
    pub(crate) fn on_visible_screen(&self, sid: u64) -> bool {
        let shown = |w: &&WindowState| !self.pane_control.shadows.minimized.contains(&w.window_id);
        self.windows.iter().filter(shown).any(|w| Self::in_cells(w, sid))
    }

    fn in_cells(w: &WindowState, sid: u64) -> bool {
        let cells = w.layout.cells.len();
        w.panes.iter().take(cells).any(|p| p.shelld_session_id() == Some(sid) || p.peeked() == Some(sid))
    }

    /// Window `wi`'s rows, worked out again if what they are made of
    /// changed.
    pub(crate) fn refresh_sidebar_tree(&mut self, wi: usize) {
        let key = self.sidebar_key(wi);
        let id = win!(self, wi).window_id;
        if self.pane_control.shadows.trees.0.iter().any(|t| t.0 == id && t.1 == key) {
            return;
        }
        let here: Vec<Option<u64>> = win!(self, wi).panes.iter().map(Pane::shelld_session_id).collect();
        let all: Vec<u64> = self.windows.iter().flat_map(|w| w.panes.iter()).chain(&self.shadows).filter_map(Pane::shelld_session_id).collect();
        let parents = &self.pane_control.shadows.family.parents;
        let rows = build(&here, &all, |s| parents.get(&s).copied(), |s| self.on_visible_screen(s));
        let ids: Vec<u32> = self.windows.iter().map(|w| w.window_id).collect();
        let trees = &mut self.pane_control.shadows.trees.0;
        trees.retain(|t| t.0 != id && ids.contains(&t.0));
        trees.push((id, key, rows));
    }

    /// Window `wi`'s rows as last worked out.
    pub(crate) fn sidebar_rows(&self, wi: usize) -> &[Row] {
        let id = win!(self, wi).window_id;
        self.pane_control.shadows.trees.0.iter().find(|t| t.0 == id).map_or(&[], |t| t.2.as_slice())
    }

    /// Window `wi`'s rows up to date, and its layout laid out for as
    /// many rows as it lists.
    pub(crate) fn sidebar_fresh(&mut self, wi: usize) {
        self.refresh_sidebar_tree(wi);
        let n = self.sidebar_rows(wi).len();
        let l = &win!(self, wi).layout;
        if l.sidebar_w > 0.0 && n > 0 && l.close_session_rects.len() != n {
            self.rebuild_layout(wi);
        }
    }

    /// The row of window `wi`'s pane in use.
    pub(crate) fn focused_row(&self, wi: usize) -> usize {
        let focused = win!(self, wi).focused_idx;
        self.sidebar_rows(wi)
            .iter()
            .position(|r| r.pane == RowPane::Slot(focused) && r.tree != SidebarTree::Parent)
            .unwrap_or(usize::MAX)
    }

    /// Row `i` of window `wi`'s sidebar.
    pub(crate) fn sidebar_row(&self, wi: usize, i: usize) -> Option<Row> {
        self.sidebar_rows(wi).get(i).copied()
    }
}

#[cfg(test)]
#[path = "sidebar_tree_tests.rs"]
mod tests;
