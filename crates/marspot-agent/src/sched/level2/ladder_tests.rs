//! Each step's entry and rollback condition on synthetic event streams,
//! one stream that passes and one that does not.

use super::super::exec_gate::RESUME_S;
use super::*;

const H: i64 = 3600;

fn ok(sid: u64, at: i64) -> Done {
    Done { sid, at, resumed_at: Some(at + 30), turn_running: false, live_unnoticed: false }
}

/// A ladder walked to `to` on passing streams, at `t` seconds.
fn walked(to: Step) -> Ladder {
    let mut l = Ladder::default();
    let mut t = 0;
    while l.step < to {
        t += 60;
        match l.step {
            Step::Shadow => {
                l.routed(false, t);
            }
            Step::A => {
                l.tick(0, t);
            }
            Step::B => {
                l.dry_record(true, true);
                l.tick(1, t);
            }
            Step::C => {
                l.switched(t);
            }
            Step::D => {
                l.tick(0, t + 25 * H);
            }
            Step::E => unreachable!(),
        }
    }
    assert_eq!(l.step, to);
    l
}

#[test]
fn sched_ladder_a_needs_twenty_placements_in_a_row_with_none_wrong() {
    let mut l = Ladder::default();
    for k in 0..19 {
        assert_eq!(l.routed(false, k), None);
    }
    // one wrong placement starts the count again
    assert_eq!(l.routed(true, 19), None);
    for k in 0..19 {
        assert_eq!(l.routed(false, 20 + k), None);
    }
    assert_eq!(l.step, Step::Shadow);
    assert_eq!(l.routed(false, 40), Some(Change { from: Step::Shadow, to: Step::A, why: None }));
    assert!(l.routes_live() && l.executes().is_none() && !l.dry_run());
    assert_eq!((l.a_at, l.route_bad), (Some(40), 1));
}

#[test]
fn sched_ladder_a_rolls_back_on_two_placed_panes_refused_within_the_hour() {
    let mut l = walked(Step::A);
    let a = l.a_at.unwrap();
    // refused later than an hour after placement: not counted
    assert_eq!(l.placed_refused(a, a + 2 * H), None);
    assert_eq!(l.placed_refused(a + 10, a + 600), None);
    let c = l.placed_refused(a + 20, a + 900).unwrap();
    assert_eq!((c.from, c.to, c.why), (Step::A, Step::Shadow, Some(Why::PlacedRefused)));
    assert!(!l.routes_live());
    // no automatic way back
    for k in 0..40 {
        l.routed(false, a + 1000 + k);
    }
    assert_eq!(l.step, Step::Shadow);
    // after the first day refusals do not count
    let mut l = walked(Step::B);
    let a = l.a_at.unwrap();
    for k in 0..3 {
        assert_eq!(l.placed_refused(a + 25 * H + k, a + 25 * H + 60 + k), None);
    }
    assert_eq!(l.step, Step::B);
}

#[test]
fn sched_ladder_b_follows_a_and_c_needs_replayed_records_and_notices() {
    let mut l = walked(Step::A);
    assert_eq!(l.tick(0, 100).map(|c| c.to), Some(Step::B));
    assert!(l.dry_run());
    // ten records without three notices, and mismatches, are not enough
    for _ in 0..10 {
        l.dry_record(true, false);
        l.dry_record(false, true);
    }
    assert_eq!(l.tick(0, 200), None);
    assert_eq!((l.dry, l.dry_notices, l.dry_mismatch), (10, 0, 10));
    for _ in 0..2 {
        l.dry_record(true, true);
    }
    assert_eq!(l.tick(0, 300), None);
    l.dry_record(true, true);
    assert_eq!(l.tick(3, 400).map(|c| c.to), Some(Step::C));
    assert_eq!((l.executes(), l.refusals_before), (Some(Scope::Unattended), 3));
}

#[test]
fn sched_ladder_d_after_eight_clean_switches_and_e_after_a_day() {
    let mut l = walked(Step::C);
    for k in 0..7 {
        assert_eq!(l.switched(k), None);
        assert_eq!(l.check(&[ok(k as u64, k * 60)], 3 * H, &[], 0, k * 60 + 60), None);
    }
    assert_eq!(l.switched(10).map(|c| c.to), Some(Step::D));
    assert_eq!(l.executes(), Some(Scope::All));
    assert_eq!(l.tick(0, 10 + 23 * H), None, "a day at D first");
    assert_eq!(l.tick(0, 10 + 24 * H).map(|c| c.to), Some(Step::E));
    assert!(l.protects());
    l.waited_attended();
    assert!(l.attended_wait && l.protects(), "a rehearsal only records it");
}

#[test]
fn sched_ladder_each_rollback_at_c_d_and_e_goes_back_to_the_dry_run_for_good() {
    let stay = 3 * H;
    let now = 2 * H;
    let cases: [(Vec<Done>, Vec<(i64, bool)>, u32, Rollback); 5] = [
        (vec![Done { resumed_at: None, ..ok(1, now - RESUME_S - 1) }], vec![], 0, Rollback::NoResume),
        (vec![ok(1, 0), ok(1, H)], vec![], 0, Rollback::TwiceInStay),
        (vec![Done { live_unnoticed: true, ..ok(1, 0) }], vec![], 0, Rollback::Unsafe),
        (vec![ok(1, 0)], vec![], 2, Rollback::Refusals),
        (vec![ok(1, 0)], vec![(0, false)], 0, Rollback::Unanswered),
    ];
    for step in [Step::C, Step::D, Step::E] {
        for (done, notices, refusals, want) in &cases {
            let mut l = walked(step);
            // the passing stream first: nothing holds
            assert_eq!(l.check(&[ok(9, now - 60)], stay, &[(now - 60, true)], l.refusals_before, now), None);
            let c = l.check(done, stay, notices, *refusals, now).unwrap();
            assert_eq!((c.from, c.to, c.why), (step, Step::B, Some(Why::Exec(*want))), "{step:?} {want:?}");
            assert!(l.dry_run() && l.executes().is_none());
            for _ in 0..20 {
                l.dry_record(true, true);
            }
            assert_eq!(l.tick(0, now + 60), None, "{want:?} waits for a person");
            l.reset(now + 120);
            assert_eq!((l.step, l.rolled, l.dry), (Step::B, None, 0));
        }
    }
    // a turn running at the switch is unsafe too
    let mut l = walked(Step::C);
    let c = l.check(&[Done { turn_running: true, ..ok(1, 0) }], stay, &[], 0, 100).unwrap();
    assert_eq!(c.why, Some(Why::Exec(Rollback::Unsafe)));
    // before C nothing is checked
    let mut l = walked(Step::B);
    assert_eq!(l.check(&cases[0].0, stay, &[], 9, now), None);
}

#[test]
fn sched_ladder_survives_a_restart() {
    let mut l = walked(Step::C);
    l.switched(5);
    l.route_bad = 2;
    l.placed_refused.push(77);
    l.attended_wait = true;
    assert_eq!(Ladder::decode(&l.encode()), Some(l.clone()));
    let mut r = walked(Step::D);
    r.check(&[ok(1, 0), ok(1, 60)], 3 * H, &[], 0, 120);
    assert_eq!(Ladder::decode(&r.encode()), Some(r.clone()));
    assert_eq!(Ladder::decode(&Ladder::default().encode()), Some(Ladder::default()));
    assert_eq!(Ladder::decode("step=z\n"), None);
    assert_eq!(Ladder::decode(&l.encode().replace("dry=", "dri=")), None);
    assert!(l.words().starts_with("step=c "), "{}", l.words());
}
