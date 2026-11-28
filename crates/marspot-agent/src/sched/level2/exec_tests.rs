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
    // the five-minute notice is timed so it and the wait end at the deadline
    let lead = NOTICE_S + i64::from(Limits::BALANCED.notice_wait_min) * 60;
    assert_eq!(decide(&w, &busy(4 * H - lead - 1), &Limits::BALANCED), Would::Wait);
    assert_eq!(decide(&w, &busy(4 * H - lead), &Limits::BALANCED), Would::Notify(Kind::Notice));
    // a long-running manager gets the drain notice at the opening
    assert_eq!(decide(&Watched { long_background: true, ..w }, &busy(10), &Limits::BALANCED), Would::Notify(Kind::Drain));
    // steady notices a working pane only to avert a refusal
    assert_eq!(decide(&Watched { long_background: true, ..w }, &busy(10), &Limits::STEADY), Would::Wait);
    assert_eq!(decide(&Watched { long_background: true, exempt: true, ..w }, &busy(10), &Limits::STEADY), Would::Notify(Kind::Drain));
}

#[test]
fn sched_exec_after_a_notice_switches_at_a_free_boundary_or_cancels_over_dear_work() {
    let n = Watched { noticed_at: Some(1000), ..watched() };
    let b = Limits::BALANCED;
    assert_eq!(decide(&n, &Boundary { quiet_s: 1, ..at(1100) }, &b), Would::Switch(Kind::Notice));
    let busy = |t| Boundary { live: true, ..at(t) };
    assert_eq!(decide(&n, &busy(1100), &b), Would::Wait);
    let over = 1000 + NOTICE_S + i64::from(b.notice_wait_min) * 60;
    assert_eq!(decide(&n, &busy(over), &b), Would::Switch(Kind::Notice), "the work is worth less than the gain");
    assert_eq!(decide(&Watched { lost_work: 0.02, ..n }, &busy(over), &b), Would::Cancel);
    assert_eq!(Would::Switch(Kind::Drain).word(), "switch kind=drain");
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
