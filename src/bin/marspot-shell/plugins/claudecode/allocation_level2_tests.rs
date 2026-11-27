use super::super::allocation_adapter::events_dir;
use super::super::allocation_inputs::{AccountIn, PaneIn};
use super::super::allocation_level1::ClassPlan;
use super::*;
use marspot_agent::sched::forecast::{self, Facts};
use std::fs;

const NOW: i64 = 1_795_716_000;

fn account(profile: u8, week_used: f64) -> AccountIn {
    AccountIn { profile, week_used, fable_used: Some(0.9), fable_known: true, week_renews: NOW + 60 * 3600, fable_renews: NOW + 60 * 3600, open: true, measured_at: NOW - 60, supply: Vec::new() }
}

fn pane(sid: u64, profile: u8) -> PaneIn {
    PaneIn { sid, profile, class: Class::Plain, marked_at: None, refused_at: None, resting: true, helper: false, out: false, transcript: None }
}

fn root(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-level2-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

/// Three panes on P3, which empties in about three hours; P7 has room.
fn scene() -> (Snapshot, View, Forecast) {
    let accounts = vec![account(3, 0.95), account(7, 0.4)];
    let panes = vec![pane(441, 3), pane(481, 3), pane(430, 3)];
    let live: Vec<u64> = panes.iter().map(|p| p.sid).collect();
    let snap = Snapshot { now: NOW, feed_at: Some(NOW - 60), accounts, panes, live: live.clone() };
    let rates = [(0.0, 0.006), (0.0, 0.004), (0.0, 0.003)];
    let view = View::from_rates(&snap, &rates, 0.0);
    let mut facts = Facts::default();
    for (sid, (_, b)) in live.iter().zip(rates) {
        for k in 1..=10 {
            facts.used(NOW - k * 3600, *sid, Class::Plain, b);
        }
    }
    let fc = forecast::build(&facts, NOW, &live);
    (snap, view, fc)
}

#[test]
fn allocation_level2_with_no_pane_runs_no_plan() {
    let r = root("none");
    let mut a = Allocation::new(events_dir(&r));
    let (mut snap, _, fc) = scene();
    snap.panes.clear();
    let view = View::from_rates(&snap, &[], 0.0);
    let mut cp = ClassPlan::default();
    cp.scan(&snap.accounts, &view, Some(&fc), 0.0, NOW);
    let mut pp = PanePlan::new(&r);
    assert!(pp.scan(&mut a, &snap, &view, Some(&fc), cp.last()).is_empty());
    // nor without a class plan
    let (snap, view, fc) = scene();
    assert!(pp.scan(&mut a, &snap, &view, Some(&fc), None).is_empty());
    assert_eq!((pp.passes, pp.full), (0, 0), "the planner ran");
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_level2_logs_a_plan_records_its_fact_and_writes_the_status() {
    let r = root("plan");
    let mut a = Allocation::new(events_dir(&r));
    let (snap, view, fc) = scene();
    let mut cp = ClassPlan::default();
    cp.scan(&snap.accounts, &view, Some(&fc), 0.0, NOW);
    let mut pp = PanePlan::new(&r);
    let log = pp.scan(&mut a, &snap, &view, Some(&fc), cp.last());
    let line = log.iter().find(|l| l.0 == "sched.plan").map(|l| l.1.clone()).unwrap_or_else(|| panic!("no plan line: {log:?}"));
    assert!(line.starts_with("trigger=first mode="), "{line}");
    assert!(line.contains(">P7@"), "a pane leaves P3 for P7: {line}");
    assert!(line.len() < 400, "{} bytes", line.len());
    let facts = a.events.read_since(NOW - 10, NOW + 10);
    let plan = facts.iter().find(|e| e.kind == "plan").expect("a plan fact");
    assert_eq!(plan.sid, 0);
    assert!(plan.detail.contains(" prev=0000000000000000 ") && plan.detail.contains(">P7#0"), "{}", plan.detail);
    let status = fs::read_to_string(r.join("sched").join("status.json")).expect("the status file");
    assert!(status.starts_with("{\"at\":") && status.contains("\"panes\":[") && status.ends_with("]}\n"), "{status}");
    // the next scan a second later re-times only: no line, no fact
    let mut later = snap.clone();
    later.now = NOW + 2;
    let log = pp.scan(&mut a, &later, &view, Some(&fc), cp.last());
    assert!(log.iter().all(|l| l.0 != "sched.plan"), "{log:?}");
    assert_eq!((pp.passes, pp.full), (2, 1));
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_level2_plans_a_pane_at_its_three_hour_speed_when_that_is_off_the_band() {
    let r = root("short");
    let a = Allocation::new(events_dir(&r));
    let (snap, view, _) = scene();
    // 441 ran at its level for seven hours, then five times faster
    let mut facts = Facts::default();
    for k in 1..=10 {
        let rate = if k <= 3 { 0.03 } else { 0.006 };
        facts.used(NOW - k * 3600, 441, Class::Plain, rate);
        facts.used(NOW - k * 3600, 481, Class::Plain, 0.004);
        facts.used(NOW - k * 3600, 430, Class::Plain, 0.003);
    }
    let fc = forecast::build(&facts, NOW, &snap.live);
    let fp = fc.panes.iter().find(|p| p.sid == 441).expect("441 in the forecast");
    assert!(fp.short[1] > fp.day[1] * fc.pane_band[1].hi, "441's recent speed {} is inside the band of {}", fp.short[1], fp.day[1]);
    let mut cp = ClassPlan::default();
    cp.scan(&snap.accounts, &view, Some(&fc), 0.0, NOW);
    let b = super::super::allocation_level2_input::build(&a, &snap, &view, Some(&fc), cp.last().expect("a class plan"), &Standing::default());
    let p = b.panes.iter().find(|p| p.sid == 441).expect("441 planned");
    assert_eq!(p.speed[1], fp.short[1]);
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_level2_checks_the_pane_speeds_against_the_account_readings() {
    let r = root("calib");
    let mut a = Allocation::new(events_dir(&r));
    let (mut snap, view, fc) = scene();
    snap.accounts[0].week_used = 0.5;
    let mut cp = ClassPlan::default();
    cp.scan(&snap.accounts, &view, Some(&fc), 0.0, NOW);
    let b = super::super::allocation_level2_input::build(&a, &snap, &view, Some(&fc), cp.last().expect("a class plan"), &Standing::default());
    let asked: f64 = b.panes.iter().filter(|p| p.on == b.accounts[0].q.id).map(|p| p.speed[0] + p.speed[1]).sum();
    let mut pp = PanePlan::new(&r);
    pp.scan(&mut a, &snap, &view, Some(&fc), cp.last());
    // eight hours on, P3 lost a third of what its panes' speeds put on it
    let mut later = snap.clone();
    later.now = NOW + 8 * 3600;
    later.feed_at = Some(later.now - 60);
    later.accounts[0].week_used = 0.5 + asked * 8.0 / 3.0;
    for acc in &mut later.accounts {
        acc.measured_at = later.now - 60;
    }
    cp.scan(&later.accounts, &view, Some(&fc), 0.0, later.now);
    let log = pp.scan(&mut a, &later, &view, Some(&fc), cp.last());
    let line = log.iter().find(|l| l.0 == "sched.calib").map(|l| l.1.clone()).unwrap_or_else(|| panic!("no calib line: {log:?}"));
    assert_eq!(line, "ratio=0.333 factor=0.333");
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn allocation_level2_plans_a_pane_idle_for_three_hours_at_the_bottom_of_its_band() {
    let r = root("idle");
    let a = Allocation::new(events_dir(&r));
    let (snap, view, _) = scene();
    // 441 ran at its level for seven hours, then used nothing
    let mut facts = Facts::default();
    for k in 1..=10 {
        if k > 3 {
            facts.used(NOW - k * 3600, 441, Class::Plain, 0.006);
        }
        facts.used(NOW - k * 3600, 481, Class::Plain, 0.004);
        facts.used(NOW - k * 3600, 430, Class::Plain, 0.003);
    }
    let fc = forecast::build(&facts, NOW, &snap.live);
    let fp = fc.panes.iter().find(|p| p.sid == 441 && p.counts).expect("441 counts in the forecast");
    assert_eq!(fp.short[1], 0.0);
    let mut cp = ClassPlan::default();
    cp.scan(&snap.accounts, &view, Some(&fc), 0.0, NOW);
    let b = super::super::allocation_level2_input::build(&a, &snap, &view, Some(&fc), cp.last().expect("a class plan"), &Standing::default());
    let p = b.panes.iter().find(|p| p.sid == 441).expect("441 planned");
    assert!(p.speed[1] > 0.0 && (p.speed[1] - fp.day[1] * fc.pane_band[1].lo).abs() < 1e-15, "{} against a level of {}", p.speed[1], fp.day[1]);
    assert!(b.outside.contains(&441));
    let _ = fs::remove_dir_all(&r);
}
