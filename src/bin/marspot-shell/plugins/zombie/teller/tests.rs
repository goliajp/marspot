use std::path::PathBuf;

use marspot::zombie::check::{Hit, Key};
use marspot::zombie::rules::Rule;
use marspot::zombie::tell::PANE_GAP;

use super::super::store::Pacers;
use super::*;

const NOW: u64 = 1_800_000_000;

fn suspect(sid: u64, id: &str) -> Suspect {
    Suspect { hit: Hit { key: Key { sid, rule: Rule::B1, id: id.into() }, evidence: format!("后台任务 {id}") }, since: NOW }
}

fn waiting(sid: u64) -> PaneReach {
    PaneReach { sid, claude_pid: 40 + sid as i32, awaiting_since: Some(NOW - 600), ..PaneReach::default() }
}

struct Run {
    dir: PathBuf,
    reach: HashMap<u64, PaneReach>,
    sent: Vec<(i32, String)>,
    fail: Option<String>,
}

impl Run {
    fn new(tag: &str, panes: &[PaneReach]) -> Run {
        let dir = std::env::temp_dir().join(format!("zombie-teller-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Run { dir, reach: panes.iter().map(|p| (p.sid, p.clone())).collect(), sent: Vec::new(), fail: None }
    }

    fn tell(&mut self, t: &mut Teller, notify: Notify, now: u64, s: &[Suspect], p: &mut Pacer) -> bool {
        let ended = |_: u64| true;
        let cx = Cx { notify, now, scale: 1, dir: &self.dir, reach: &self.reach, turn_ended: &ended };
        let (sent, fail) = (&mut self.sent, self.fail.clone());
        t.tell(&cx, s, p, &mut |pid, body| match &fail {
            Some(e) => Err(e.clone()),
            None => {
                sent.push((pid, body.to_string()));
                Ok(())
            }
        })
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("dryrun.log")).unwrap_or_default()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn zombie_dry_run_writes_the_message_and_sends_nothing() {
    let mut r = Run::new("dry", &[waiting(7)]);
    let (mut t, mut p) = (Teller::default(), Pacers::default());
    assert!(r.tell(&mut t, Notify::DryRun, NOW, &[suspect(7, "1@2")], p.of(Notify::DryRun)));
    assert!(r.sent.is_empty());
    let log = r.log();
    assert!(log.starts_with(&format!("== would_tell {NOW} sid=7 items=1@2\nmarspot 提醒：")), "{log}");
    assert!(p.dry.told("1@2"), "told in the dry run's own ledger");
    assert!(!p.live.told("1@2") && p.live.allows(7, NOW), "the live ledger and clock are untouched");
    assert!(!r.tell(&mut t, Notify::DryRun, NOW + PANE_GAP, &[suspect(7, "1@2")], p.of(Notify::DryRun)), "not would_tell again");
}

#[test]
fn zombie_a_dry_run_message_does_not_keep_a_live_one_from_going_out() {
    let mut r = Run::new("dry-then-live", &[waiting(7)]);
    let (mut t, mut p) = (Teller::default(), Pacers::default());
    let s = [suspect(7, "1@2")];
    assert!(r.tell(&mut t, Notify::DryRun, NOW, &s, p.of(Notify::DryRun)));
    assert!(r.tell(&mut t, Notify::Live, NOW + 60, &s, p.of(Notify::Live)), "the live pacing starts with nothing told");
    assert_eq!(r.sent.len(), 1);
    assert!(r.sent[0].1.contains("1. 后台任务 1@2"));
    assert!(!r.tell(&mut t, Notify::Live, NOW + PANE_GAP, &s, p.of(Notify::Live)), "told live once");
}

#[test]
fn zombie_live_sends_one_message_per_pane_to_its_claude_and_never_again() {
    let mut r = Run::new("live", &[waiting(7), waiting(8)]);
    let (mut t, mut p) = (Teller::default(), Pacer::default());
    let s = [suspect(7, "1@2"), suspect(7, "3@4"), suspect(8, "5@6")];
    assert!(r.tell(&mut t, Notify::Live, NOW, &s, &mut p));
    assert_eq!(r.sent.len(), 2);
    assert_eq!(r.sent[0].0, 47);
    assert!(r.sent[0].1.contains("1. 后台任务 1@2") && r.sent[0].1.contains("2. 后台任务 3@4"));
    assert_eq!(r.sent[1].0, 48);
    assert!(r.log().contains(&format!("== told {NOW} sid=7 items=1@2,3@4\n")));
    assert!(!r.tell(&mut t, Notify::Live, NOW + 5 * 3600, &s, &mut p), "every item told");
    assert_eq!(r.sent.len(), 2);
}

#[test]
fn zombie_an_undelivered_message_stays_untold() {
    let mut r = Run::new("undelivered", &[waiting(7)]);
    r.fail = Some("the session has no inbox".into());
    let (mut t, mut p) = (Teller::default(), Pacer::default());
    assert!(!r.tell(&mut t, Notify::Live, NOW, &[suspect(7, "1@2")], &mut p));
    assert!(!p.told("1@2") && p.allows(7, NOW));
    r.fail = None;
    assert!(r.tell(&mut t, Notify::Live, NOW + 300, &[suspect(7, "1@2")], &mut p));
}

#[test]
fn zombie_no_message_while_the_person_is_there_the_account_refuses_or_the_pane_rests() {
    for (tag, f) in [
        ("here", (|p: &mut PaneReach| p.user_here = true) as fn(&mut PaneReach)),
        ("refused", |p| p.refused = true),
        ("dormant", |p| p.dormant = true),
        ("busy", |p| p.awaiting_since = None),
    ] {
        let mut pane = waiting(7);
        f(&mut pane);
        let mut r = Run::new(tag, &[pane]);
        let (mut t, mut p) = (Teller::default(), Pacer::default());
        assert!(!r.tell(&mut t, Notify::Live, NOW, &[suspect(7, "1@2")], &mut p), "{tag}");
        assert!(r.sent.is_empty() && r.log().is_empty() && !p.told("1@2"), "{tag}");
    }
    let mut r = Run::new("unknown", &[]);
    assert!(!r.tell(&mut Teller::default(), Notify::DryRun, NOW, &[suspect(7, "1@2")], &mut Pacer::default()), "a pane with no state");
}

#[test]
fn zombie_held_back_messages_leave_a_countable_entry_once_per_reason() {
    let panes: Vec<PaneReach> = (1..=4).map(waiting).collect();
    let mut r = Run::new("held", &panes);
    let (mut t, mut p) = (Teller::default(), Pacer::default());
    let s: Vec<Suspect> = (1..=4).map(|sid| suspect(sid, &format!("{sid}@1"))).collect();
    r.tell(&mut t, Notify::DryRun, NOW, &s, &mut p);
    r.tell(&mut t, Notify::DryRun, NOW + 60, &s, &mut p);
    let log = r.log();
    assert_eq!(log.matches("== would_tell ").count(), 3, "three an hour");
    assert_eq!(log.matches("== held ").count(), 1, "the fourth, once: {log}");
    assert!(log.contains(&format!("== held {NOW} sid=4 items=4@1 why=host_hour\n")));
}

#[test]
fn zombie_items_the_message_only_counts_are_told_in_the_next_window() {
    let long = |id: &str| {
        let mut s = suspect(7, id);
        s.hit.evidence = format!("后台任务 {id}，命令：{}", "x".repeat(2048));
        s
    };
    let many: Vec<Suspect> = (0..13).map(|i| suspect(7, &format!("{i}@1"))).collect();
    for (tag, s, listed) in [("thirteen", many, 12), ("long", vec![long("1@2"), long("3@4")], 1)] {
        let mut r = Run::new(tag, &[waiting(7)]);
        let (mut t, mut p) = (Teller::default(), Pacer::default());
        assert!(r.tell(&mut t, Notify::Live, NOW, &s, &mut p), "{tag}");
        let (shown, counted) = s.split_at(listed);
        assert!(shown.iter().all(|x| p.told(&x.hit.key.id)), "{tag}: the listed items are told");
        assert!(counted.iter().all(|x| !p.told(&x.hit.key.id)), "{tag}: the counted ones are not");
        assert!(r.sent[0].1.contains(&format!("另有 {} 项", counted.len())), "{tag}");
        assert!(!r.tell(&mut t, Notify::Live, NOW + 60, &s, &mut p), "{tag}: within the pane's four hours");
        assert!(r.tell(&mut t, Notify::Live, NOW + PANE_GAP, &s, &mut p), "{tag}: the next window");
        assert_eq!(r.sent.len(), 2, "{tag}");
        for x in counted {
            assert!(p.told(&x.hit.key.id) && r.sent[1].1.contains(&format!("1. 后台任务 {}", x.hit.key.id)), "{tag}: {}", r.sent[1].1);
        }
    }
}
