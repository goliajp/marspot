//! Which account a helper goes to: the one with the most left.
//!
//! A refused pane goes where quota is about to lapse (`best_profile`),
//! so that nothing is left unused at a renewal.  Helpers are started in
//! batches, and five of them sent to the account nearest its renewal
//! are refused together minutes later and all restart, each losing its
//! prompt cache.  So a helper goes where the most is left, and a batch
//! is spread: the feed lags by minutes, and a helper placed since the
//! feed was last written is not in its numbers yet.  Each such helper
//! is taken to share its account's room with the next one.

use std::sync::Mutex;

use super::super::heads_up::nearly_out;
use super::super::seen::Resets;
use super::{same_model, Move, Room, FIVE_HOUR_SPENT};

/// Helpers placed on each account since the feed was last written.
#[derive(Debug, Default)]
pub(in crate::plugins) struct Placed {
    /// The stamp of the feed these counts are on top of.
    stamp: i64,
    /// That stamp, or when it was read where it was ahead of that.
    feed_at: i64,
    /// One entry per account number, so at most 256.
    counts: Vec<(u8, u32)>,
}

impl Placed {
    const fn new() -> Self {
        Self { stamp: 0, feed_at: 0, counts: Vec::new() }
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
            self.counts.clear();
        }
    }

    pub(in crate::plugins) fn note(&mut self, account: u8) {
        match self.counts.iter_mut().find(|(n, _)| *n == account) {
            Some((_, c)) => *c = c.saturating_add(1),
            None => self.counts.push((account, 1)),
        }
    }

    pub(in crate::plugins) fn count(&self, account: u8) -> u32 {
        self.counts.iter().find(|(n, _)| *n == account).map_or(0, |(_, c)| *c)
    }
}

/// The agents whose accounts are chosen by their usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::plugins) enum Pool {
    Claude,
    Codex,
    Kimi,
}

/// One tally per pool, shared by the helpers' start and by a refused
/// helper's move, which both place helpers on accounts.
static PLACED: Mutex<[Placed; 3]> = Mutex::new([Placed::new(), Placed::new(), Placed::new()]);

/// Run `f` on the tally of `pool`.
pub(in crate::plugins) fn placed<R>(pool: Pool, f: impl FnOnce(&mut Placed) -> R) -> R {
    let mut all = PLACED.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut all[pool as usize])
}

/// The rank of an account among the candidates; lower goes first.
fn tier(room: &Room, model: Option<&str>) -> u8 {
    if room.refused_for(model) {
        4
    } else if nearly_out(room, model) {
        3
    } else if *room == Room::Unknown {
        2
    } else if !room.known_for(model) {
        1
    } else {
        0
    }
}

/// What is left on an account for one more helper there: its headroom
/// shared with the helpers placed since the feed was written.  The
/// week first, the five hours to break ties, as in `best_profile`.
fn share(room: &Room, model: Option<&str>, placed: u32) -> (f64, f64) {
    let (week, five) = room.headroom_for(model);
    let n = f64::from(placed) + 1.0;
    (week.unwrap_or(0.0) / n, five / n)
}

/// The account with the most room for one more helper on `model`, or
/// `None` when the best there is refuses it.
///
/// Unlike `best_profile` it does not leave out any account, the
/// caller's own included: a helper is a new session, and the account
/// its caller is on is as good a place as any other.
pub(in crate::plugins) fn roomiest(rooms: &[(u8, Room)], placed: &Placed, model: Option<&str>) -> Option<u8> {
    let best = rooms.iter().min_by(|a, b| {
        let (sa, sb) = (share(&a.1, model, placed.count(a.0)), share(&b.1, model, placed.count(b.0)));
        tier(&a.1, model)
            .cmp(&tier(&b.1, model))
            .then_with(|| sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal))
            // accounts the feed does not cover all share as (0, 0): without
            // this a whole batch would go to the lowest number among them
            .then_with(|| placed.count(a.0).cmp(&placed.count(b.0)))
            .then_with(|| a.0.cmp(&b.0))
    })?;
    (!best.1.refused_for(model)).then_some(best.0)
}

/// Where a refused helper goes instead of where `next_move` would send
/// it: the roomiest account other than the one that refused it, so
/// helpers refused one after another do not all land on the same one.
/// An account the feed does not cover (`Room::Unknown`) is never a
/// destination, as in `best_profile`: nothing says it would not refuse
/// the helper too.  A Kimi account is `Room::Known` even where its use
/// was not measured, and stays one.
/// The account chosen is counted as placed.
pub(in crate::plugins) fn for_helper(decision: Move, rooms: &[(u8, Room)], current: u8, model: Option<&str>, placed: &mut Placed) -> Move {
    let Move::To(_) = decision else { return decision };
    let others: Vec<(u8, Room)> = rooms.iter().filter(|(p, r)| *p != current && *r != Room::Unknown).cloned().collect();
    match roomiest(&others, placed, model) {
        Some(to) => {
            placed.note(to);
            Move::To(to)
        }
        None => Move::Hold("every account is out"),
    }
}

/// What is known about a pool's accounts at one moment.
#[derive(Debug, Clone, Default)]
pub(in crate::plugins) struct Accounts {
    pub rooms: Vec<(u8, Room)>,
    /// When each account's windows end; empty for an account the feed
    /// does not cover.
    pub resets: Vec<(u8, Resets)>,
    /// When the feed was written; 0 without one.
    pub feed_at: i64,
}

/// The Claude profiles `profiles` as the refused-pane sweep sees them:
/// the feed, with the refusals panes have met.  Reads files.
pub(in crate::plugins) fn claude_accounts(profiles: &[u8], now: i64) -> Accounts {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else { return Accounts::default() };
    let usage = marspot::cc_usage::read();
    let accounts = usage.as_ref().map_or(&[][..], |u| &u.accounts[..]);
    let email = |p: u8| super::profile_email(&home, p);
    let mut rooms = super::rooms_for(profiles, accounts, email, now);
    let resets = super::super::seen::resets_for(profiles, accounts, email);
    let refused = marspot::paths::state_root().join("plugins/claudecode/refused.tsv");
    if let Ok(text) = std::fs::read_to_string(refused) {
        super::super::seen::Seen::decode(&text).mark(&mut rooms, &resets, now);
    }
    Accounts { rooms, resets, feed_at: usage.map_or(0, |u| u.generated_at) }
}

/// [`for_helper`] on the shared tally of `pool`, as of the feed written
/// at `feed_at`.
pub(in crate::plugins) fn helper_move(pool: Pool, feed_at: i64, decision: Move, rooms: &[(u8, Room)], current: u8, model: Option<&str>) -> Move {
    placed(pool, |p| {
        p.feed(feed_at, super::super::rooms::unix_now());
        for_helper(decision, rooms, current, model, p)
    })
}

/// When a refused account can take a helper on `model` again, as far
/// as the feed says: the latest end among the windows that are spent.
/// `None` where no spent window has a known end, or the account only
/// needs logging in again.
pub(in crate::plugins) fn recovers_at(room: &Room, resets: &Resets, model: Option<&str>, now: i64) -> Option<i64> {
    let Room::Known { status, util_5h, util_7d, models, .. } = room else { return None };
    if status == "auth_error" {
        return None;
    }
    let mut ends = Vec::new();
    if util_7d.is_some_and(|u| u >= 1.0) {
        ends.push(resets.week);
    }
    if util_5h.is_some_and(|u| u >= FIVE_HOUR_SPENT) {
        ends.push(resets.five_hour);
    }
    if let Some(want) = model.map(str::to_ascii_lowercase) {
        let spent = models.iter().any(|(label, u)| same_model(&want, label) && u.is_some_and(|u| u >= 1.0));
        if spent {
            ends.extend(resets.models.iter().filter(|(label, _)| same_model(&want, label)).map(|(_, t)| *t));
        }
    }
    ends.into_iter().filter(|t| *t > now).max()
}

#[cfg(test)]
#[path = "roomiest_tests.rs"]
mod tests;
