//! Each rollback condition met by the executor itself, from the switches
//! it ordered and the starts the main thread reported, and the calls-off
//! that answer a notice so that no rollback follows.

use marspot_agent::sched::level2::exec_gate::{NOTICE_ANSWER_S, RESUME_S, Rollback};

use super::*;

/// The rollback the executor holds; a rolled-back executor is the dry
/// run again and orders nothing.
fn rolled(x: &Exec) -> Option<Rollback> {
    if x.ladder.rolled.is_some() {
        assert!(x.ladder.dry_run() && !x.live(), "{:?}", x.ladder);
    }
    match x.ladder.rolled {
        Some((Step::C, Why::Exec(r))) if x.ladder.step == Step::B => Some(r),
        _ => None,
    }
}

/// An executor at C that ordered 441 from P3 to P7 at `NOW + 20`.
fn ordered(name: &str) -> (PathBuf, Allocation, Exec, Vec<(&'static str, String)>) {
    let r = root(name);
    let mut a = Allocation::new(events_dir(&r));
    let mut x = exec_at(&r, at(Step::C), Step::E);
    let mut log = Vec::new();
    scan(&mut x, &mut a, &snap(NOW, &[(441, 3, true)]), &quiet(false), &[watch()], &mut log);
    let o = scan(&mut x, &mut a, &snap(NOW + 20, &[(441, 3, true)]), &quiet(false), &[watch()], &mut log);
    assert_eq!(o.len(), 1);
    (r, a, x, log)
}

#[test]
fn allocation_exec_rolls_back_when_a_switched_pane_does_not_come_back() {
    let (r, mut a, mut x, mut log) = ordered("noresume");
    started(441, NOW + 22, false, false);
    scan(&mut x, &mut a, &snap(NOW + 25, &[]), &quiet(false), &[], &mut log);
    assert_eq!(x.ladder.step, Step::C, "still inside its two minutes");
    scan(&mut x, &mut a, &snap(NOW + 23 + RESUME_S, &[]), &quiet(false), &[], &mut log);
    assert_eq!(rolled(&x), Some(Rollback::NoResume));
    assert!(log.iter().any(|(t, l)| *t == "sched.exec.rollback" && l.starts_with("reason=no_resume from=c to=b")), "{log:?}");
    assert!(!x.live());
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_rolls_back_on_a_second_switch_inside_the_stay() {
    let (r, mut a, mut x, mut log) = ordered("twice");
    started(441, NOW + 22, false, false);
    scan(&mut x, &mut a, &snap(NOW + 25, &[(441, 7, true)]), &quiet(false), &[], &mut log);
    assert_eq!((x.ladder.step, x.ladder.done), (Step::C, 1));
    let back = Watch { from: 7, to: 3, ..watch() };
    let o = scan(&mut x, &mut a, &snap(NOW + 3600, &[(441, 7, true)]), &quiet(false), &[back], &mut log);
    assert_eq!(o.len(), 1, "{o:?}");
    started(441, NOW + 3601, false, false);
    scan(&mut x, &mut a, &snap(NOW + 3610, &[(441, 3, true)]), &quiet(false), &[], &mut log);
    assert_eq!(rolled(&x), Some(Rollback::TwiceInStay));
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_rolls_back_when_the_start_found_a_turn_running() {
    let (r, mut a, mut x, mut log) = ordered("unsafe");
    // what the main thread read at the start decides, not the scan's
    started(441, NOW + 22, true, false);
    scan(&mut x, &mut a, &snap(NOW + 25, &[(441, 7, true)]), &quiet(false), &[], &mut log);
    assert_eq!(rolled(&x), Some(Rollback::Unsafe));
    // work under the agent with no notice sent is unsafe too
    let (r2, mut a2, mut y, mut log2) = ordered("unsafe2");
    started(441, NOW + 22, false, true);
    scan(&mut y, &mut a2, &snap(NOW + 25, &[(441, 7, true)]), &quiet(false), &[], &mut log2);
    assert_eq!(rolled(&y), Some(Rollback::Unsafe));
    let _ = fs::remove_dir_all(&r);
    let _ = fs::remove_dir_all(&r2);
}

#[test]
fn allocation_exec_rolls_back_on_more_unforeseen_refusals_than_before() {
    let r = root("refusals");
    let mut a = Allocation::new(events_dir(&r));
    let mut x = exec_at(&r, Ladder { refusals_before: 1, ..at(Step::C) }, Step::E);
    let mut log = Vec::new();
    let with = |n| Scan { watches: &[], lim: &Limits::BALANCED, unforeseen: n, refused: &[], waits: &[], setting_live: true, reset: None, full: 0 };
    x.step(&mut a, &snap(NOW, &[]), &quiet(false), &with(1), &mut log);
    assert_eq!(x.ladder.step, Step::C);
    x.step(&mut a, &snap(NOW + 1, &[]), &quiet(false), &with(2), &mut log);
    assert_eq!(rolled(&x), Some(Rollback::Refusals));
    let _ = fs::remove_dir_all(&r);
}

/// An executor at C that sent 441 a drain notice at `NOW + 5`.
fn noticed(name: &str) -> (PathBuf, Allocation, Exec, Vec<(&'static str, String)>) {
    let r = root(name);
    let mut a = Allocation::new(events_dir(&r));
    let mut x = exec_at(&r, at(Step::C), Step::E);
    let mut log = Vec::new();
    scan(&mut x, &mut a, &snap(NOW + 5, &[(441, 3, true)]), &working(), &[busy_watch()], &mut log);
    assert_eq!(events(&a, "told").len(), 1);
    (r, a, x, log)
}

#[test]
fn allocation_exec_rolls_back_on_a_notice_left_unanswered() {
    let (r, mut a, mut x, mut log) = noticed("unanswered");
    // the pane keeps working: no boundary, no switch, no word
    for t in [600, 1200, 5 + NOTICE_ANSWER_S - 1] {
        scan(&mut x, &mut a, &snap(NOW + t, &[(441, 3, false)]), &working(), &[busy_watch()], &mut log);
    }
    assert_eq!(x.ladder.step, Step::C);
    scan(&mut x, &mut a, &snap(NOW + 6 + NOTICE_ANSWER_S, &[(441, 3, false)]), &working(), &[busy_watch()], &mut log);
    assert_eq!(rolled(&x), Some(Rollback::Unanswered));
    assert!(log.iter().any(|(t, l)| *t == "sched.switch.cancelled" && l.contains("why=rolled_back")), "the session hears to carry on: {log:?}");
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_a_notice_called_off_at_c_is_answered_and_rolls_nothing_back() {
    // the user came to the pane after the notice
    let (r, mut a, mut x, mut log) = noticed("attended");
    let focus = HashMap::from([(441, hold(true, true))]);
    scan(&mut x, &mut a, &snap(NOW + 600, &[(441, 3, true)]), &focus, &[busy_watch()], &mut log);
    assert!(x.cancelled.contains(&441));
    assert!(events(&a, "switch").iter().any(|e| e.detail.contains("cancelled why=attended told=1")), "{:?}", events(&a, "switch"));
    scan(&mut x, &mut a, &snap(NOW + 6 + NOTICE_ANSWER_S, &[(441, 3, true)]), &focus, &[busy_watch()], &mut log);
    assert_eq!((x.ladder.step, x.ladder.rolled), (Step::C, None));
    // the main thread declined the order that followed a notice
    let (r2, mut a2, mut y, mut log2) = noticed("declined");
    let o = scan(&mut y, &mut a2, &snap(NOW + 600, &[(441, 3, true)]), &quiet(false), &[busy_watch()], &mut log2);
    assert_eq!(o, vec![Order { sid: 441, from: 3, to: 7, kind: Kind::Drain, noticed: true }]);
    scan(&mut y, &mut a2, &snap(NOW + 700, &[(441, 3, true)]), &quiet(false), &[], &mut log2);
    assert!(y.cancelled.contains(&441));
    assert!(log2.iter().any(|(t, l)| *t == "sched.switch.cancelled" && l.contains("why=declined")), "{log2:?}");
    scan(&mut y, &mut a2, &snap(NOW + 6 + NOTICE_ANSWER_S, &[(441, 3, true)]), &quiet(false), &[], &mut log2);
    assert_eq!((y.ladder.step, y.ladder.rolled), (Step::C, None));
    let _ = fs::remove_dir_all(&r);
    let _ = fs::remove_dir_all(&r2);
}
