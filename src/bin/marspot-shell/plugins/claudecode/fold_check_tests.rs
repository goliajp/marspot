//! The Claude record fold of `marspot-adapters`, fed recorded session
//! records in pieces cut anywhere, says exactly what the four readers it
//! is to replace say of the same records:
//!
//! - token totals: `pane_tokens::TokenCache::recent`
//! - context size: `context::used_in`
//! - the task table: `marspot::zombie::transcript::ClaudeLog`
//! - the `used` rows: `allocation_usage::Readers::read`

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use marspot::zombie::transcript::{ClaudeLog, SubLog, SubMeta};
use marspot_adapters::claude::fold;
use marspot_agent::types::{Compacted, FoldSink, RecordFold, Reply, TaskChange, TurnState};

use super::super::allocation_prices::Tokens as RowTokens;
use super::super::allocation_usage::Readers;
use super::super::model::short_model;
use super::super::pane_tokens::{RECENT_S, TokenCache};
use super::used_in;
use crate::plugins::helpers::used::Model;

/// What the sink saw: each reply, and the task changes joined the way
/// the zombie reader joins calls, results and notifications.
#[derive(Default)]
pub(super) struct Sink {
    replies: Vec<Reply>,
    calls: HashMap<String, (String, Option<String>)>,
    started: HashMap<String, (String, bool, Option<String>)>,
    ended: HashMap<String, u64>,
    finished: HashMap<String, u64>,
}

impl FoldSink for Sink {
    fn used(&mut self, r: &Reply) {
        self.replies.push(*r);
    }
    fn compacted(&mut self, c: Compacted) {
        panic!("no reader of today says {c:?}");
    }
    fn task(&mut self, c: TaskChange<'_>) {
        match c {
            TaskChange::Called { call, tool, command, .. } => {
                self.calls.insert(call.into(), (tool.into(), command.map(str::to_string)));
            }
            TaskChange::Answered { call, task, known, background } => {
                let joined = self.calls.remove(call);
                assert_eq!(joined.is_some(), known, "the table and the sink agree on open call {call}");
                if let Some(task) = task {
                    let (tool, command) = joined.unwrap_or_default();
                    self.started.insert(task.into(), (tool, background, command));
                }
            }
            TaskChange::Ended { task, at, waiting } => {
                self.ended.entry(task.into()).or_insert(at);
                if !waiting {
                    self.finished.insert(task.into(), at);
                }
            }
        }
    }
}

pub(super) fn records() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut v: Vec<PathBuf> = std::fs::read_dir(root.join("zombie/records")).unwrap().map(|e| e.unwrap().path()).collect();
    v.retain(|p| p.extension().is_some_and(|e| e == "jsonl"));
    v.sort();
    v.push(root.join("claude_fold/replies.jsonl"));
    assert_eq!(v.len(), 7, "six recorded session records and the replies fixture");
    v
}

/// Where the records are cut: not at all, at every byte, every 7 and 97
/// bytes, and at pseudo-random places.
pub(super) fn cut_plans(len: usize) -> Vec<Vec<usize>> {
    let mut plans = vec![Vec::new()];
    for step in [1, 7, 97] {
        plans.push((step..len).step_by(step).collect());
    }
    let (mut x, mut at, mut random) = (0x2545_f491_u64, 0usize, Vec::new());
    while at < len {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        at += 1 + (x >> 33) as usize % 300;
        random.push(at.min(len));
    }
    plans.push(random);
    plans
}

/// The fold over `bytes`, each call handed what lies between the last
/// folded line and the next cut, as the records service reads a file.
fn folded(bytes: &[u8], cuts: &[usize]) -> (RecordFold, Sink) {
    let (mut st, mut sink) = (RecordFold::from_start(), Sink::default());
    for &cut in cuts.iter().chain([bytes.len()].iter()) {
        let from = st.at as usize;
        if cut > from {
            fold(&bytes[from..cut], &mut st, &mut sink);
        }
    }
    assert_eq!(st.at as usize, bytes.len(), "every line was folded");
    (st, sink)
}

type Started = (String, bool, Option<String>);

/// The task maps, by the sink's ids, with their sizes so that an id
/// only one side has shows.
fn render_tasks(ids: &[&String], started: impl Fn(&str) -> Option<Started>, ended: impl Fn(&str) -> Option<u64>, finished: impl Fn(&str) -> Option<u64>, sizes: [usize; 3]) -> String {
    let mut s = format!("sizes {sizes:?}\n");
    for id in ids {
        writeln!(s, "{id} started={:?} ended={:?} finished={:?}", started(id), ended(id), finished(id)).unwrap();
    }
    s
}

fn zombie_side(lines: &[&str], ids: &[&String]) -> String {
    let mut main = ClaudeLog::default();
    let mut sub = ClaudeLog::default();
    sub.subs.insert("s".into(), (SubLog::default(), SubMeta::default()));
    for l in lines {
        main.feed_main(l);
        sub.feed_sub("s", l);
    }
    let t = &main.tasks;
    let started = |id: &str| t.started.get(&id.to_string()).map(|r| (r.tool.clone(), r.background, r.command.clone()));
    let mut s = format!("turn={:?} last_at={}\n", main.main.turn_ended, main.main.last_at);
    s += &render_tasks(ids, started, |id| t.ended.get(&id.to_string()).copied(), |id| t.finished.get(&id.to_string()).copied(), [t.started.len(), t.ended.len(), t.finished.len()]);
    let (log, _) = &sub.subs["s"];
    writeln!(s, "sub first={:?} last_at={} last={:?} own={:?}", log.first, log.last_at, log.last, log.tasks).unwrap();
    // the fixed table keeps no tool names: a task's call was read when its tool is known
    for id in ids {
        let rec = t.started.get(&id.to_string());
        let how = rec.map(|r| (!r.tool.is_empty(), r.background));
        writeln!(s, "table {id} {how:?} {:?} {:?}", t.ended.get(&id.to_string()), t.finished.get(&id.to_string())).unwrap();
    }
    s
}

fn fold_side(st: &RecordFold, sink: &Sink, ids: &[&String]) -> String {
    let ended = match st.turn {
        TurnState::Ended { at } => Some(at),
        TurnState::Working | TurnState::Unknown => None,
    };
    let mut s = format!("turn={ended:?} last_at={}\n", st.tasks.last_seen);
    let started = |id: &str| sink.started.get(id).cloned();
    s += &render_tasks(ids, started, |id| sink.ended.get(id).copied(), |id| sink.finished.get(id).copied(), [sink.started.len(), sink.ended.len(), sink.finished.len()]);
    let mut own: Vec<_> = st.tasks.tasks().iter().filter_map(|t| Some((t.own?, t.id.as_str()?.to_string()))).collect();
    own.sort();
    let own: VecDeque<String> = own.into_iter().map(|(_, id)| id).collect();
    writeln!(s, "sub first={:?} last_at={} last={:?} own={own:?}", st.tasks.first, st.tasks.last_active, st.tasks.last).unwrap();
    for id in ids {
        let t = st.tasks.tasks().iter().find(|t| t.id.is(id));
        let how = t.and_then(|t| t.started).map(|h| (h.known_call, h.background));
        writeln!(s, "table {id} {how:?} {:?} {:?}", t.and_then(|t| t.ended_at), t.and_then(|t| t.finished_at)).unwrap();
    }
    s
}

fn family(raw: &str) -> Option<String> {
    Model::new(short_model(raw).split('-').next().unwrap_or_default()).map(|m| m.as_str().to_string())
}

fn row_tokens(t: &marspot_agent::types::Tokens) -> RowTokens {
    RowTokens { input: t.input, cache_5m: t.cache_5m, cache_1h: t.cache_1h, cache_read: t.cache_read, output: t.output }
}

/// The reply rows and their total, as `allocation_usage` reads a file
/// it watched from empty.
fn rows_of_readers(path: &Path, bytes: &[u8]) -> String {
    std::fs::write(path, b"").unwrap();
    let mut r = Readers::default();
    assert!(r.read(path, 0).is_empty());
    std::fs::write(path, bytes).unwrap();
    let mut total = RowTokens::default();
    let mut s = String::new();
    for sp in r.read(path, 0) {
        total.add(&sp.tokens);
        writeln!(s, "{} {} {}", sp.at, sp.model, sp.tokens.words()).unwrap();
    }
    s + &format!("total {}\n", total.words())
}

pub(super) fn rows_of_fold(st: &RecordFold, sink: &Sink) -> String {
    let mut s = String::new();
    for r in sink.replies.iter().filter(|r| !r.grew.is_empty()) {
        if let Some(m) = r.model {
            writeln!(s, "{} {} {}", r.at, m.as_str(), row_tokens(&r.grew).words()).unwrap();
        }
    }
    s + &format!("total {}\n", row_tokens(&st.spent).words())
}

/// When the recent totals are asked for: at each reply, and as each
/// leaves the window.
fn moments(sink: &Sink) -> Vec<i64> {
    let mut v: Vec<i64> = sink.replies.iter().flat_map(|r| [r.at as i64, r.at as i64 + RECENT_S, r.at as i64 + RECENT_S + 1]).collect();
    v.sort();
    v.dedup();
    v
}

/// What `pane_tokens` does with the replies: a record of the reply just
/// counted is skipped, each counted one weighs by the API's relative
/// prices, and the total is over the window.
fn recent_of_fold(sink: &Sink, nows: &[i64]) -> String {
    let mut recent = VecDeque::new();
    for r in sink.replies.iter().filter(|r| !r.repeat) {
        let w = r.whole.input as f64 + 1.25 * r.cache_written as f64 + 0.1 * r.whole.cache_read as f64 + 5.0 * r.whole.output as f64;
        if w > 0.0 {
            recent.push_back((r.at as i64, w));
        }
    }
    let mut s = String::new();
    for now in nows {
        while recent.front().is_some_and(|(at, _)| *at < now - RECENT_S) || recent.len() > 4096 {
            recent.pop_front();
        }
        let total: f64 = recent.iter().map(|(_, w)| w).sum();
        writeln!(s, "{now} {:x}", total.to_bits()).unwrap();
    }
    s
}

#[test]
fn claudecode_fold_says_what_the_four_readers_say_of_the_same_records() {
    let tmp = std::env::temp_dir().join(format!("marspot-fold-check-{}.jsonl", std::process::id()));
    for path in records() {
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        let lines: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
        let used = used_in(&text, true);
        let context = format!("tokens={:?} tool={:?} model={:?}", used.tokens, used.had_tool, used.model.map(|m| m.as_str().to_string()));
        let rows = rows_of_readers(&tmp, &bytes);
        let (whole, whole_sink) = folded(&bytes, &[]);
        let nows = moments(&whole_sink);
        let mut cache = TokenCache::default();
        let recent: String = nows.iter().map(|now| format!("{now} {:x}\n", cache.recent(&tmp, *now).to_bits())).collect();
        let mut ids: Vec<&String> = whole_sink.started.keys().chain(whole_sink.ended.keys()).collect();
        ids.sort();
        ids.dedup();
        let tasks = zombie_side(&lines, &ids);
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(!whole.tasks.overflowed, "{name}");
        for cuts in cut_plans(bytes.len()) {
            let (st, sink) = folded(&bytes, &cuts);
            let ctx = format!("tokens={:?} tool={:?} model={:?}", st.ctx_tokens, st.called_tool, st.model.and_then(|m| family(m.as_str())));
            assert_eq!(ctx, context, "{name}: context, {} cuts", cuts.len());
            assert_eq!(rows_of_fold(&st, &sink), rows, "{name}: used rows, {} cuts", cuts.len());
            assert_eq!(recent_of_fold(&sink, &nows), recent, "{name}: token totals, {} cuts", cuts.len());
            assert_eq!(fold_side(&st, &sink, &ids), tasks, "{name}: task table, {} cuts", cuts.len());
        }
    }
    std::fs::remove_file(&tmp).unwrap();
}

/// The end state alone would miss a record read wrong early on that a
/// later one covers up: the context, the turn and the subagent's state
/// are compared after every line too.
#[test]
fn claudecode_fold_agrees_with_the_readers_after_every_line() {
    for path in records() {
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        let (mut st, mut sink) = (RecordFold::from_start(), Sink::default());
        let mut main = ClaudeLog::default();
        main.subs.insert("s".into(), (SubLog::default(), SubMeta::default()));
        let mut end = 0;
        for line in text.split_inclusive('\n') {
            fold(line.as_bytes(), &mut st, &mut sink);
            end += line.len();
            let line = line.trim_end_matches('\n');
            main.feed_main(line);
            main.feed_sub("s", line);
            let used = used_in(&text[..end], true);
            let want = (used.tokens, used.had_tool, used.model.map(|m| m.as_str().to_string()));
            assert_eq!((st.ctx_tokens, st.called_tool, st.model.and_then(|m| family(m.as_str()))), want, "{name} at byte {end}");
            let turn = match st.turn {
                TurnState::Ended { at } => Some(at),
                TurnState::Working | TurnState::Unknown => None,
            };
            let (sub, _) = &main.subs["s"];
            let want = format!("{:?} {} {:?} {} {:?}", main.main.turn_ended, main.main.last_at, sub.first, sub.last_at, sub.last);
            let got = format!("{turn:?} {} {:?} {} {:?}", st.tasks.last_seen, st.tasks.first, st.tasks.last_active, st.tasks.last);
            assert_eq!(got, want, "{name} at byte {end}");
        }
    }
}

#[test]
fn claudecode_fold_check_fixtures_exercise_every_reader() {
    let bytes = std::fs::read(records().pop().unwrap()).unwrap();
    let (st, sink) = folded(&bytes, &[]);
    assert!(sink.replies.iter().any(|r| r.repeat) && sink.replies.iter().any(|r| r.model.is_none()));
    assert!(st.ctx_tokens.is_some() && st.called_tool == Some(true) && !st.spent.is_empty());
    assert!(sink.started.len() >= 3 && sink.ended.len() >= 3 && sink.finished.len() < sink.ended.len());
}
