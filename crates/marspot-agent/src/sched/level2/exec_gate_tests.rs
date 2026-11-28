use super::*;

fn ok(sid: u64, at: i64) -> Done {
    Done { sid, at, resumed_at: Some(at + 30), turn_running: false, live_unnoticed: false }
}

#[test]
fn sched_exec_gate_rollback_conditions_hold_only_when_broken() {
    let stay = 3 * 3600;
    assert_eq!(rollback(&[ok(1, 0), ok(1, stay)], stay, &[(0, true)], 2, 2, 7200), None);
    assert_eq!(rollback(&[Done { resumed_at: None, ..ok(1, 0) }], stay, &[], 0, 0, 60), None, "not late yet");
    assert_eq!(rollback(&[Done { resumed_at: Some(RESUME_S + 1), ..ok(1, 0) }], stay, &[], 0, 0, 7200), Some(Rollback::NoResume));
    assert_eq!(rollback(&[ok(1, 0)], stay, &[(0, false)], 0, 0, NOTICE_ANSWER_S), None, "still inside its half hour");
    for r in [Rollback::NoResume, Rollback::TwiceInStay, Rollback::Unsafe, Rollback::Refusals, Rollback::Unanswered] {
        assert_eq!(Rollback::of(r.word()), Some(r));
    }
}
