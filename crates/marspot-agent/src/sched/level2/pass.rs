//! One pass of the planner over a standing plan: a full search, or only
//! the standing structure re-timed and re-costed.

use super::standing::{Ended, Standing, Verdict};
use super::grid::Grid;
use super::{Input, Plan, evaluate, plan, search};

pub struct Pass {
    pub plan: Plan,
    /// What the full search did with the structure; `None` on a re-time.
    pub verdict: Option<Verdict>,
    pub ended: Ended,
}

/// The standing plan on `input`, with ended switches removed.  On a
/// re-time (`retime`), a switch whose window is not open keeps its gain
/// when it averts a refusal; a full search weighs the structure by every
/// gain worked out afresh.
fn timed(standing: &mut Standing, input: &Input, now: i64, retime: bool) -> (Option<Plan>, Ended) {
    let g = Grid::new(input);
    let kept: Vec<(u64, u8, f64, f64)> = if retime { standing.switches.iter().filter(|s| s.open_at > now).map(|s| (s.sid, s.order, s.gain, s.gain_eff)).collect() } else { Vec::new() };
    let first = standing.schedules(input, &g, now).map(|s| search::evaluate_on(input, &g, &s, &kept));
    let ended = standing.end(first.as_ref(), input, now);
    if ended.cancelled.is_empty() && ended.missed.is_empty() && first.is_some() {
        return (first, ended);
    }
    let again = standing.schedules(input, &g, now).map(|s| search::evaluate_on(input, &g, &s, &kept));
    (again, ended)
}

/// A full search from the standing plan; committed switches are kept
/// as they stand.
pub fn full(standing: &mut Standing, input: &Input, digest: u64, now: i64) -> Pass {
    let (timed, ended) = timed(standing, input, now, false);
    let mut cand = plan(input, timed.as_ref().map(|t| t.scheds.as_slice()));
    if let Some(t) = &timed {
        let committed: Vec<u64> = standing.committed(now).map(|s| s.sid).collect();
        let mut scheds = cand.scheds.clone();
        let mut forced = false;
        for (p, pane) in input.panes.iter().enumerate() {
            if committed.contains(&pane.sid) && scheds[p] != t.scheds[p] {
                scheds[p] = t.scheds[p];
                forced = true;
            }
        }
        if forced {
            let (mode, active, charge) = (cand.mode, cand.active, cand.charge);
            cand = evaluate(input, &scheds);
            (cand.mode, cand.active, cand.charge) = (mode, active, charge);
        }
    }
    let verdict = standing.weigh(&cand, timed.as_ref(), digest, now);
    let plan = match (&verdict, timed) {
        (Verdict::Kept { .. }, Some(t)) => t,
        _ => cand,
    };
    Pass { plan, verdict: Some(verdict), ended }
}

/// The standing structure re-timed; `None` when it no longer holds and
/// a full search is due.
pub fn retime(standing: &mut Standing, input: &Input, now: i64) -> Option<Pass> {
    let (timed, ended) = timed(standing, input, now, true);
    let plan = timed?;
    standing.retime(&plan, now);
    Some(Pass { plan, verdict: None, ended })
}
