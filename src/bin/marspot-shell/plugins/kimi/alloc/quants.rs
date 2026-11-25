//! The kimi accounts as the shared core weighs them.

use crate::plugins::allocation::Quant;
use crate::plugins::claudecode::quota::Room;

use super::super::extra::Extras;

/// A month read without a renewal, or past it, renews this long from now.
const MONTH_S: i64 = 30 * 86_400;
/// A reading older than this comes after every fresh one.
const STALE_ACCOUNT_S: i64 = 30 * 60;

/// Every account the readings weigh, as the core sees it at `now`: what
/// is left of the window that binds, and when that window renews.  The
/// window kimi does not have is the week itself (`f = r`).
///
/// The week and the month are each a share of their own allowance, and
/// how the two allowances compare is not known; so with both read, the
/// one with less left binds and comes with its own renewal (a tie goes
/// to the week, which renews first).
pub(in crate::plugins::kimi) fn quants(rooms: &[(u8, Room)], extras: &Extras, now: i64) -> Vec<Quant> {
    let hours = |at: i64| (at - now) as f64 / 3600.0;
    let left = |used: f64| (1.0 - used).max(0.0);
    rooms
        .iter()
        .filter_map(|(n, room)| {
            let Room::Known { util_7d, reset_7d, measured_at, .. } = room else { return None };
            let week = util_7d.map(|w| (left(w), *reset_7d));
            let month = extras.get(n).and_then(|e| e.month).map(|(m, renews)| match renews {
                Some(at) if at <= now => (1.0, now + MONTH_S),
                Some(at) => (left(m), at),
                None => (left(m), now + MONTH_S),
            });
            let (r, renews) = match (week, month) {
                (Some(w), Some(m)) => if m.0 < w.0 { m } else { w },
                (Some(w), None) => w,
                // the month's share left stands in for a week's: an approximation
                (None, Some(m)) => m,
                (None, None) => return None,
            };
            let t = hours(renews);
            Some(Quant { id: *n, r, f: r, t, tf: t, open: !room.refused_for(None), stale: now - measured_at > STALE_ACCOUNT_S })
        })
        .collect()
}
