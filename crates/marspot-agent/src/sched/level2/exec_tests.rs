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
    // a pane that rests often: the five-minute notice is timed so it and
    // the wait end at the deadline, leaving the clean rest its chance
    let often = Watched { clean_per_h: 12.0, ..w };
    let lead = NOTICE_S + i64::from(Limits::BALANCED.notice_wait_min) * 60;
    assert_eq!(decide(&often, &busy(4 * H - lead - 1), &Limits::BALANCED), Would::Wait);
    assert_eq!(decide(&often, &busy(4 * H - lead), &Limits::BALANCED), Would::Notify(Kind::Notice));
    // at 0.09 clean rests an hour one is unlikely in four hours: the notice goes at once
    assert_eq!(decide(&w, &busy(10), &Limits::BALANCED), Would::Notify(Kind::Notice));
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
    assert_eq!(decide(&n, &busy(1100), &b), Would::Wait, "inside the notice's five minutes");
    // the first boundary after the five minutes, inside the wait for one:
    // a pane with a process always under its agent is switched there
    let over = 1000 + NOTICE_S;
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
