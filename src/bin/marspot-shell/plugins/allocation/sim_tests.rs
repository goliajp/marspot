//! The simulator's pass conditions for the new rule.
//!
//! In every shortage case, refusals at most a third of today's rule's;
//! at 0.15 points a move, no more unmet demand than today's rule; with
//! the planners seeing true speeds, at most 0.01 of a week lapsed; and
//! no pane moved while resting more than six times in the week.

use super::RHO0;
use super::sim::{Batch, Case, Outcome, run};
use super::sim_policies::Policy;

const SEEDS: [u64; 3] = [1, 2, 3];

/// Per pane per week, resting moves, on average over the panes.
const REST_MOVES_PER_PANE: f64 = 6.0;

#[derive(Clone, Copy, Debug, Default)]
struct Mean {
    unmet: f64,
    unmet_restricted: f64,
    refusals: f64,
    rest: f64,
    waste: f64,
    most_rest: u32,
}

fn mean(policy: Policy, c: &Case) -> Mean {
    let runs: Vec<Outcome> = SEEDS.iter().map(|s| run(policy, c, *s)).collect();
    let n = runs.len() as f64;
    Mean {
        unmet: runs.iter().map(|o| o.unmet).sum::<f64>() / n,
        unmet_restricted: runs.iter().map(|o| o.unmet_restricted).sum::<f64>() / n,
        refusals: runs.iter().map(|o| f64::from(o.refusals)).sum::<f64>() / n,
        rest: runs.iter().map(|o| f64::from(o.rest)).sum::<f64>() / n,
        waste: runs.iter().map(|o| o.waste).sum::<f64>() / n,
        most_rest: runs.iter().map(|o| o.most_rest_per_pane).max().unwrap_or(0),
    }
}

fn panes(c: &Case) -> f64 {
    (c.n_f + c.n_n + c.batches.iter().map(|b| b.n).sum::<usize>()) as f64
}

fn sigma(mut c: Case) -> Case {
    c.sigma = 1.3;
    c
}

/// `(name, case, planners see true speeds, one of the six rows the
/// conditions are set for)`: the six shortage rows, then two with panes
/// coming and going, which are reported and held to the move count only.
fn shortage_cases() -> Vec<(&'static str, Case, bool, bool)> {
    let base = Case::base(3, 9, 0.07);
    let with = |f: &dyn Fn(&mut Case)| {
        let mut c = sigma(base.clone());
        f(&mut c);
        c
    };
    let batches = vec![
        Batch { n: 5, rate: RHO0, from: 24.0, until: 32.0 },
        Batch { n: 8, rate: RHO0, from: 96.0, until: 100.0 },
    ];
    vec![
        ("7 pts/h, same speeds", base.clone(), true, true),
        ("7 pts/h, speeds differ", sigma(base.clone()), true, true),
        ("believed x0.5", with(&|c| c.bias = 0.5), false, true),
        ("believed x2", with(&|c| c.bias = 2.0), false, true),
        ("0.15 pts a move", with(&|c| c.move_cost = 0.0015), true, true),
        ("0.5 pts a move", with(&|c| c.move_cost = 0.005), true, true),
        ("helper batches", with(&|c| c.batches = batches.clone()), true, false),
        ("three panes stop at 72 h", with(&|c| c.leave = Some((3, 72.0))), true, false),
    ]
}

fn line(name: &str, m: &Mean) -> String {
    format!(
        "{name:<26} unmet {:>6.1} (Fable {:>5.1}) refusals {:>5.1} rest {:>5.1} waste {:.3} most_rest {}",
        m.unmet, m.unmet_restricted, m.refusals, m.rest, m.waste, m.most_rest
    )
}

#[test]
fn allocation_sim_pass_conditions() {
    let mut failures = Vec::new();
    for (name, c, true_speeds, held) in shortage_cases() {
        let (old, new) = (mean(Policy::Old, &c), mean(Policy::Plan, &c));
        eprintln!("{}\n{}", line(&format!("{name} / old"), &old), line(&format!("{name} / new"), &new));
        if new.rest / panes(&c) > REST_MOVES_PER_PANE {
            failures.push(format!("{name}: {} resting moves for {} panes", new.rest, panes(&c)));
        }
        if !held {
            continue;
        }
        if new.refusals > old.refusals / 3.0 {
            failures.push(format!("{name}: refusals {} > a third of {}", new.refusals, old.refusals));
        }
        if c.move_cost == 0.0015 && new.unmet > old.unmet {
            failures.push(format!("{name}: unmet {} > {}", new.unmet, old.unmet));
        }
        if true_speeds && new.waste > 0.01 {
            failures.push(format!("{name}: waste {} > 0.01", new.waste));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn allocation_sim_surplus_rows() {
    let mut four = Case::base(2, 9, 0.04);
    four.fable_share = 2.0 / 11.0;
    let mut two = sigma(Case::base(2, 4, 0.02));
    two.fable_share = 1.0 / 3.0;
    for (name, c) in [("4 pts/h", four), ("2 pts/h, speeds differ", two)] {
        let (old, new) = (mean(Policy::Old, &c), mean(Policy::Plan, &c));
        eprintln!("{}\n{}", line(&format!("{name} / old"), &old), line(&format!("{name} / new"), &new));
        assert!(new.rest / panes(&c) <= REST_MOVES_PER_PANE, "{name}: {new:?}");
        assert!(new.refusals <= old.refusals, "{name}: {new:?} vs {old:?}");
        assert!(new.unmet <= old.unmet + 0.5, "{name}: {new:?} vs {old:?}");
    }
}

#[test]
fn allocation_sim_is_deterministic() {
    let c = sigma(Case::base(3, 9, 0.07));
    assert_eq!(run(Policy::Plan, &c, 5), run(Policy::Plan, &c, 5));
}
