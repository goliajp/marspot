//! One line per change in `alloc_events`, none without, across a restart.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::plugins::allocation::Class;
use crate::plugins::claudecode::allocation_inputs::{AccountIn, PaneIn, Snapshot};

const NOW: i64 = 1_795_530_644;

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("marspot-alloc-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

fn lines(d: &Path) -> Vec<String> {
    let mut names: Vec<PathBuf> = fs::read_dir(d).map(|r| r.map(|e| e.unwrap().path()).collect()).unwrap_or_default();
    names.sort();
    names.iter().flat_map(|p| fs::read_to_string(p).unwrap().lines().map(str::to_string).collect::<Vec<_>>()).collect()
}

fn account(profile: u8) -> AccountIn {
    AccountIn { profile, week_used: 0.5, fable_used: Some(0.0), week_renews: NOW + 100 * 3600, fable_renews: NOW + 100 * 3600, open: true, measured_at: NOW }
}

fn pane(sid: u64, profile: u8, resting: bool) -> PaneIn {
    PaneIn { sid, profile, class: Class::Plain, marked_at: None, refused_at: None, resting, helper: false, out: false, transcript: None }
}

fn snap(now: i64, panes: Vec<PaneIn>, live: Vec<u64>) -> Snapshot {
    Snapshot { now, feed_at: Some(now), accounts: vec![account(1), account(2)], panes, live }
}

/// The kinds written by one more scan.
fn step(a: &mut Allocation, d: &Path, s: &Snapshot) -> Vec<String> {
    let before = lines(d).len();
    a.observe(s);
    lines(d)[before..].iter().map(|l| l.split('\t').nth(3).unwrap().to_string()).collect()
}

#[test]
fn allocation_events_one_line_per_change() {
    let d = dir("lines");
    let mut a = Allocation::new(d.clone());
    let both = || vec![pane(1, 1, false), pane(2, 2, true)];
    assert_eq!(step(&mut a, &d, &snap(NOW, both(), vec![1, 2])), ["bound", "busy", "bound", "rest"]);
    assert!(step(&mut a, &d, &snap(NOW + 2, both(), vec![1, 2])).is_empty(), "nothing changed, nothing written");
    let rested = || vec![pane(1, 1, true), pane(2, 2, true)];
    assert_eq!(step(&mut a, &d, &snap(NOW + 4, rested(), vec![1, 2])), ["rest"]);
    let refused = || {
        let mut p = rested();
        p[0].refused_at = Some(NOW + 5);
        p[0].out = true;
        p
    };
    assert_eq!(step(&mut a, &d, &snap(NOW + 6, refused(), vec![1, 2])), ["refused", "would_move"]);
    assert!(step(&mut a, &d, &snap(NOW + 8, refused(), vec![1, 2])).is_empty(), "the same refusal and target are not news");
    let mut moved = refused();
    moved[0].profile = 2;
    moved[0].out = false;
    assert_eq!(step(&mut a, &d, &snap(NOW + 10, moved.clone(), vec![1, 2])), ["bound"]);
    let one = vec![moved[0].clone()];
    assert_eq!(step(&mut a, &d, &snap(NOW + 12, one.clone(), vec![1])), ["bound"], "pane 2 is gone");
    assert!(step(&mut a, &d, &snap(NOW + 14, one.clone(), vec![1])).is_empty());
    // a restarted shell reads its facts back and has nothing new to say
    let mut again = Allocation::new(d.clone());
    assert!(step(&mut again, &d, &snap(NOW + 16, one.clone(), vec![1])).is_empty(), "{:?}", lines(&d));
    assert_eq!(again.last.get(&1).and_then(|l| l.profile), Some(2));
    assert!(!again.last.contains_key(&2));
    let text = lines(&d);
    assert_eq!(text.len(), 9, "{text:?}");
    assert!(text[0].starts_with(&format!("{NOW}\tclaude\t1\tbound\t1\tplain\t")), "{}", text[0]);
    fs::remove_dir_all(&d).unwrap();
}

fn reply(id: &str, out: u64, at: &str) -> String {
    format!(r#"{{"type":"assistant","timestamp":"{at}","message":{{"id":"{id}","model":"claude-opus-5-5","usage":{{"input_tokens":5000,"output_tokens":{out}}}}}}}"#) + "\n"
}

#[test]
fn allocation_events_use_rebuilds_the_same_speed() {
    let d = dir("used");
    let t = dir("used-transcript");
    fs::create_dir_all(&t).unwrap();
    let file = t.join("s.jsonl");
    fs::write(&file, reply("m0", 10, "2026-11-24T14:00:00Z")).unwrap();
    let mut p = pane(1, 1, false);
    p.transcript = Some(file.clone());
    let mut a = Allocation::new(d.clone());
    assert_eq!(step(&mut a, &d, &snap(NOW, vec![p.clone()], vec![1])), ["bound", "busy"]);
    let append = |s: String| {
        let mut f = fs::OpenOptions::new().append(true).open(&file).unwrap();
        std::io::Write::write_all(&mut f, s.as_bytes()).unwrap();
    };
    append(reply("m1", 3000, "2026-11-24T14:30:50Z"));
    assert!(step(&mut a, &d, &snap(NOW + 10, vec![p.clone()], vec![1])).is_empty(), "held until a minute has passed");
    append(reply("m2", 4000, "2026-11-24T14:31:30Z"));
    assert_eq!(step(&mut a, &d, &snap(NOW + 70, vec![p.clone()], vec![1])), ["used"], "one line for both replies");
    assert!(step(&mut a, &d, &snap(NOW + 140, vec![p.clone()], vec![1])).is_empty(), "nothing used, nothing written");
    let at = NOW + 3600;
    let running = a.used.get(&1).unwrap().rate(at, Class::Plain);
    assert!(running.is_some_and(|r| r > 0.0));
    let mut again = Allocation::new(d.clone());
    again.rebuild(NOW + 200);
    assert_eq!(again.used.get(&1).unwrap().rate(at, Class::Plain), running);
    fs::remove_dir_all(&d).unwrap();
    fs::remove_dir_all(&t).unwrap();
}

#[test]
fn allocation_plan_is_logged_when_it_changes() {
    let d = dir("plan");
    let mut a = Allocation::new(d.clone());
    let s = snap(NOW, vec![pane(1, 1, false)], vec![1]);
    let tags = |log: Vec<(&'static str, String)>| log.into_iter().map(|(t, _)| t).collect::<Vec<_>>();
    assert_eq!(tags(a.observe(&s)), ["alloc.plan"]);
    assert!(a.observe(&snap(NOW + 2, vec![pane(1, 1, false)], vec![1])).is_empty());
    let mut stale = snap(NOW + 4, vec![pane(1, 1, false)], vec![1]);
    stale.feed_at = Some(NOW + 4 - 16 * 60);
    assert_eq!(tags(a.observe(&stale)), ["alloc.stale_feed"]);
    let both = vec![pane(1, 1, false), pane(2, 1, false)];
    assert_eq!(tags(a.observe(&snap(NOW + 6, both, vec![1, 2]))), ["alloc.stale_feed", "alloc.plan"]);
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_all_out_notice_once_per_pane() {
    let d = dir("notice");
    let mut a = Allocation::new(d.clone());
    let mut s = snap(NOW, vec![], vec![1]);
    for acc in &mut s.accounts {
        acc.week_used = 1.0;
    }
    let mut p = pane(1, 1, false);
    p.out = true;
    s.panes = vec![p];
    let notices = |log: Vec<(&'static str, String)>| log.into_iter().filter(|(t, _)| *t == "alloc.notice").count();
    assert_eq!(notices(a.observe(&s)), 1);
    s.now += 2;
    assert_eq!(notices(a.observe(&s)), 0);
    fs::remove_dir_all(&d).unwrap();
}
