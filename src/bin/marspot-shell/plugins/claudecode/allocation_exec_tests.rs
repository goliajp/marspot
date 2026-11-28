use std::fs;

use marspot_agent::sched::level2::exec_gate::RESUME_S;
use marspot_agent::sched::level2::ladder::{Step, Why};
use marspot_agent::sched::level2::exec_gate::Rollback;

use super::super::allocation_adapter::events_dir;
use super::super::allocation_inputs::AccountIn;
use super::*;
use crate::plugins::allocation::Class;

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

fn watch() -> Watch {
    let w = Watched { open_at: NOW, dead_at: NOW + 4 * 3600, exempt: false, clean_per_h: 0.09, long_background: false, lost_work: 0.0, gain: 0.01, noticed_at: None };
    Watch { sid: 441, from: 3, to: 7, w }
}

fn quiet(focus: bool) -> HashMap<u64, Hold> {
    HashMap::from([(441, Hold { child: Some(false), timer: Some(false), helpers: false, focus, claude_pid: 0 })])
}

fn at(step: Step) -> Ladder {
    Ladder { step, ..Ladder::default() }
}

#[test]
fn allocation_exec_shadow_does_nothing_and_the_dry_run_records_and_replays() {
    let r = root("dry");
    let mut a = Allocation::new(events_dir(&r));
    let mut x = Exec::load(&r);
    let lim = Limits::BALANCED;
    let mut log = Vec::new();
    let resting = snap(NOW, &[(441, 3, true)]);
    assert!(x.step(&mut a, &resting, &quiet(false), &[watch()], &lim, 0, &[], &mut log).is_empty());
    assert!(log.iter().any(|(t, l)| *t == "sched.gate" && l.starts_with("step=shadow ")), "{log:?}");
    assert!(a.events.read_since(NOW - 10, NOW + 100).iter().all(|e| !e.kind.starts_with("would_")), "shadow writes no record");
    x.ladder = at(Step::B);
    // ten seconds into the rest: a short boundary, since a clean rest
    // before the deadline is unlikely at 0.09 an hour
    for t in [1, 10, 20] {
        assert!(x.step(&mut a, &snap(NOW + t, &[(441, 3, true)]), &quiet(false), &[watch()], &lim, 0, &[], &mut log).is_empty(), "a dry run orders nothing");
    }
    let rec: Vec<_> = a.events.read_since(NOW - 10, NOW + 100).into_iter().filter(|e| e.kind.starts_with("would_")).collect();
    assert_eq!(rec.len(), 1, "one record per decision: {rec:?}");
    assert!(rec[0].kind == "would_switch" && rec[0].detail.starts_with("to=P7 switch kind=short "), "{rec:?}");
    assert_eq!((x.ladder.dry, x.ladder.dry_mismatch), (1, 0));
    // the ladder is kept across a restart
    let again = Exec::load(&r);
    assert_eq!(again.ladder, x.ladder);
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_exec_live_orders_a_switch_counts_it_and_rolls_back_when_it_does_not_resume() {
    let r = root("live");
    let mut a = Allocation::new(events_dir(&r));
    let mut x = Exec::load(&r);
    x.ladder = at(Step::C);
    let lim = Limits::BALANCED;
    let mut log = Vec::new();
    x.step(&mut a, &snap(NOW, &[(441, 3, true)]), &quiet(false), &[watch()], &lim, 0, &[], &mut log);
    // someone is at the pane: at C nothing is switched
    assert!(x.step(&mut a, &snap(NOW + 10, &[(441, 3, true)]), &quiet(true), &[watch()], &lim, 0, &[], &mut log).is_empty());
    let o = x.step(&mut a, &snap(NOW + 20, &[(441, 3, true)]), &quiet(false), &[watch()], &lim, 0, &[], &mut log);
    assert_eq!(o, vec![Order { sid: 441, from: 3, to: 7, kind: Kind::Short }]);
    // the main thread restarted it and it came back on P7
    x.step(&mut a, &snap(NOW + 25, &[]), &quiet(false), &[], &lim, 0, &[], &mut log);
    x.step(&mut a, &snap(NOW + 60, &[(441, 7, true)]), &quiet(false), &[], &lim, 0, &[], &mut log);
    assert_eq!((x.ladder.step, x.ladder.done), (Step::C, 1));
    // a second switch that never comes back rolls the executor back
    let w2 = Watch { from: 7, to: 3, ..watch() };
    // it has rested for hours: a clean rest
    let o = x.step(&mut a, &snap(NOW + 4 * 3600, &[(441, 7, true)]), &quiet(false), &[w2], &lim, 0, &[], &mut log);
    assert_eq!(o, vec![Order { sid: 441, from: 7, to: 3, kind: Kind::Clean }]);
    assert!(x.step(&mut a, &snap(NOW + 4 * 3600 + 10, &[(441, 7, true)]), &quiet(false), &[w2], &lim, 0, &[], &mut log).is_empty(), "one order at a time");
    x.step(&mut a, &snap(NOW + 4 * 3600 + 15, &[]), &quiet(false), &[], &lim, 0, &[], &mut log);
    x.step(&mut a, &snap(NOW + 4 * 3600 + 16 + RESUME_S, &[]), &quiet(false), &[], &lim, 0, &[], &mut log);
    assert_eq!((x.ladder.step, x.ladder.rolled), (Step::B, Some((Step::C, Why::Exec(Rollback::NoResume)))));
    assert!(log.iter().any(|(t, l)| *t == "sched.exec.rollback" && l.starts_with("reason=no_resume from=c to=b")), "{log:?}");
    assert!(!x.live());
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
