//! Fitting the plan to the fleet's limits, and dropping switches that
//! no longer gain.

use super::moves;
use super::search::{Eval, NOISE, better, solve};
use super::tally::Stage;
use super::{Mode, Sched};

/// The plan that fits the fleet's day and the notices an hour.  The
/// charge per counted switch is the gain of the dearest pane left out:
/// each pane's switches are worth what the plan loses without them, the
/// panes of least gain per counted switch are left out until the plan
/// fits, and the search runs once more from there at that charge, so a
/// switch comes back only if it gains more than the charge.  With
/// separable gains this is the charge bisection would settle on.
pub(super) fn fit(e: &mut Eval, over: Vec<Sched>, mode: Mode) -> (Vec<Sched>, Mode) {
    let base = e.cost(&over).money;
    let mut gains: Vec<(f64, usize)> = Vec::new();
    for p in 0..over.len() {
        let counted = (0..over[p].n as usize).filter(|&h| !e.exempt(p, &over[p], h)).count();
        if counted == 0 {
            continue;
        }
        let mut t = over.clone();
        t[p] = Sched::STAY;
        gains.push(((e.cost(&t).money - base) / counted as f64, p));
    }
    e.mark(Stage::Fit);
    gains.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut trimmed = over;
    let mut charge = 0.0_f64;
    for (g, p) in gains {
        if e.fits(&trimmed) {
            break;
        }
        trimmed[p] = Sched::STAY;
        charge = charge.max(g);
    }
    e.charge = charge.max(0.0) + NOISE;
    let (s, m, _) = solve(e, &trimmed);
    if e.fits(&s) {
        return (s, m);
    }
    let t = moves::trim(e, trimmed);
    e.mark(Stage::Fit);
    (t, mode)
}

/// A pane whose switches the plan does no worse without stays: a chain
/// or a swap can leave a switch that no longer gains anything.
pub(super) fn prune(e: &mut Eval, best: &mut [Sched]) {
    let mut cur = e.cost(best);
    for p in 0..best.len() {
        if best[p].n == 0 {
            continue;
        }
        let was = best[p];
        best[p] = Sched::STAY;
        let c = e.cost(best);
        if better(&cur, &c) {
            best[p] = was;
        } else {
            cur = c;
        }
    }
}
