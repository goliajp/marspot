//! S1 and S2 replayed over saved records: a session as it stood at any
//! moment, with a task counted alive from the record that started it to
//! the first notification of its end (a replay has no processes).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use super::super::records::{self, Rec};
use super::super::rules::{self, Last, Limits};
use super::super::transcript::{ClaudeLog, SubLog, SubMeta};
use super::fixtures;

enum Src {
    Main,
    Sub(String),
}

pub(super) struct History {
    events: Vec<(u64, Src, Rec)>,
    subs: BTreeMap<String, (u64, SubMeta)>,
}

fn recs(path: &Path) -> Vec<Rec> {
    fs::read_to_string(path).unwrap_or_default().lines().filter_map(records::parse).collect()
}

impl History {
    /// `main` and the subagents in `subdir` (`agent-<id>.jsonl`, with
    /// `.meta.json` and a `.birth` file holding the transcript's birth
    /// time), all of them or the ones named.
    pub fn load(main: &Path, subdir: &Path) -> History {
        let mut events: Vec<(u64, Src, Rec)> = recs(main).into_iter().map(|r| (r.at(), Src::Main, r)).collect();
        let mut subs = BTreeMap::new();
        for e in fs::read_dir(subdir).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(id) = name.strip_prefix("agent-").and_then(|n| n.strip_suffix(".jsonl")) else { continue };
            let meta = SubMeta::parse(&fs::read_to_string(subdir.join(format!("agent-{id}.meta.json"))).unwrap_or_default());
            let birth = fs::read_to_string(subdir.join(format!("agent-{id}.birth"))).ok().and_then(|b| b.trim().parse().ok());
            let rs = recs(&e.path());
            let birth = birth.or_else(|| rs.first().map(Rec::at)).unwrap_or(0);
            events.extend(rs.into_iter().map(|r| (r.at(), Src::Sub(id.to_string()), r)));
            subs.insert(id.to_string(), (birth, meta));
        }
        events.sort_by_key(|e| e.0);
        History { events, subs }
    }

    pub fn fixture() -> History {
        let d = fixtures().join("records");
        History::load(&d.join("main-153c990a.jsonl"), &d)
    }

    fn fold(&self, log: &mut ClaudeLog, e: &(u64, Src, Rec)) {
        match &e.1 {
            Src::Main => log.main.feed(&e.2, &mut log.tasks),
            Src::Sub(id) => {
                let (birth, meta) = &self.subs[id];
                let (sub, _) = log.subs.entry(id.clone()).or_insert_with(|| (SubLog::born(Some(*birth)), meta.clone()));
                sub.feed(&e.2, &mut log.tasks);
            }
        }
    }

    /// The session as it stood at `t`.
    pub fn at(&self, t: u64) -> ClaudeLog {
        let mut log = ClaudeLog::default();
        for e in self.events.iter().take_while(|e| e.0 <= t) {
            self.fold(&mut log, e);
        }
        log
    }
}

/// A subagent's view at `t` in a replayed log: its tasks alive until
/// their end was notified, finished when a finish or stop was reported
/// after its last record.
pub(super) fn view<'a>(log: &ClaudeLog, id: &'a str) -> rules::Subagent<'a> {
    let finished = |id: &str, sub: &SubLog| log.tasks.finished.get(&id.to_string()).is_some_and(|f| *f >= sub.last_at);
    let (sub, _) = &log.subs[id];
    let mut v = sub.view(id, finished(id, sub), |t| log.tasks.started.get(&t.to_string()).is_some() && log.tasks.ended.get(&t.to_string()).is_none());
    v.tasks_alive += log.subs.iter().filter(|(k, (s, m))| m.parent.as_deref() == Some(id) && !finished(k, s)).count();
    v
}

/// S1 over `t` in the replay.
pub(super) fn s1_at<'a>(h: &History, id: &'a str, t: u64) -> (bool, rules::Subagent<'a>) {
    let log = h.at(t);
    let v = view(&log, id);
    (rules::s1(&v, 0, log.main.turn_ended, t, &Limits::default()), v)
}

/// One S1 hit of a sweep: the subagent, when S1 first held in a quiet
/// spell, when that spell began, and what came next.
#[derive(Debug)]
pub(super) struct Found {
    pub id: String,
    pub at: u64,
    pub quiet_from: u64,
    pub next: Option<(u64, String)>,
}

/// Every quiet spell in which S1 holds at some moment, at the first such moment.
pub(super) fn sweep(h: &History) -> Vec<Found> {
    let l = Limits::default();
    let mut log = ClaudeLog::default();
    let mut idle: BTreeSet<String> = BTreeSet::new();
    let mut found: Vec<Found> = Vec::new();
    let mut hit_spells: BTreeSet<(String, u64)> = BTreeSet::new();
    for (k, e) in h.events.iter().enumerate() {
        self_fold(h, &mut log, e, &mut idle);
        // past the last record nothing is known
        let Some(end) = h.events.get(k + 1).map(|n| n.0) else { break };
        let Some(ended) = log.main.turn_ended else { continue };
        let mut done = Vec::new();
        for id in &idle {
            let v = view(&log, id);
            if v.finished {
                done.push(id.clone());
                continue;
            }
            if v.tasks_alive > 0 || v.last != Last::Idle || hit_spells.contains(&(id.clone(), v.last_at)) {
                continue;
            }
            let t = e.0.max(v.last_at + l.s1_quiet).max(ended + l.s1_idle);
            if t < end {
                debug_assert!(rules::s1(&v, 0, Some(ended), t, &l));
                hit_spells.insert((id.clone(), v.last_at));
                found.push(Found { id: id.clone(), at: t, quiet_from: v.last_at, next: None });
            }
        }
        for id in done {
            idle.remove(&id);
        }
    }
    for f in &mut found {
        f.next = h.events.iter().find(|e| e.0 > f.at && matches!(&e.1, Src::Sub(s) if *s == f.id)).map(|e| (e.0, kind(&e.2)));
    }
    found
}

fn self_fold(h: &History, log: &mut ClaudeLog, e: &(u64, Src, Rec), idle: &mut BTreeSet<String>) {
    h.fold(log, e);
    if let Src::Sub(id) = &e.1 {
        if log.subs[id].0.last == Last::Idle {
            idle.insert(id.clone());
        } else {
            idle.remove(id);
        }
    }
}

fn kind(r: &Rec) -> String {
    match r {
        Rec::User { notes, .. } if !notes.is_empty() => {
            format!("notified:{}", notes.iter().map(|n| n.task.as_str()).collect::<Vec<_>>().join(","))
        }
        Rec::User { results, .. } if !results.is_empty() => "tool result".into(),
        Rec::User { .. } => "message".into(),
        Rec::Assistant { .. } => "reply".into(),
        Rec::TurnEnd { .. } => "turn end".into(),
        Rec::Queued { .. } => "queued".into(),
    }
}

fn t(s: &str) -> u64 {
    crate::cc_usage::parse_iso_utc(s.to_string()).unwrap() as u64
}

const W3B: &str = "aa03fffa738e7db41";

#[test]
fn zombie_replay_s1_hits_w3b_once_its_monitor_ended_and_the_main_session_rested() {
    let h = History::fixture();
    // 00:57:43 its last reply, waiting for a build; 01:09:44 its monitor
    // bkssmnnza ends; 01:23:05 the main session's last turn before 01:57:50 ends
    let (hit, v) = s1_at(&h, W3B, t("2026-11-23T01:20:00Z"));
    assert!(!hit, "quiet 22 min only: {v:?}");
    let (hit, v) = s1_at(&h, W3B, t("2026-11-23T01:30:00Z"));
    assert!(!hit && v.tasks_alive == 0, "quiet enough, the main session idle 7 min only: {v:?}");
    let (hit, v) = s1_at(&h, W3B, t("2026-11-23T01:40:00Z"));
    assert!(hit, "{v:?}");
    let first = sweep(&h).into_iter().find(|f| f.id == W3B).expect("the sweep finds w-3b");
    assert_eq!(first.at, t("2026-11-23T01:33:05Z"), "ten minutes after the main session's turn ended");
    assert_eq!(first.quiet_from, t("2026-11-23T00:57:43Z"));
}

#[test]
fn zombie_replay_s1_does_not_hit_a_subagent_whose_background_command_still_runs_on_a_remote() {
    // aa933e36: 04:40:04 starts brftivyiv (window tests on a remote machine), 04:43:34 ends its turn;
    // the command's end is notified at 06:06:18
    let h = History::fixture();
    let at = t("2026-11-12T05:30:00Z");
    let (hit, v) = s1_at(&h, "aa933e36dcf92f20b", at);
    assert!(!hit);
    assert!(v.tasks_alive >= 1, "{v:?}");
    let log = h.at(at);
    let without = rules::Subagent { tasks_alive: 0, ..v };
    assert!(rules::s1(&without, 0, log.main.turn_ended, at, &Limits::default()), "that command is all that keeps S1 off");
}

#[test]
fn zombie_replay_s2_hits_the_two_foreground_commands_that_hung() {
    let h = History::fixture();
    let l = Limits::default();
    // ab780d53: a Bash call at 20:54:50, answered at 21:56:21
    let at = t("2026-11-22T21:30:00Z");
    let log = h.at(at);
    let v = view(&log, "ab780d53de32e5d5c");
    assert_eq!(rules::s2(&v, at, &l), Some(at - t("2026-11-22T20:54:50Z")), "{v:?}");
    assert_eq!(rules::s2(&v, t("2026-11-22T21:00:00Z"), &l), None, "five minutes in it is within its limit");
    // ab5e072c: a Bash call at 21:18:25, answered at 22:44:52
    let at = t("2026-11-22T22:40:00Z");
    let log = h.at(at);
    let v = view(&log, "ab5e072cfd8e4bc8f");
    assert!(rules::s2(&v, at, &l).is_some(), "{v:?}");
}

/// S1 over every subagent of a whole session, printed for a person to
/// judge.  `MARSPOT_ZOMBIE_HISTORY` names a directory with `main.jsonl`
/// and `subagents/`.
#[test]
#[ignore]
fn zombie_s1_over_a_whole_session() {
    let Some(dir) = std::env::var_os("MARSPOT_ZOMBIE_HISTORY") else { return };
    let dir = Path::new(&dir);
    let h = History::load(&dir.join("main.jsonl"), &dir.join("subagents"));
    let found = sweep(&h);
    println!("S1-SUBAGENTS\t{}", h.subs.len());
    println!("S1-HITS\t{}", found.len());
    for f in &found {
        let (_, meta) = &h.subs[&f.id];
        let next = f.next.as_ref().map_or("none\t-".to_string(), |(at, k)| format!("{at}\t{k}"));
        println!("S1-HIT\t{}\t{}\t{}\t{}\t{}\t{}", f.id, f.quiet_from, f.at, next, meta.depth, meta.description);
    }
}
