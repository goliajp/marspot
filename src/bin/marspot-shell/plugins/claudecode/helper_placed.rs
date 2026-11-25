//! The tally of helpers placed on each account that what the choice
//! reads does not hold yet.

use crate::plugins::allocation::Class;

/// Helpers placed on each account that what the choice reads does not
/// hold yet.
#[derive(Debug, Default)]
pub(in crate::plugins) struct Placed {
    /// The stamp of the feed these counts are on top of.
    stamp: i64,
    /// That stamp, or when it was read where it was ahead of that.
    feed_at: i64,
    /// One entry per helper: its account, class, and when it was placed.
    entries: Vec<(u8, Class, i64)>,
    /// Whether a view of the panes has been settled against `entries`.
    viewed: bool,
    /// The helper panes the views settled so far showed: session, the
    /// account it was on, when a view last showed it.
    shown: Vec<(u64, u8, i64)>,
}

/// Helpers counted between two feeds at most; past it the oldest goes,
/// the one the next feed is likeliest to count already.
const PLACED_CAP: usize = 256;

/// Helper panes remembered as shown at most; past it the one shown
/// longest ago goes.
const SHOWN_CAP: usize = 256;

/// How long a helper's pane may take to show in a view after it was
/// placed: a container, the agent and its binding all come first.  A
/// pane no view has shown for this long is forgotten.
pub(super) const SHOWN_WITHIN_S: i64 = 15 * 60;

impl Placed {
    pub(super) const fn new() -> Self {
        Self { stamp: 0, feed_at: 0, entries: Vec::new(), viewed: false, shown: Vec::new() }
    }

    /// The feed being read at `now` was written at `at`.  A newer one
    /// already counts what was placed before it, so the tally starts
    /// again; an older one, read earlier and handed over late, says
    /// nothing new.  A stamp ahead of `now` counts as `now`, so the next
    /// feed is newer whatever the clock that wrote it said.  Where the
    /// clock was set back, the mark goes back to just before `now` and
    /// stays there: a feed is read some time after it is written.
    pub(in crate::plugins) fn feed(&mut self, at: i64, now: i64) {
        let written = at.min(now);
        self.feed_at = self.feed_at.min(now - 1);
        if at != self.stamp && written > self.feed_at {
            self.stamp = at;
            self.feed_at = written;
            self.entries.clear();
        }
    }

    /// A helper of `class` placed on `account` at `at`.
    pub(in crate::plugins) fn note(&mut self, account: u8, class: Class, at: i64) {
        if self.entries.len() >= PLACED_CAP {
            self.entries.remove(0);
        }
        self.entries.push((account, class, at));
    }

    /// Helpers placed on `account`, of either class.
    pub(in crate::plugins) fn count(&self, account: u8) -> u32 {
        self.entries.iter().filter(|(n, _, _)| *n == account).count() as u32
    }

    /// A view of the panes at `now` holds the helper panes `shown`
    /// (session, account, class).  Each one it shows that no view showed
    /// before, or on another account than before, is a helper placed
    /// since: it takes the oldest entry of its account and class, else
    /// of its account, out of the count, since its load is in the view
    /// now.  A pane a view leaves out (its binding is being redone) is
    /// remembered as it was until it has gone unseen too long.
    ///
    /// The first view settled only learns the helper panes it holds:
    /// nothing says which of them the entries made before it are.  Those
    /// entries stay counted until they expire, for at most
    /// `SHOWN_WITHIN_S` on top of their pane's load, which keeps the
    /// next helpers off a full account rather than on it.
    pub(in crate::plugins) fn settle(&mut self, shown: &[(u64, u8, Class)], now: i64) {
        let first = !self.viewed;
        self.viewed = true;
        let mut fresh: Vec<&(u64, u8, Class)> = Vec::new();
        for h in shown {
            match self.shown.iter_mut().find(|e| e.0 == h.0) {
                Some(e) => {
                    if e.1 != h.1 {
                        fresh.push(h);
                        e.1 = h.1;
                    }
                    e.2 = now;
                }
                None => {
                    if !first {
                        fresh.push(h);
                    }
                    self.shown.push((h.0, h.1, now));
                }
            }
        }
        fresh.sort_by_key(|h| h.0);
        for (_, n, c) in fresh {
            let k = self.entries.iter().position(|e| e.0 == *n && e.1 == *c).or_else(|| self.entries.iter().position(|e| e.0 == *n));
            if let Some(k) = k {
                self.entries.remove(k);
            }
        }
        self.shown.retain(|e| now - e.2 <= SHOWN_WITHIN_S);
        if self.shown.len() > SHOWN_CAP {
            self.shown.sort_by_key(|e| std::cmp::Reverse(e.2));
            self.shown.truncate(SHOWN_CAP);
        }
        self.expire(now);
    }

    /// Drop the entries no pane has taken this long after they were
    /// placed: helpers that did not start.
    pub(in crate::plugins) fn expire(&mut self, now: i64) {
        self.entries.retain(|e| now - e.2 < SHOWN_WITHIN_S);
    }

    /// The helpers placed, per account and class.
    pub(in crate::plugins) fn counts(&self) -> Vec<(u8, Class, u32)> {
        let mut out: Vec<(u8, Class, u32)> = Vec::new();
        for (n, c, _) in &self.entries {
            match out.iter_mut().find(|(m, d, _)| m == n && d == c) {
                Some((_, _, k)) => *k += 1,
                None => out.push((*n, *c, 1)),
            }
        }
        out
    }
}
