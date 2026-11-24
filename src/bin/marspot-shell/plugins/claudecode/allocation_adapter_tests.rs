//! One line per change in `alloc_events`, none without, across a restart.

use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::plugins::allocation::Class;
use crate::plugins::allocation::replay::replay;
use crate::plugins::claudecode::allocation_inputs::{AccountIn, MarksFile, PaneIn, Snapshot};
use crate::plugins::claudecode::fable_need::{Ask, Marks};

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
    AccountIn { profile, week_used: 0.5, fable_used: Some(0.0), fable_known: true, week_renews: NOW + 100 * 3600, fable_renews: NOW + 100 * 3600, open: true, measured_at: NOW }
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
    // the line carries every input: fed back, the same answer comes out
    let would = lines(&d).into_iter().find(|l| l.split('\t').nth(3) == Some("would_move")).unwrap();
    let detail = would.split('\t').nth(6).unwrap();
    assert!(detail.starts_with("to=P2 why=out "), "{detail}");
    let (to, why) = replay(detail).unwrap().unwrap();
    assert_eq!((to, why.word()), (2, "out"));
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
    assert!(step(&mut a, &d, &snap(NOW + 10, vec![p.clone()], vec![1])).is_empty(), "held until five minutes have passed");
    append(reply("m2", 4000, "2026-11-24T14:31:30Z"));
    assert_eq!(step(&mut a, &d, &snap(NOW + 310, vec![p.clone()], vec![1])), ["used"], "one line for both replies");
    let used = lines(&d).pop().unwrap();
    assert!(used.ends_with("\tplain\tmodel=claude-opus-5-5 in=10000 cc5m=0 cc1h=0 cr=0 out=7000"), "the tokens, not their price: {used}");
    assert!(step(&mut a, &d, &snap(NOW + 640, vec![p.clone()], vec![1])).is_empty(), "nothing used, nothing written");
    let at = NOW + 3600;
    let running = a.used.get(&1).unwrap().rate(at, Class::Plain);
    assert!(running.is_some_and(|r| r > 0.0));
    let mut again = Allocation::new(d.clone());
    again.rebuild(NOW + 700);
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
    assert_eq!(tags(a.observe(&snap(NOW + 6, both.clone(), vec![1, 2]))), ["alloc.stale_feed", "alloc.plan"]);
    // readings that move the targets but not where panes are or would go
    let mut moved = snap(NOW + 8, both, vec![1, 2]);
    moved.accounts[0].week_used = 0.52;
    assert!(a.observe(&moved).is_empty());
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_restart_reads_every_kept_day_for_what_panes_were() {
    let d = dir("days");
    let mut log = EventLog::new(d.clone());
    let old = |at, kind: &str, profile, detail: &str| Event { at, sid: 4, kind: kind.into(), profile, class: "plain".into(), detail: detail.into() };
    log.append(&old(NOW - 3 * DAY, "bound", Some(2), "")).unwrap();
    log.append(&old(NOW - 3 * DAY + 5, "rest", Some(2), "")).unwrap();
    log.append(&old(NOW - 2 * DAY, "refused", Some(2), "at=1795300000")).unwrap();
    let mut a = Allocation::new(d.clone());
    a.rebuild(NOW);
    let l = a.last.get(&4).unwrap();
    assert_eq!((l.profile, l.bound_at, l.resting, l.refused_at), (Some(2), Some(NOW - 3 * DAY), Some(true), Some(1_795_300_000)));
    // nothing to say again on the first scan
    let p = PaneIn { refused_at: Some(1_795_300_000), ..pane(4, 2, true) };
    assert!(step(&mut a, &d, &snap(NOW, vec![p], vec![4])).is_empty(), "{:?}", lines(&d));
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_reads_the_main_threads_fable_marks() {
    let d = dir("marks");
    fs::create_dir_all(&d).unwrap();
    let mut marks = Marks::default();
    marks.note(9, &Ask { at: NOW - 60, agent: None }, NOW - 30);
    fs::write(d.join("fable_need.tsv"), marks.changed().unwrap()).unwrap();
    let mut file = MarksFile::new(d.clone());
    assert_eq!(file.read(NOW).get(&9), Some(&(NOW - 30)));
    fs::remove_file(d.join("fable_need.tsv")).unwrap();
    assert!(file.read(NOW).is_empty(), "gone with the file");
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_unread_fable_cap_is_no_fable_target() {
    let mut ac = account(1);
    ac.fable_used = None;
    ac.fable_known = false;
    let q = ac.quant(NOW, 0.0);
    assert!(!q.accepts(Class::Restricted) && q.accepts(Class::Plain), "{q:?}");
    ac.fable_known = true;
    assert!(ac.quant(NOW, 0.0).accepts(Class::Restricted), "no Fable cap listed: nothing holds Fable back");
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

#[test]
fn allocation_held_back_notice_once_per_pane() {
    // P1 is out; all of P2's week is Fable's, and the Fable pane there
    // wants more than P2 can give it an hour
    let d = dir("held");
    let mut a = Allocation::new(d.clone());
    let mut s = snap(NOW, vec![], vec![1, 2]);
    s.accounts[0].week_used = 1.0;
    let mut p = pane(1, 1, false);
    p.out = true;
    let f = PaneIn { class: Class::Restricted, ..pane(2, 2, false) };
    s.panes = vec![p, f];
    let notices = |log: Vec<(&'static str, String)>| log.into_iter().filter(|(t, _)| *t == "alloc.notice").map(|(_, l)| l).collect::<Vec<_>>();
    assert_eq!(notices(a.observe(&s)), ["sid=1: waits rather than take what the Fable panes are short of on P2"]);
    s.now += 2;
    assert!(notices(a.observe(&s)).is_empty());
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_restart_counts_the_week_s_resting_would_moves() {
    let d = dir("week");
    let mut log = EventLog::new(d.clone());
    let would = |at, why: &str| Event { at, sid: 4, kind: "would_move".into(), profile: Some(2), class: "plain".into(), detail: format!("to=P3 why={why} here=2") };
    log.append(&would(NOW - 8 * DAY, "u_out")).unwrap();
    log.append(&would(NOW - 2 * DAY, "out")).unwrap();
    log.append(&would(NOW - DAY, "u_out")).unwrap();
    log.append(&would(NOW - 60, "leave_early")).unwrap();
    let mut a = Allocation::new(d.clone());
    a.rebuild(NOW);
    let l = a.last.get(&4).unwrap();
    let week: Vec<i64> = l.rest_moves.iter().filter(|at| NOW - **at < WEEK_S).copied().collect();
    assert_eq!(week, [NOW - DAY, NOW - 60], "forced moves and those over a week old do not count");
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn allocation_same_target_after_busy_is_one_would_move() {
    // P1 runs out within the hour; pane 3, not the slowest plain pane
    // there, would leave early for P2 each time it rests
    let d = dir("again");
    let mut a = Allocation::new(d.clone());
    let at = |now: i64, rest3: bool| {
        let mut s = snap(now, vec![pane(1, 1, true), pane(3, 1, rest3)], vec![1, 3]);
        s.accounts[0].week_used = 0.995;
        for acc in &mut s.accounts {
            acc.fable_used = Some(1.0);
        }
        s
    };
    let would = |kinds: Vec<String>| kinds.iter().filter(|k| *k == "would_move").count();
    assert_eq!(would(step(&mut a, &d, &at(NOW, true))), 1);
    assert_eq!(would(step(&mut a, &d, &at(NOW + 60, false))), 0);
    assert_eq!(would(step(&mut a, &d, &at(NOW + 120, true))), 0, "the same target is not a second move");
    assert_eq!(a.last.get(&3).map(|l| l.rest_moves.len()), Some(1));
    // a restarted shell reads the target back and it still stands
    let mut again = Allocation::new(d.clone());
    assert_eq!(would(step(&mut again, &d, &at(NOW + 180, true))), 0);
    assert_eq!(again.last.get(&3).map(|l| l.rest_moves.len()), Some(1));
    fs::remove_dir_all(&d).unwrap();
}
