//! Whether the new rule may start moving panes: the simulator's pass
//! conditions, each worked out on every row it is set for.
//!
//! 1. In each of the six shortage rows, refusals at most a third of
//!    today's rule's.
//! 2. At the measured cost of a move (0.136 points), no more unmet
//!    demand than today's rule.
//! 3. In shortage, with the planners seeing true speeds, at most 0.01
//!    of a week lapsed.
//! 4. No pane moved while resting more than six times in the week, in
//!    any run.
//! 5. The Fable panes' unmet demand no more than under today's rule.
//!
//! Every row is the mean of three seeds, except condition 4, which
//! takes the most of any pane in any of them.

use super::RHO0;
use super::sim::{Batch, Case, Outcome, run};
use super::sim_policies::Policy;

pub(super) const SEEDS: [u64; 3] = [1, 2, 3];

/// The measured mean cost of a move, weeks: the first turn on the new
/// account writing the cache again.
pub(super) const MOVE_COST: f64 = 0.00136;

/// Resting moves one pane may make in a week.
pub(super) const REST_MOVES_PER_PANE: u32 = 6;

pub(super) const CONDITIONS: usize = 5;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Mean {
    pub unmet: f64,
    pub unmet_restricted: f64,
    pub refusals: f64,
    pub rest: f64,
    pub waste: f64,
    pub most_rest: u32,
    pub restricted_eaten: f64,
    pub restricted_lapsed: f64,
}

pub(super) fn mean(policy: Policy, c: &Case) -> Mean {
    let runs: Vec<Outcome> = SEEDS.iter().map(|s| run(policy, c, *s)).collect();
    let n = runs.len() as f64;
    Mean {
        unmet: runs.iter().map(|o| o.unmet).sum::<f64>() / n,
        unmet_restricted: runs.iter().map(|o| o.unmet_restricted).sum::<f64>() / n,
        refusals: runs.iter().map(|o| f64::from(o.refusals)).sum::<f64>() / n,
        rest: runs.iter().map(|o| f64::from(o.rest)).sum::<f64>() / n,
        waste: runs.iter().map(|o| o.waste).sum::<f64>() / n,
        most_rest: runs.iter().map(|o| o.most_rest_per_pane).max().unwrap_or(0),
        restricted_eaten: runs.iter().map(|o| o.restricted_eaten).sum::<f64>() / n,
        restricted_lapsed: runs.iter().map(|o| o.restricted_lapsed).sum::<f64>() / n,
    }
}

/// One simulated situation and what the conditions ask of it.
#[derive(Clone, Debug)]
pub(super) struct Situation {
    pub name: &'static str,
    pub case: Case,
    /// One of the six shortage rows condition 1 is set for.
    pub six: bool,
    pub shortage: bool,
    /// The planners see the true speeds.
    pub true_speeds: bool,
}

pub(super) fn sigma(mut c: Case) -> Case {
    c.sigma = 1.3;
    c
}

pub(super) fn situations() -> Vec<Situation> {
    let base = Case::base(3, 9, 0.07);
    let with = |f: &dyn Fn(&mut Case)| {
        let mut c = sigma(base.clone());
        f(&mut c);
        c
    };
    let batches = vec![Batch { n: 5, rate: RHO0, from: 24.0, until: 32.0 }, Batch { n: 8, rate: RHO0, from: 96.0, until: 100.0 }];
    let mut four = Case::base(2, 9, 0.04);
    four.fable_share = 2.0 / 11.0;
    let mut two = sigma(Case::base(2, 4, 0.02));
    two.fable_share = 1.0 / 3.0;
    let s = |name, case, six, shortage, true_speeds| Situation { name, case, six, shortage, true_speeds };
    vec![
        s("7 pts/h, same speeds", base.clone(), true, true, true),
        s("7 pts/h, speeds differ", sigma(base.clone()), true, true, true),
        s("believed x0.5", with(&|c| c.bias = 0.5), true, true, false),
        s("believed x2", with(&|c| c.bias = 2.0), true, true, false),
        s("0.136 pts a move", with(&|c| c.move_cost = MOVE_COST), true, true, true),
        s("0.5 pts a move", with(&|c| c.move_cost = 0.005), true, true, true),
        s("helper batches", with(&|c| c.batches = batches.clone()), false, true, true),
        s("three panes stop at 72 h", with(&|c| c.leave = Some((3, 72.0))), false, true, true),
        // no reading so far has had them apart: P3 and P6 renew Fable
        // first, P4 after its week
        s("Fable renews apart", with(&|c| c.tf_shift = vec![(3, -60.0), (4, 60.0), (6, -40.0)]), false, true, true),
        s("4 pts/h", four, false, false, true),
        s("2 pts/h, speeds differ", two, false, false, true),
    ]
}

/// One row: both rules' numbers and each condition's verdict, `None`
/// where the condition is not set for this row.
#[derive(Clone, Debug)]
pub(super) struct GateRow {
    pub name: &'static str,
    pub old: Mean,
    pub new: Mean,
    pub holds: [Option<bool>; CONDITIONS],
}

/// The verdict: the new rule may move panes only when every condition
/// holds on every row it is set for.
#[derive(Clone, Debug)]
pub(super) struct Step4Gate {
    pub rows: Vec<GateRow>,
    pub open: bool,
}

pub(super) fn row(s: &Situation) -> GateRow {
    let (old, new) = (mean(Policy::Old, &s.case), mean(Policy::Plan, &s.case));
    let holds = [
        s.six.then_some(new.refusals <= old.refusals / 3.0),
        (s.case.move_cost == MOVE_COST).then_some(new.unmet <= old.unmet),
        (s.shortage && s.true_speeds).then_some(new.waste <= 0.01),
        Some(new.most_rest <= REST_MOVES_PER_PANE),
        Some(new.unmet_restricted <= old.unmet_restricted),
    ];
    GateRow { name: s.name, old, new, holds }
}

pub(super) fn step4_gate() -> Step4Gate {
    let rows: Vec<GateRow> = situations().iter().map(row).collect();
    let open = rows.iter().all(|r| r.holds.iter().all(|h| h.unwrap_or(true)));
    Step4Gate { rows, open }
}

fn mark(h: Option<bool>) -> &'static str {
    match h {
        None => "  -",
        Some(true) => " ok",
        Some(false) => " NO",
    }
}

pub(super) fn line(r: &GateRow) -> String {
    let m = |w: &str, x: &Mean| {
        format!(
            "{w} unmet {:>6.1} (Fable {:>5.1}) refusals {:>5.1} rest {:>5.1} waste {:.3} most_rest {:>2} Fable's eaten {:>5.1} lapsed {:>4.1}",
            x.unmet, x.unmet_restricted, x.refusals, x.rest, x.waste, x.most_rest, x.restricted_eaten, x.restricted_lapsed
        )
    };
    let marks: String = r.holds.iter().map(|h| mark(*h)).collect();
    format!("{:<26}{marks}\n    {}\n    {}", r.name, m("old", &r.old), m("new", &r.new))
}
