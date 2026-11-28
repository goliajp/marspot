use super::*;

fn live() -> Gate {
    let mut g = Gate::default();
    for k in 0..DRY_RECORDS {
        g.dry_record(k < DRY_NOTICES);
    }
    assert_eq!(g.stage, Stage::Unattended);
    g
}

fn ok(sid: u64, at: i64) -> Done {
    Done { sid, at, resumed_at: Some(at + 30), turn_running: false, live_unnoticed: false }
}

#[test]
fn sched_exec_gate_dry_run_needs_its_records_and_notices() {
    let mut g = Gate::default();
    for _ in 0..20 {
        g.dry_record(false);
    }
    assert_eq!(g.stage, Stage::DryRun, "no notice among them");
    let mut g = live();
    let done: Vec<Done> = (0..UNATTENDED_DONE as u64).map(|k| ok(k, 1000 * k as i64)).collect();
    assert_eq!(g.check(&done, 3 * 3600, &[], 0, 10_000), None);
    assert_eq!(g.stage, Stage::All);
}

#[test]
fn sched_exec_gate_each_rollback_condition_goes_back_to_the_dry_run() {
    let stay = 3 * 3600;
    let cases: [(Vec<Done>, Vec<(i64, bool)>, u32, Rollback); 5] = [
        (vec![Done { resumed_at: None, ..ok(1, 0) }], vec![], 0, Rollback::NoResume),
        (vec![ok(1, 0), ok(1, 3600)], vec![], 0, Rollback::TwiceInStay),
        (vec![Done { turn_running: true, ..ok(1, 0) }], vec![], 0, Rollback::Unsafe),
        (vec![ok(1, 0)], vec![], 3, Rollback::Refusals),
        (vec![ok(1, 0)], vec![(0, false)], 0, Rollback::Unanswered),
    ];
    for (done, notices, refusals, want) in cases {
        let mut g = live();
        g.refusals_before = 2;
        assert_eq!(g.check(&done, stay, &notices, refusals, 2 * 3600), Some(want), "{want:?}");
        assert_eq!(g.stage, Stage::DryRun, "{want:?}");
        // it stays in the dry run whatever records come
        for _ in 0..20 {
            g.dry_record(true);
        }
        assert_eq!(g.stage, Stage::DryRun, "{want:?} needs a person");
        g.resume();
        assert_eq!((g.stage, g.rolled_back, g.refusals_before), (Stage::DryRun, None, 2));
    }
    let mut g = live();
    assert_eq!(g.check(&[Done { live_unnoticed: true, ..ok(1, 0) }], stay, &[], 0, 100), Some(Rollback::Unsafe));
}
