use crate::sched::level2::tests::{acct, input, pane};
use crate::sched::level2::{Grid, Limits, guard, plan};

/// P3 of 2026-11-26: 50 points left, all of it Fable-usable; P8 with
/// other-model room.  Under balanced, with Fable short, an Opus pane
/// that leaves a spent account goes to P8, not P3; under steady P3 is a
/// target as before.
#[test]
fn sched_guard_keeps_other_models_off_what_only_fable_can_use() {
    let a = [acct(6, 0.002, 0.0, 100.0), acct(3, 0.5, 0.5, 30.0), acct(8, 0.98, 0.51, 140.0)];
    let p = [pane(441, 6, false, 0.0, 0.004)];
    let mut i = input(&a, &p);
    i.limits = Limits::BALANCED;
    let g = Grid::new(&i);
    assert!(!g.admits(&i, 0, 1), "P3 has nothing but Fable-usable quota");
    assert!(g.admits(&i, 0, 2));
    let plan = plan(&i, None);
    assert!(plan.switches.iter().all(|s| s.to != 3), "{:?}", plan.switches);
    // with Fable not short the reserve is open to every model
    i.pi = 0.0;
    assert!(Grid::new(&i).admits(&i, 0, 1));
    let mut s = input(&a, &p);
    s.limits = Limits::STEADY;
    assert!(Grid::new(&s).admits(&s, 0, 1), "steady only prices it");
}

/// 428: a Fable session whose agents use Opus about half the time goes
/// where both classes last it, and to a Fable-only account only when no
/// account holds both.
#[test]
fn sched_guard_sends_a_mixed_pane_where_both_classes_last() {
    let p = [pane(428, 6, true, 0.003, 0.003)];
    assert!(guard::mixed(&p[0]) && !guard::mixed(&pane(1, 6, true, 0.003, 0.0001)));
    let a = [acct(6, 0.002, 0.001, 100.0), acct(3, 0.5, 0.5, 30.0), acct(1, 0.7, 0.35, 140.0)];
    let mut i = input(&a, &p);
    i.limits = Limits::BALANCED;
    let g = Grid::new(&i);
    assert!(!g.admits(&i, 0, 1) && g.admits(&i, 0, 2));
    let only_fable = [acct(6, 0.002, 0.001, 100.0), acct(3, 0.5, 0.5, 30.0)];
    let mut j = input(&only_fable, &p);
    j.limits = Limits::BALANCED;
    assert!(Grid::new(&j).admits(&j, 0, 1), "no account holds both: treated as a Fable pane");
}
