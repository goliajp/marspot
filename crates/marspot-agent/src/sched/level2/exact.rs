//! Exact mode: with at most six active panes of at most four first
//! targets each, every joint choice among staying and each pane's
//! proposals (a switch at its first allowed moment or at the last
//! moment before its account empties, a return at a renewal, or both)
//! is simulated and the cheapest kept, at most 1024 of them: twice the
//! 512 one switch time per target would need, since each target can be
//! taken at two times.  Panes outside the active set keep their
//! schedules.  Two choices equal in money within `TIE` go by the order
//! among equals, pane by pane: staying first, then the earlier renewal.

use super::propose::{allowed, candidates, key};
use super::search::{Cost, Eval, better};
use super::{Sched, TIE};

pub const PANES: usize = 6;
pub const TARGETS: usize = 4;
pub const COMBOS: usize = 1024;

/// The options of each active pane, staying first, when exact mode applies.
pub(super) fn options(e: &Eval, active: &[usize]) -> Option<Vec<Vec<Sched>>> {
    if active.len() > PANES {
        return None;
    }
    let (input, g) = (e.input(), e.grid());
    let mut out = Vec::with_capacity(active.len());
    let mut combos = 1usize;
    for &p in active {
        let mut opts = vec![Sched::STAY];
        let mine = candidates(input, g, p);
        let mut firsts: Vec<u8> = mine.iter().map(|s| s.hops[0].to).collect();
        firsts.sort_unstable();
        firsts.dedup();
        if firsts.len() > TARGETS {
            return None;
        }
        opts.extend(mine.into_iter().filter(|s| allowed(input, g, &e.ctx.deadline, p, s)));
        combos *= opts.len();
        if combos > COMBOS {
            return None;
        }
        out.push(opts);
    }
    Some(out)
}

pub(super) fn run(e: &mut Eval, cur: &[Sched], active: &[usize]) -> Option<Vec<Sched>> {
    let opts = options(e, active)?;
    let mut s = cur.to_vec();
    let mut pick = vec![0usize; active.len()];
    let mut best: Option<(Cost, Vec<Sched>)> = None;
    loop {
        for (k, &p) in active.iter().enumerate() {
            s[p] = opts[k][pick[k]];
        }
        let c = e.cost(&s);
        let wins = best.as_ref().is_none_or(|b| {
            let tie = (c.money - b.0.money).abs() <= TIE && (c.eff - b.0.eff).abs() <= 1e-9;
            if tie { keys(e, &s, active) < keys(e, &b.1, active) } else { better(&c, &b.0) }
        });
        if wins {
            best = Some((c, s.clone()));
        }
        // the next combination, the last pane fastest
        let mut k = active.len();
        loop {
            if k == 0 {
                return best.map(|b| b.1);
            }
            k -= 1;
            pick[k] += 1;
            if pick[k] < opts[k].len() {
                break;
            }
            pick[k] = 0;
        }
    }
}

fn keys(e: &Eval, s: &[Sched], active: &[usize]) -> Vec<(u8, i64, u8)> {
    active.iter().map(|&p| key(e.input(), &s[p])).collect()
}
