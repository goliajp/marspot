//! The one objective on the real panes of 2026-11-25 and 11-26, against
//! what the shell did and what its dry run would have done.

use super::waste::{PARAMS, Params};
use super::waste_world::{Outcome, Policy, fixture, pts, run};

fn line(name: &str, o: &Outcome) -> String {
    let acc: Vec<String> = o
        .accounts
        .iter()
        .map(|a| format!("P{} {}/{} reset {}/{} w7 {} wf {} stranded {} (at reset {})", a.id, pts(a.week), pts(a.fable), pts(1.0 - a.w7), pts(1.0 - a.wf / super::KAPPA_FABLE), pts(a.w7), pts(a.wf), pts(a.stranded()), pts(a.stranded_at_reset())))
        .collect();
    format!(
        "{name}: switches {} (resting {}) refusals {} unmet {} wasted 7d {} Fable {}\n  {}",
        o.switches,
        o.resting_moves,
        o.refusals,
        pts(o.unmet),
        pts(o.w7()),
        pts(o.wf()),
        acc.join("\n  ")
    )
}

fn worst_stranded(o: &Outcome) -> f64 {
    o.accounts.iter().map(|a| a.stranded()).fold(0.0, f64::max)
}

/// What the shell did reproduces the polls closely enough to stand for
/// them: every account's end reading within three points of the poll.
#[test]
fn alloc_replay_actual_follows_the_polls() {
    let fx = fixture();
    let o = run(&fx, Policy::Actual);
    for (a, (id, week, fable)) in o.accounts.iter().zip(&fx.end) {
        assert_eq!(a.id, *id);
        assert!((a.week - week).abs() < 0.03, "P{id} week {} against the poll's {week}", a.week);
        assert!((a.fable - fable).abs() < 0.03, "P{id} Fable {} against the poll's {fable}", a.fable);
    }
}

/// Under the old rules P4's week fills with about 39% of its Fable
/// unused and P3's Fable is barely touched; under the one objective no
/// account ends with its week full and 10% of Fable unused, with no
/// more switches and less waste.
#[test]
fn alloc_replay_waste_keeps_fable_room() {
    let fx = fixture();
    let actual = run(&fx, Policy::Actual);
    let dry = run(&fx, Policy::DryRun);
    let new = run(&fx, Policy::Waste(PARAMS));
    println!("{}\n{}\n{}", line("actual", &actual), line("dryrun", &dry), line("waste", &new));
    let p4 = actual.accounts.iter().find(|a| a.id == 4).unwrap();
    assert!(p4.stranded() > 0.3, "the old rules strand P4's Fable: {p4:?}");
    assert!(worst_stranded(&new) < 0.10, "{}", line("waste", &new));
    assert!(new.switches <= actual.switches, "{} switches against {}", new.switches, actual.switches);
    assert!(new.w7() + new.wf() < actual.w7() + actual.wf(), "waste {} against {}", new.w7() + new.wf(), actual.w7() + actual.wf());
}

/// The sweep the weights were chosen by: every combination that keeps
/// to today's switches and strands no Fable, by total waste.
#[test]
fn alloc_replay_waste_sweep() {
    let fx = fixture();
    let actual = run(&fx, Policy::Actual);
    let mut ok = Vec::new();
    for lambda in [0.5, 1.0, 2.0, 4.0] {
        for margin in [0.0, 0.005, 0.01, 0.02] {
            for per_day in [4, 8, 16] {
                for stay_h in [12.0, 24.0, 48.0] {
                    let p = Params { lambda, margin, per_day, stay_h };
                    let o = run(&fx, Policy::Waste(p));
                    let fits = o.switches <= actual.switches && worst_stranded(&o) < 0.10;
                    println!(
                        "lambda {lambda} margin {margin} per_day {per_day} stay {stay_h}: switches {} resting {} refusals {} unmet {} w7 {} wf {} stranded {} {}",
                        o.switches,
                        o.resting_moves,
                        o.refusals,
                        pts(o.unmet),
                        pts(o.w7()),
                        pts(o.wf()),
                        pts(worst_stranded(&o)),
                        if fits { "fits" } else { "-" }
                    );
                    if fits {
                        ok.push((p, o.w7() + o.wf()));
                    }
                }
            }
        }
    }
    let chosen = ok.iter().find(|(p, _)| *p == PARAMS).map(|(_, w)| *w);
    assert!(chosen.is_some(), "the weights in force fit");
}
