use super::*;

const H: i64 = 3600;

fn watched() -> Watched {
    Watched { open_at: 0, dead_at: 4 * H, exempt: false, clean_per_h: 0.09, long_background: false, lost_work: 0.001, gain: 0.01, noticed_at: None }
}

fn at(t: i64) -> Boundary {
    Boundary { at: t, quiet_s: 600, ..Boundary::default() }
}

#[test]
fn sched_exec_takes_the_cheapest_kind_first() {
    let w = watched();
    let b = Limits::BALANCED;
    assert_eq!(decide(&w, &at(10), &b), Would::Switch(Kind::Clean));
    // a short boundary: a clean rest before the deadline is unlikely at
    // 0.09 an hour (the measured rate), so it is taken
    assert_eq!(decide(&w, &Boundary { quiet_s: 10, ..at(10) }, &b), Would::Switch(Kind::Short));
    let often = Watched { clean_per_h: 4.0, ..w };
    assert_eq!(decide(&often, &Boundary { quiet_s: 10, ..at(10) }, &b), Would::Wait, "a clean rest will come");
    // never inside a turn, never with the user there, never before the window
    for x in [Boundary { turn_running: true, ..at(10) }, Boundary { user: true, ..at(10) }] {
        assert_eq!(decide(&w, &x, &b), Would::Wait);
    }
    assert_eq!(decide(&Watched { open_at: 100, ..w }, &at(10), &b), Would::Wait);
}

#[test]
fn sched_exec_notices_a_working_pane_as_the_profile_allows() {
    let w = watched();
    let busy = |t| Boundary { live: true, ..at(t) };
    // the notice is timed so it and the wait end at the deadline, however
    // rarely the pane rests
    let lead = NOTICE_S + i64::from(Limits::BALANCED.notice_wait_min) * 60;
    for rate in [0.09, 12.0] {
        let p = Watched { clean_per_h: rate, ..w };
        assert_eq!(decide(&p, &busy(10), &Limits::BALANCED), Would::Wait, "not at the opening");
        assert_eq!(decide(&p, &busy(4 * H - lead - 1), &Limits::BALANCED), Would::Wait);
        assert_eq!(decide(&p, &busy(4 * H - lead), &Limits::BALANCED), Would::Notify(Kind::Notice));
    }
    // a long-running manager gets the drain notice, on the same timing
    let manager = Watched { long_background: true, ..w };
    assert_eq!(decide(&manager, &busy(10), &Limits::BALANCED), Would::Wait);
    assert_eq!(decide(&manager, &busy(4 * H - lead), &Limits::BALANCED), Would::Notify(Kind::Drain));
    // steady notices a working pane only to avert a refusal
    let steady = NOTICE_S + i64::from(Limits::STEADY.notice_wait_min) * 60;
    assert_eq!(decide(&manager, &busy(4 * H - steady), &Limits::STEADY), Would::Wait);
    assert_eq!(decide(&Watched { exempt: true, ..manager }, &busy(4 * H - steady), &Limits::STEADY), Would::Notify(Kind::Drain));
}

#[test]
fn sched_exec_after_a_notice_switches_at_a_free_boundary_or_cancels_over_dear_work() {
    let n = Watched { noticed_at: Some(1000), ..watched() };
    let b = Limits::BALANCED;
    assert_eq!(decide(&n, &Boundary { quiet_s: 1, ..at(1100) }, &b), Would::Switch(Kind::Notice));
    let busy = |t| Boundary { live: true, ..at(t) };
    assert_eq!(decide(&n, &busy(1100), &b), Would::Wait, "inside the notice's five minutes");
    // the first boundary after the five minutes: a pane with a process
    // always under its agent is switched there
    let over = 1000 + NOTICE_S;
    assert_eq!(decide(&n, &busy(over), &b), Would::Switch(Kind::Notice), "the work is worth less than the gain");
    assert_eq!(decide(&Watched { lost_work: 0.02, ..n }, &busy(over), &b), Would::Cancel);
    assert_eq!(Would::Switch(Kind::Drain).word(), "switch kind=drain");
}

#[test]
fn sched_exec_a_drain_notice_waits_for_the_background_work_until_the_wait_is_over() {
    let d = Watched { noticed_at: Some(1000), long_background: true, ..watched() };
    let b = Limits::BALANCED;
    let busy = |t| Boundary { live: true, ..at(t) };
    let end = 1000 + NOTICE_S + i64::from(b.notice_wait_min) * 60;
    assert_eq!(decide(&d, &busy(1000 + NOTICE_S), &b), Would::Wait, "not after only five minutes");
    assert_eq!(decide(&d, &Boundary { quiet_s: 1, ..at(1000 + NOTICE_S) }, &b), Would::Switch(Kind::Drain), "the work ended");
    assert_eq!(decide(&d, &busy(end - 1), &b), Would::Wait);
    assert_eq!(decide(&d, &busy(end), &b), Would::Switch(Kind::Drain));
    assert_eq!(decide(&Watched { lost_work: 0.02, ..d }, &busy(end), &b), Would::Cancel);
    assert!(end - 1000 < super::super::exec_gate::NOTICE_ANSWER_S, "answered before a rollback");
}

#[test]
fn sched_exec_steady_never_switches_over_live_background_work_after_a_notice() {
    // an exempt switch let steady notice; the work did not end
    let lim = Limits::STEADY;
    let busy = |t| Boundary { live: true, ..at(t) };
    let end = 1000 + NOTICE_S + i64::from(lim.notice_wait_min) * 60;
    for long_background in [false, true] {
        let n = Watched { noticed_at: Some(1000), exempt: true, long_background, lost_work: 0.0, gain: 1.0, ..watched() };
        for t in [1000 + NOTICE_S, end - 1, end, 4 * H] {
            let would = decide(&n, &busy(t), &lim);
            assert!(matches!(would, Would::Wait | Would::Cancel), "{would:?} at {t}");
        }
        assert_eq!(decide(&n, &busy(end), &lim), Would::Cancel, "answered inside the half hour");
        let kind = if long_background { Kind::Drain } else { Kind::Notice };
        assert_eq!(decide(&n, &Boundary { quiet_s: 1, ..at(end - 1) }, &lim), Would::Switch(kind), "the work ended");
    }
}

#[test]
fn sched_exec_a_record_replays_to_the_same_decision() {
    let w = Watched { lost_work: 0.1 + 0.2, gain: 1.0 / 3.0, noticed_at: Some(77), ..watched() };
    let b = Boundary { live: true, ..at(4 * H) };
    let (w2, b2) = parse(&words(&w, &b)).unwrap();
    assert_eq!((w2, b2), (w, b));
    assert_eq!(decide(&w2, &b2, &Limits::FULL), decide(&w, &b, &Limits::FULL));
    assert_eq!(parse("open=1"), None);
}

#[test]
fn sched_exec_replay_compares_with_the_decision_recorded() {
    let w = Watched { noticed_at: Some(77), ..watched() };
    let b = Boundary { live: true, quiet_s: 0, ..at(4 * H) };
    let lim = Limits::BALANCED;
    let would = decide(&w, &b, &lim);
    let rec = record(&w, &b, &lim, would);
    assert_eq!(replay(&format!("to=P7 {rec}")), Some(true));
    // a record whose decision is not what its fields decide fails
    assert_ne!(would, Would::Cancel);
    assert_eq!(replay(&record(&w, &b, &lim, Would::Cancel)), Some(false));
    // the limits travel with the record: steady does not notice a
    // working pane near its deadline, balanced does
    let fresh = Watched { noticed_at: None, ..w };
    let near = Boundary { at: fresh.dead_at - 60, ..b };
    let (s, bal) = (decide(&fresh, &near, &Limits::STEADY), decide(&fresh, &near, &Limits::BALANCED));
    assert_ne!(s, bal);
    assert_eq!(replay(&record(&fresh, &near, &Limits::STEADY, bal)), Some(false));
    assert_eq!(replay(&record(&fresh, &near, &Limits::STEADY, s)), Some(true));
    assert_eq!(replay("decided=switch:sideways open=1"), None);
}

#[test]
fn sched_exec_a_committed_switch_keeps_its_earliest_deadline() {
    let mut d = Deadlines::default();
    let w = |dead_at| Watched { dead_at, ..watched() };
    // a pass re-times the window later: the deadline held does not move
    assert_eq!(d.hold(7, 3, w(4 * H), 10).dead_at, 4 * H);
    d.passed();
    assert_eq!(d.hold(7, 3, w(5 * H), 600).dead_at, 4 * H);
    assert_eq!(d.hold(8, 3, w(5 * H), 600).dead_at, 5 * H, "another pane's is its own");
    d.passed();
    // an earlier one is taken; a window not yet open is left as planned
    assert_eq!(d.hold(7, 3, w(3 * H), 900).dead_at, 3 * H);
    assert_eq!(d.hold(8, 3, Watched { open_at: 2000, ..w(9 * H) }, 900).dead_at, 9 * H);
    d.passed();
    // a switch no pass handed over is forgotten, as is one to another account
    assert_eq!(d.hold(8, 3, w(6 * H), 1200).dead_at, 6 * H);
    assert_eq!(d.hold(7, 4, w(6 * H), 1200).dead_at, 6 * H);
}
