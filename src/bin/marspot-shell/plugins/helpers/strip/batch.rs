//! Which helpers a pane's strip lists.  The helpers under one top pane
//! are a batch from the first to start (or to go back to work) for as
//! long as the pane of one of them is open, idle or not: one that ended
//! its turn can still be talked to.  One whose pane closed stays a
//! moment, longer when it was closed at work, then goes on its own.
//!
//! Derived from the events alone, at the times they carry, so replaying
//! the log gives the batches the shell had before it started over.

use std::collections::{BTreeSet, HashMap};

use marspot::shell_proto::HELPER_STRIP_ROWS;

use super::super::table::{Event, Helper, Table};

/// Stopped helpers kept for one top pane; the oldest goes past it.
const ENDED_MAX: usize = HELPER_STRIP_ROWS;
/// A chain of parents that loops ends here.
const WALK_MAX: usize = 16;
/// How long one whose pane closed when it was not idle stays.
pub(super) const OVER_MS: u64 = 3_000;
/// How long one closed while at work stays.
pub(super) const STOPPED_MS: u64 = 30_000;

/// Whose shadow a pane is (0: no shadow); None: not known now.
pub(in crate::plugins::helpers) type Above<'a> = &'a dyn Fn(u64) -> Option<u64>;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Ended {
    pub root: u64,
    /// As it was when it stopped, its clock ended.
    pub helper: Helper,
    pub at_ms: u64,
    /// It had finished, and its pane was closed later.
    pub was_idle: bool,
}

impl Ended {
    /// Closed while at work, by whoever did.
    pub fn was_stopped(&self) -> bool {
        !self.was_idle && !self.helper.exited && self.helper.delivered_ms.is_some()
    }

    /// When it is listed no more.  One that was idle and could still be
    /// talked to goes with its pane, as it leaves the title's count.
    fn until_ms(&self) -> u64 {
        if self.was_stopped() {
            self.at_ms + STOPPED_MS
        } else if self.was_idle && !self.helper.exited {
            self.at_ms
        } else {
            self.helper.clock.ended_ms + OVER_MS
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct Batch {
    pub number: u64,
    /// Those of it whose panes are open.
    pub open: BTreeSet<u64>,
}

#[derive(Debug, Default)]
pub(in crate::plugins::helpers) struct Batches {
    ended: Vec<Ended>,
    /// By top pane.
    of: HashMap<u64, Batch>,
    /// The number last given out.
    last: u64,
}

/// A start that fails before its task goes in is not listed.
pub(super) fn listed(h: &Helper) -> bool {
    h.preparing || h.delivered_ms.is_some()
}

impl Batches {
    /// Apply `ev`, which happened at `at_ms`, to `table` and to these.
    pub(in crate::plugins::helpers) fn apply(&mut self, table: &mut Table, sid: u64, at_ms: u64, ev: &Event, above: Above) {
        self.expire(table, at_ms);
        match ev {
            Event::Stopped { exited } => self.stopped(table, sid, at_ms, *exited, above),
            Event::Rooted(root) => self.put(sid, *root, at_ms),
            Event::Started(_) | Event::Carried(_) => self.ended.retain(|e| e.helper.sid != sid),
            _ => {}
        }
        table.apply(sid, at_ms, ev);
        self.join(table, sid, at_ms, above);
    }

    /// `sid` is in its top pane's batch while it is at work.
    pub(super) fn join(&mut self, table: &Table, sid: u64, at_ms: u64, above: Above) {
        let Some(h) = table.map.get(&sid).filter(|h| listed(h) && h.clock.runs()) else { return };
        let root = h.root.unwrap_or_else(|| self.top(table, sid, above));
        if root != sid {
            self.add(root, sid, at_ms);
        }
    }

    fn add(&mut self, root: u64, sid: u64, at_ms: u64) {
        let batch = self.of.entry(root).or_insert_with(|| {
            self.last = at_ms.max(self.last + 1);
            Batch { number: self.last, open: BTreeSet::new() }
        });
        batch.open.insert(sid);
    }

    fn stopped(&mut self, table: &Table, sid: u64, at_ms: u64, exited: bool, above: Above) {
        let Some(h) = table.map.get(&sid).filter(|h| listed(h)) else { return };
        let root = h.root.unwrap_or_else(|| self.top(table, sid, above));
        // in no batch: in a window, or it ended with an earlier one
        if !self.of.get_mut(&root).is_some_and(|b| b.open.remove(&sid)) {
            return;
        }
        if self.ended.iter().filter(|e| e.root == root).count() >= ENDED_MAX
            && let Some(i) = self.ended.iter().position(|e| e.root == root)
        {
            self.ended.remove(i);
        }
        let mut helper = h.clone();
        let was_idle = !helper.clock.runs();
        helper.clock.end(at_ms);
        helper.exited |= exited;
        self.ended.push(Ended { root, helper, at_ms, was_idle });
    }

    /// When the batch of `root` is over; None while the pane of one of it
    /// is open.
    fn over_ms(&self, table: &Table, root: u64, batch: &Batch) -> Option<u64> {
        if batch.open.iter().any(|sid| table.map.contains_key(sid)) {
            return None;
        }
        Some(self.ended.iter().filter(|e| e.root == root).map(Ended::until_ms).fold(0, u64::max))
    }

    /// Forget what is over at `now_ms`; whether anything was.
    pub(in crate::plugins::helpers) fn expire(&mut self, table: &Table, now_ms: u64) -> bool {
        let before = self.ended.len() + self.of.len();
        self.ended.retain(|e| e.until_ms() > now_ms);
        let over: Vec<u64> = self.of.iter().filter(|(r, b)| self.over_ms(table, **r, b).is_some_and(|t| t <= now_ms)).map(|(r, _)| *r).collect();
        for root in &over {
            self.of.remove(root);
        }
        self.ended.retain(|e| self.of.contains_key(&e.root));
        before != self.ended.len() + self.of.len()
    }

    /// When the next of it is over (ms after the epoch).
    pub(in crate::plugins::helpers) fn next_ms(&self, table: &Table) -> Option<u64> {
        let rows = self.ended.iter().map(Ended::until_ms);
        let batches = self.of.iter().filter_map(|(r, b)| self.over_ms(table, *r, b));
        rows.chain(batches).min()
    }

    /// The pane at the top of `sid`'s chain of parents; of a chain with
    /// a pane that is gone, what was last known of it.
    pub(in crate::plugins::helpers) fn top(&self, table: &Table, sid: u64, above: Above) -> u64 {
        let recorded = |of: u64| {
            let ended = self.ended.iter().find(|e| e.helper.sid == of).map(|e| e.root);
            table.map.get(&of).and_then(|h| h.root).or(ended)
        };
        let mut at = sid;
        for _ in 0..WALK_MAX {
            let parent = match above(at) {
                Some(p) => p,
                None => match recorded(at).or_else(|| recorded(sid)) {
                    Some(root) => return root,
                    None => table.map.get(&at).map_or(0, |h| h.started.parent),
                },
            };
            if parent == 0 {
                break;
            }
            at = parent;
        }
        at
    }

    /// Out of any batch but that of `top`, and into that one if it was in
    /// another.
    fn put(&mut self, sid: u64, top: u64, at_ms: u64) {
        let mut left = false;
        for b in self.of.iter_mut().filter(|(root, _)| **root != top).map(|(_, b)| b) {
            left |= b.open.remove(&sid);
        }
        if left && top != sid {
            self.add(top, sid, at_ms);
        }
    }

    /// Move each helper to the batch of the pane now at its top; those
    /// whose top changed.
    pub(super) fn settle(&mut self, table: &mut Table, now_ms: u64, above: Above) -> Vec<(u64, u64)> {
        let tops: Vec<(u64, u64)> = table.map.values().map(|h| (h.sid, self.top(table, h.sid, above))).collect();
        let ended: Vec<u64> = self.ended.iter().map(|e| self.top(table, e.root, above)).collect();
        for (e, top) in self.ended.iter_mut().zip(ended) {
            e.root = top;
        }
        let mut moved = Vec::new();
        for (sid, top) in tops {
            self.put(sid, top, now_ms);
            if let Some(h) = table.map.get_mut(&sid).filter(|h| h.root != Some(top)) {
                h.root = Some(top);
                moved.push((sid, top));
            }
        }
        moved
    }

    pub(super) fn ended(&self) -> &[Ended] {
        &self.ended
    }

    pub(super) fn open(&self) -> &HashMap<u64, Batch> {
        &self.of
    }
}
