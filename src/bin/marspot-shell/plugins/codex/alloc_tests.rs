//! The codex adapter only watches: refusals and readings are read off
//! a rollout's tail, a refusal is written once, a would-move replays to
//! the same decision, and nothing is moved.
//!
//! The rollout lines are cut down from real ones, with made-up ids.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use super::alloc::{AccountIn, CodexAlloc, PaneIn, Snapshot};
use super::alloc_rollout::{CxTokens, Note, notes_of};
use crate::plugins::allocation::replay::{Why, replay};
use crate::plugins::claudecode::allocation_events::ymd;
use marspot_term::test_env::TempDir;

pub(super) const NOW: i64 = 1_795_530_644;
pub(super) const H: i64 = 3600;
pub(super) const START: i64 = NOW - 2 * H;
const THREAD: &str = "00000000-0000-7000-8000-000000000001";

/// A made-up turn id.
pub(super) fn turn(n: u32) -> String {
    format!("00000000-0000-7000-8000-{n:012x}")
}

pub(super) fn iso(secs: i64) -> String {
    let d = ymd(secs);
    let t = secs.rem_euclid(86_400);
    format!("{}-{}-{}T{:02}:{:02}:{:02}.000Z", &d[..4], &d[4..6], &d[6..], t / 3600, t / 60 % 60, t % 60)
}

pub(super) fn refused_line(at: i64, turn_n: u32) -> String {
    format!(
        r#"{{"timestamp":"{}","ordinal":12,"type":"event_msg","payload":{{"type":"task_complete","turn_id":"{}","last_agent_message":null,"error":{{"message":"You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at 10:32 AM.","codex_error_info":"usage_limit_exceeded"}},"started_at":{},"completed_at":{at},"duration_ms":1427}}}}"#,
        iso(at),
        turn(turn_n),
        at - 1
    )
}

pub(super) fn done_line(at: i64) -> String {
    format!(r#"{{"timestamp":"{}","ordinal":6,"type":"event_msg","payload":{{"type":"task_complete","turn_id":"00000000-0000-7000-8000-00000000000b","last_agent_message":"done","started_at":{},"completed_at":{at},"duration_ms":45154}}}}"#, iso(at), at - 40)
}

/// A sub-agent's refusal, in the parent's turn `turn_n`; `payload` is
/// what its final answer says.
pub(super) fn agent_line(at: i64, turn_n: u32, payload: &str) -> String {
    format!(
        r#"{{"timestamp":"{}","ordinal":51871,"type":"response_item","payload":{{"type":"agent_message","id":"amsg_0001","author":"/root/worker_a","recipient":"/root","content":[{{"type":"input_text","text":"Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker_a\nPayload:\n{payload}"}}],"internal_chat_message_metadata_passthrough":{{"turn_id":"{}","create_time":{at}.268805}}}}}}"#,
        iso(at),
        turn(turn_n)
    )
}

pub(super) const AGENT_REFUSAL: &str = "Agent errored: You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage to purchase more credits or try again at Sep 16th, 2026 2:17 AM.\\n\\nThis agent's turn failed.";

fn agent_refused_line(at: i64, turn_n: u32) -> String {
    agent_line(at, turn_n, AGENT_REFUSAL)
}

pub(super) fn tokens_line(at: i64, used_percent: f64, resets_at: i64) -> String {
    format!(
        r#"{{"timestamp":"{}","ordinal":23,"type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":47712,"cached_input_tokens":37504,"output_tokens":154,"total_tokens":47866}},"last_token_usage":{{"input_tokens":23943,"cached_input_tokens":23808,"cache_write_input_tokens":0,"output_tokens":49,"reasoning_output_tokens":0,"total_tokens":23992}},"model_context_window":258400}},"rate_limits":{{"limit_id":"codex","limit_name":null,"primary":{{"used_percent":{used_percent},"window_minutes":10080,"resets_at":{resets_at}}},"secondary":null,"credits":{{"has_credits":true,"unlimited":false,"balance":"100.0"}},"plan_type":"pro","rate_limit_reached_type":null}}}}}}"#,
        iso(at)
    )
}

#[test]
fn codex_rollout_task_complete_refusal_is_recognised() {
    assert_eq!(notes_of(&refused_line(NOW - 60, 10)), [Note::Refused { at: NOW - 60, agent: false, turn: Some(turn(10)) }]);
    assert_eq!(notes_of(&done_line(NOW - 30)), [Note::Done { at: NOW - 30 }]);
    // a turn's own time, not the stamp a forked copy of it carries
    let copied = refused_line(NOW - 600, 10).replacen(&iso(NOW - 600), &iso(NOW), 1);
    assert_eq!(notes_of(&copied), [Note::Refused { at: NOW - 600, agent: false, turn: Some(turn(10)) }]);
    assert!(notes_of(r#"{"type":"response_item","payload":{"type":"message","content":[{"text":"usage_limit_exceeded"}]}}"#).is_empty());
}

#[test]
fn codex_rollout_sub_agent_refusal_is_recognised() {
    assert_eq!(notes_of(&agent_refused_line(NOW - 10, 12)), [Note::Refused { at: NOW - 10, agent: true, turn: Some(turn(12)) }]);
    // a sub-agent's final answer that quotes the words is no refusal
    let quoting = agent_line(NOW - 10, 12, "The parent log said: Agent errored: You've hit your usage limit. Nothing to do.");
    assert!(notes_of(&quoting).is_empty(), "{quoting}");
    // the words quoted in a message of the session's own are no refusal
    let quoted = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Agent errored: You've hit your usage limit"}]}}"#;
    assert!(notes_of(quoted).is_empty());
}

#[test]
fn codex_rollout_rate_limits_reading_is_parsed() {
    let notes = notes_of(&tokens_line(NOW, 37.0, NOW + 40 * H));
    let tokens = Some(CxTokens { input: 23943, cached: 23808, output: 49 });
    assert_eq!(notes[0], Note::Limits { at: NOW, week: Some((0.37, NOW + 40 * H)), balance: Some(100.0), cap: None, tokens });
    let Note::Used { at, tokens } = notes[1] else { panic!("{notes:?}") };
    assert_eq!((at, tokens), (NOW, CxTokens { input: 23943, cached: 23808, output: 49 }));
    assert!(tokens.weeks() > 0.0 && tokens.weeks() < 1e-4, "{}", tokens.weeks());
    assert_eq!(CxTokens::parse(&tokens.words()), Some(tokens));
    let no_limits = tokens_line(NOW, 1.0, NOW).replace(r#""rate_limits":{"limit_id""#, r#""rate_limits":null,"x":{"limit_id""#);
    assert!(matches!(notes_of(&no_limits)[..], [Note::Used { .. }]), "{:?}", notes_of(&no_limits));
}

pub(super) struct World {
    _tmp: TempDir,
    pub dir: PathBuf,
    pub rollout: PathBuf,
}

pub(super) fn world(name: &str) -> World {
    let tmp = TempDir::new(name);
    let dir = tmp.join("alloc_events");
    let rollout = tmp.join(format!("rollout-2026-11-24T10-00-00-{THREAD}.jsonl"));
    fs::write(&rollout, "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/w/a\"}}\n").unwrap();
    World { _tmp: tmp, dir, rollout }
}

pub(super) fn append(path: &Path, lines: &[String]) {
    let mut f = fs::OpenOptions::new().append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

pub(super) fn accounts(now: i64) -> Vec<AccountIn> {
    vec![
        AccountIn { profile: 1, week_used: 0.30, week_renews: now + 40 * H, spark: None, open: true, measured_at: now - 300, supply: Vec::new() },
        AccountIn { profile: 2, week_used: 0.10, week_renews: now + 140 * H, spark: None, open: true, measured_at: now - 300, supply: Vec::new() },
    ]
}

pub(super) fn snap(w: &World, now: i64, idle: bool) -> Snapshot {
    let pane = PaneIn { sid: 7, profile: 1, started_at: START, rollout: Some(w.rollout.clone()), idle, helper: false, restricted: false, peer: Default::default() };
    Snapshot { now, feed_at: Some(now), accounts: accounts(now), panes: vec![pane] }
}

pub(super) fn rows(dir: &Path, kind: &str) -> Vec<Vec<String>> {
    let mut names: Vec<PathBuf> = fs::read_dir(dir).map(|r| r.map(|e| e.unwrap().path()).collect()).unwrap_or_default();
    names.sort();
    names
        .iter()
        .flat_map(|p| fs::read_to_string(p).unwrap().lines().map(|l| l.split('\t').map(str::to_string).collect::<Vec<_>>()).collect::<Vec<_>>())
        .filter(|f| f[3] == kind)
        .collect()
}

#[test]
fn codex_alloc_refusals_before_the_tui_started_are_ignored() {
    let w = world("codex-alloc-before");
    append(&w.rollout, &[refused_line(START - 600, 1), agent_refused_line(START - 5, 2)]);
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w, NOW, false));
    assert!(rows(&w.dir, "refused").is_empty(), "a refusal of an earlier codex is history");
    assert!(rows(&w.dir, "would_move").is_empty(), "nor does it make the pane move");
    append(&w.rollout, &[refused_line(NOW + 5, 3)]);
    a.observe(&snap(&w, NOW + 10, false));
    let got = rows(&w.dir, "refused");
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0][1].as_str(), got[0][2].as_str(), got[0][4].as_str()), ("codex", "7", "1"));
    assert_eq!(got[0][6], format!("at={} how=turn turn={}", NOW + 5, turn(3)));
}

/// A refusal is one row however often it is seen: read again, heard
/// from the peerbus too, and after a restart that reads the tail again.
#[test]
fn codex_alloc_refused_row_is_written_once_per_refusal() {
    let w = world("codex-alloc-once");
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w, NOW, false));
    // a sub-agent's refusal names the turn the parent's refusal ends
    append(&w.rollout, &[agent_refused_line(NOW + 3, 4), refused_line(NOW + 4, 4)]);
    let mut s = snap(&w, NOW + 6, false);
    s.panes[0].peer.refused = Some((NOW + 6, None));
    a.observe(&s);
    a.observe(&s);
    assert_eq!(rows(&w.dir, "refused").len(), 1, "one refused turn, three reports of it");
    let mut again = CodexAlloc::new(w.dir.clone());
    again.observe(&s);
    assert_eq!(rows(&w.dir, "refused").len(), 1, "a restart does not write it again");
    // a good turn ends it; the next refusal is a new one
    append(&w.rollout, &[done_line(NOW + 600), refused_line(NOW + 1200, 5)]);
    again.observe(&snap(&w, NOW + 1210, false));
    assert_eq!(rows(&w.dir, "refused").len(), 2);
}

/// Every would-move carries all it was decided from: read back and
/// decided again it comes out the same.
#[test]
fn codex_alloc_would_move_replays_to_the_same_decision() {
    let w = world("codex-alloc-replay");
    let mut a = CodexAlloc::new(w.dir.clone());
    a.observe(&snap(&w, NOW, false));
    append(&w.rollout, &[refused_line(NOW + 5, 6)]);
    a.observe(&snap(&w, NOW + 10, false));
    // an account a pane's rollout reads as nearly spent, the pane resting there
    let w2 = world("codex-alloc-replay-rest");
    let mut b = CodexAlloc::new(w2.dir.clone());
    append(&w2.rollout, &[tokens_line(NOW - 60, 99.5, NOW + 40 * H)]);
    let mut s = snap(&w2, NOW, true);
    s.panes.push(PaneIn { sid: 9, ..s.panes[0].clone() });
    s.panes[1].rollout = None;
    b.observe(&s);
    let mut lines = rows(&w.dir, "would_move");
    lines.extend(rows(&w2.dir, "would_move"));
    let whys: Vec<String> = lines.iter().map(|f| f[6].split(' ').nth(1).unwrap().to_string()).collect();
    assert!(whys.contains(&"why=out".to_string()) && whys.contains(&"why=leave_early".to_string()), "{whys:?}");
    for f in &lines {
        let detail = &f[6];
        let to: u8 = detail.split(' ').next().unwrap().strip_prefix("to=P").unwrap().parse().unwrap();
        let why = Why::from_word(detail.split(' ').nth(1).unwrap().strip_prefix("why=").unwrap()).unwrap();
        assert_eq!(replay(detail), Some(Some((to, why))), "{detail}");
        assert_eq!(to, 2);
    }
}

#[test]
fn codex_alloc_heads_up_once_per_account_week() {
    let w = world("codex-alloc-told");
    let mut a = CodexAlloc::new(w.dir.clone());
    append(&w.rollout, &[tokens_line(NOW - 60, 99.8, NOW + 40 * H)]);
    let (_, tell) = a.observe(&snap(&w, NOW, false));
    assert_eq!(tell.len(), 1, "{tell:?}");
    assert_eq!((tell[0].sid, tell[0].profile), (7, 1));
    assert!(a.observe(&snap(&w, NOW + 60, false)).1.is_empty());
    assert!(CodexAlloc::new(w.dir.clone()).observe(&snap(&w, NOW + 120, false)).1.is_empty(), "nor after a restart");
    assert!(super::alloc_wire::body(1, 0.4).contains("P1"));
}

/// An account with a reset left counts what it brings back before it
/// ends; one with none reads as it always did.
#[test]
fn a_reset_left_before_its_end_adds_to_the_account() {
    use marspot_agent::types::reset::{Supply, WeekCut};
    let plain = &accounts(NOW)[0];
    let before = super::alloc_decide::quant(plain, None, NOW, 0.01);
    assert_eq!(before, super::alloc_decide::quant(&AccountIn { supply: Vec::new(), ..plain.clone() }, None, NOW, 0.01));
    let reset = Supply { week: true, window: false, gives_back_week: false, week_cut: WeekCut::Nothing, moves_week: true, requires_limit: false, starts_h: 0.0, ends_h: Some(10.0), left: 1, usable_below_left: Some(0.8) };
    let with = super::alloc_decide::quant(&AccountIn { supply: vec![reset], ..plain.clone() }, None, NOW, 0.01);
    // 0.70 left at 0.01 an hour, used at its end in 10 h with 0.60 left: 0.40 comes back
    assert!((before.r - 0.7).abs() < 1e-9 && (with.r - 1.1).abs() < 1e-9, "{with:?}");
    assert!((with.t - (10.0 + 168.0)).abs() < 1e-9, "the week moves to a week after the use");
}
