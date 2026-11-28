use std::fs;

use marspot_agent::sched::level2::Limits;
use marspot_agent::sched::level2::ladder::{Step, Why};

use super::super::allocation_adapter::events_dir;
use super::super::allocation_inputs::AccountIn;
use super::*;
use crate::plugins::allocation::Class;

#[path = "allocation_exec_rollback_tests.rs"]
mod rollback;

const NOW: i64 = 1_795_716_000;

fn root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-exec-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn snap(now: i64, panes: &[(u64, u8, bool)]) -> Snapshot {
    let account = |profile| AccountIn { profile, week_used: 0.5, fable_used: Some(0.5), fable_known: true, week_renews: now + 60 * 3600, fable_renews: now + 60 * 3600, open: true, measured_at: now - 60, supply: Vec::new() };
    let panes: Vec<PaneIn> = panes.iter().map(|(sid, profile, resting)| PaneIn { sid: *sid, profile: *profile, class: Class::Plain, marked_at: None, refused_at: None, resting: *resting, helper: false, out: false, transcript: None }).collect();
    Snapshot { now, feed_at: Some(now - 60), accounts: vec![account(3), account(7)], live: vec![441], panes }
}

fn watch_of(sid: u64) -> Watch {
    let w = Watched { open_at: NOW, dead_at: NOW + 4 * 3600, exempt: false, clean_per_h: 0.09, long_background: false, lost_work: 0.0, gain: 0.01, noticed_at: None };
    Watch { sid, from: 3, to: 7, w }
}

fn watch() -> Watch {
    watch_of(441)
}

/// A pane with long background work that a switch now would end.
fn busy_watch() -> Watch {
    let mut w = watch();
    w.w.long_background = true;
    w
}

fn hold(focus: bool, child: bool) -> Hold {
    Hold { child: Some(child), timer: Some(false), helpers: false, focus, claude_pid: 0 }
}

fn quiet(focus: bool) -> HashMap<u64, Hold> {
    HashMap::from([(441, hold(focus, false))])
}

fn working() -> HashMap<u64, Hold> {
    HashMap::from([(441, hold(false, true))])
}

fn at(step: Step) -> Ladder {
    Ladder { step, ..Ladder::default() }
}

/// An executor whose ladder may go as high as `top`, with `ladder`.
fn exec_at(r: &Path, ladder: Ladder, top: Step) -> Exec {
    let mut x = Exec::load(r, NOW);
    x.ladder = ladder;
    x.top = top;
    x
}

/// One scan under balanced with switching live.
fn scan(x: &mut Exec, a: &mut Allocation, s: &Snapshot, holds: &HashMap<u64, Hold>, w: &[Watch], log: &mut Vec<(&'static str, String)>) -> Vec<Order> {
    let sc = Scan { watches: w, lim: &Limits::BALANCED, unforeseen: 0, refused: &[], waits: &[], setting_live: true, reset: None };
    x.step(a, s, holds, &sc, log)
}

fn events(a: &Allocation, kind: &str) -> Vec<super::super::allocation_events::Event> {
    a.events.read_since(NOW - 10, NOW + 7 * 86_400).into_iter().filter(|e| e.kind == kind).collect()
}

#[test]
fn allocation_exec_shadow_and_a_do_nothing_and_the_dry_run_records_and_replays() {
    let r = root("dry");
    let mut a = Allocation::new(events_dir(&r));
    let mut x = exec_at(&r, Ladder::default(), LADDER_MAX);
    let mut log = Vec::new();
    let resting = snap(NOW, &[(441, 3, true)]);
    assert!(scan(&mut x, &mut a, &resting, &quiet(false), &[watch()], &mut log).is_empty());
    assert!(log.iter().any(|(t, l)| *t == "sched.gate" && l.starts_with("step=shadow ")), "{log:?}");
    // at A the table places, the executor neither records nor orders
    x.ladder = at(Step::A);
    x.top = Step::A;
    for t in [1, 600, 4 * 3600] {
        assert!(scan(&mut x, &mut a, &snap(NOW + t, &[(441, 3, true)]), &quiet(false), &[watch()], &mut log).is_empty(), "a issues no order");
    }
    assert!(a.events.read_since(NOW - 10, NOW + 5 * 3600).iter().all(|e| !e.kind.starts_with("would_")), "neither shadow nor a writes a record");
    x.ladder = at(Step::B);
    // ten seconds into the rest: a short boundary, since a clean rest
    // before the deadline is unlikely at 0.09 an hour
    for t in [1, 10, 20] {
        assert!(scan(&mut x, &mut a, &snap(NOW + t, &[(441, 3, true)]), &quiet(false), &[watch()], &mut log).is_empty(), "a dry run orders nothing");
    }
    let rec: Vec<_> = a.events.read_since(NOW - 10, NOW + 100).into_iter().filter(|e| e.kind.starts_with("would_")).collect();
    assert_eq!(rec.len(), 1, "one record per decision: {rec:?}");
    assert!(rec[0].kind == "would_switch" && rec[0].detail.starts_with("to=P7 decided=switch:short "), "{rec:?}");
    assert_eq!(exec::replay(&rec[0].detail), Some(true));
    assert_eq!((x.ladder.dry, x.ladder.dry_mismatch), (1, 0));
    // the ladder is kept across a restart
    let again = Exec::load(&r, NOW);
    assert_eq!(again.ladder, x.ladder);
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_counts_only_rehearsed_switches_and_stops_at_its_cap() {
    let r = root("count");
    let mut a = Allocation::new(events_dir(&r));
    let mut x = exec_at(&r, at(Step::B), LADDER_MAX);
    let mut log = Vec::new();
    // a pane at work with long background work: a drain notice, then
    // the switch at the first boundary with nothing live
    scan(&mut x, &mut a, &snap(NOW + 5, &[(441, 3, true)]), &working(), &[busy_watch()], &mut log);
    assert_eq!(events(&a, "would_notify").len(), 1);
    assert_eq!((x.ladder.dry, x.ladder.dry_notices), (0, 0), "a notice is not a switch");
    scan(&mut x, &mut a, &snap(NOW + 600, &[(441, 3, true)]), &quiet(false), &[busy_watch()], &mut log);
    let sw = events(&a, "would_switch");
    assert!(sw.len() == 1 && sw[0].detail.contains("decided=switch:drain"), "{sw:?}");
    assert_eq!((x.ladder.dry, x.ladder.dry_notices), (1, 1));
    // enough records do not open c in this build, live or not
    x.ladder.dry = 20;
    x.ladder.dry_notices = 5;
    scan(&mut x, &mut a, &snap(NOW + 700, &[]), &quiet(false), &[], &mut log);
    assert_eq!(x.ladder.step, Step::B);
    // a kept ladder above the cap loads at the cap
    x.ladder = at(Step::D);
    x.save(&mut log);
    let held = Exec::load(&r, NOW);
    assert_eq!(held.ladder.step, LADDER_MAX);
    assert!(held.start.iter().any(|(t, _)| *t == "sched.gate.capped"), "{:?}", held.start);
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_with_switching_not_live_neither_advances_nor_orders() {
    let r = root("off");
    let mut a = Allocation::new(events_dir(&r));
    let mut x = exec_at(&r, at(Step::A), Step::E);
    let mut log = Vec::new();
    fn off(w: &[Watch]) -> Scan<'_> {
        Scan { watches: w, lim: &Limits::BALANCED, unforeseen: 0, refused: &[], waits: &[], setting_live: false, reset: None }
    }
    x.step(&mut a, &snap(NOW, &[(441, 3, true)]), &quiet(false), &off(&[]), &mut log);
    assert_eq!(x.ladder.step, Step::A, "no advance while switching is off");
    x.ladder = at(Step::C);
    let o = x.step(&mut a, &snap(NOW + 4 * 3600, &[(441, 3, true)]), &quiet(false), &off(&[watch()]), &mut log);
    assert!(o.is_empty() && !x.live(), "no order and today's rule keeps its moves");
    let o = scan(&mut x, &mut a, &snap(NOW + 4 * 3600 + 10, &[(441, 3, true)]), &quiet(false), &[watch()], &mut log);
    assert_eq!(o.len(), 1);
    assert!(x.live());
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_c_switches_only_unattended_panes_and_four_a_day() {
    let r = root("scope");
    let mut a = Allocation::new(events_dir(&r));
    let mut x = exec_at(&r, at(Step::C), Step::E);
    let mut log = Vec::new();
    scan(&mut x, &mut a, &snap(NOW, &[(441, 3, true)]), &quiet(false), &[watch()], &mut log);
    // someone is at the pane: at C nothing is switched
    assert!(scan(&mut x, &mut a, &snap(NOW + 10, &[(441, 3, true)]), &quiet(true), &[watch()], &mut log).is_empty());
    // at D it would be, were the user not in it; at C it is once they left
    let o = scan(&mut x, &mut a, &snap(NOW + 20, &[(441, 3, true)]), &quiet(false), &[watch()], &mut log);
    assert_eq!(o, vec![Order { sid: 441, from: 3, to: 7, kind: Kind::Short, noticed: false }]);
    // five resting panes, four orders
    let mut y = exec_at(&r, at(Step::C), Step::E);
    let sids = [1u64, 2, 3, 4, 5];
    let panes: Vec<(u64, u8, bool)> = sids.iter().map(|s| (*s, 3, true)).collect();
    let holds: HashMap<u64, Hold> = sids.iter().map(|s| (*s, hold(false, false))).collect();
    let ws: Vec<Watch> = sids.iter().map(|s| watch_of(*s)).collect();
    scan(&mut y, &mut a, &snap(NOW + 60, &panes), &holds, &ws, &mut log);
    let o = scan(&mut y, &mut a, &snap(NOW + 130, &panes), &holds, &ws, &mut log);
    assert_eq!(o.len(), 4, "{o:?}");
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_a_notice_rehearsed_at_b_is_not_taken_for_one_sent_at_c() {
    let r = root("leftover");
    let mut a = Allocation::new(events_dir(&r));
    let mut x = exec_at(&r, Ladder { dry: 9, dry_notices: 3, ..at(Step::B) }, Step::E);
    let mut log = Vec::new();
    scan(&mut x, &mut a, &snap(NOW + 5, &[(441, 3, true)]), &working(), &[busy_watch()], &mut log);
    assert!(x.noticed.get(&441).is_some_and(|n| !n.sent), "rehearsed only");
    // a tenth replayed switch elsewhere opens c
    x.ladder.dry = 10;
    let o = scan(&mut x, &mut a, &snap(NOW + 600, &[(441, 3, true)]), &working(), &[busy_watch()], &mut log);
    assert_eq!(x.ladder.step, Step::C);
    assert!(o.is_empty(), "no switch on a notice never sent: {o:?}");
    assert!(x.noticed.get(&441).is_some_and(|n| n.sent), "the notice is sent now");
    assert_eq!(events(&a, "told").len(), 1);
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_an_unreadable_gate_holds_until_reset_and_strict_waits_are_rehearsed() {
    let r = root("gate");
    fs::create_dir_all(r.join("sched")).unwrap();
    fs::write(r.join("sched").join("gate.txt"), "step=z\n").unwrap();
    let mut x = Exec::load(&r, NOW);
    assert_eq!(x.ladder.rolled.map(|w| w.1), Some(Why::Unreadable));
    let mut a = Allocation::new(events_dir(&r));
    let mut log = Vec::new();
    let reset = |stamp| Scan { watches: &[], lim: &Limits::BALANCED, unforeseen: 0, refused: &[], waits: &[], setting_live: true, reset: stamp };
    x.step(&mut a, &snap(NOW, &[]), &quiet(false), &reset(Some(NOW - 5)), &mut log);
    assert_eq!((x.ladder.rolled, x.ladder.reset_at), (None, NOW - 5));
    assert!(log.iter().any(|(t, _)| *t == "sched.gate.reset"), "{log:?}");
    x.ladder.rolled = Some((Step::C, Why::Unreadable));
    x.step(&mut a, &snap(NOW + 1, &[]), &quiet(false), &reset(Some(NOW - 5)), &mut log);
    assert!(x.ladder.rolled.is_some(), "a stamp applies once");
    // at e under strict protection a wait on an attended pane is recorded
    let mut y = exec_at(&r, at(Step::E), Step::E);
    let waits = Scan { watches: &[], lim: &Limits::FULL, unforeseen: 0, refused: &[], waits: &[441], setting_live: true, reset: None };
    y.step(&mut a, &snap(NOW + 2, &[(441, 3, false)]), &quiet(true), &waits, &mut log);
    assert!(y.ladder.attended_wait);
    assert_eq!(events(&a, "would_wait").len(), 1);
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_notice_texts_name_both_accounts() {
    let n = say::notice_body(3, 7, Kind::Notice);
    assert!(n.contains("P3") && n.contains("P7") && n.contains("5 分钟"), "{n}");
    assert!(say::notice_body(3, 7, Kind::Drain).contains("不要启动新的"));
    assert!(say::after_switch(3, 7).contains("P7"));
    assert!(!say::carry_on_body().is_empty());
}
