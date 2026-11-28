//! What a codex session's rollout says about its account: a turn
//! refused for usage, a turn that ended without that, the account's
//! readings, and the tokens each reply used.
//!
//! Only the end of a file is read.  The first look at a file reads its
//! last `FIRST_LOOK` bytes; after that only what was appended, and at
//! most `READ_CAP` of it, so the cost of a pass does not grow with the
//! session.  A line still being written is read once it is whole.
//!
//! Each record is dated by when it happened, not when it was written:
//! a forked session copies the turns it came from into its new file,
//! each stamped with the moment of the copy, while a turn's own
//! `completed_at` (and a message's `create_time`) keep their real time.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use marspot_peer::json::{self, Value};

use crate::agentsvc::accounts::wallet::Cap;

use crate::plugins::claudecode::allocation_events::{DAY, ymd};

/// How much of a file's end the first look reads.
const FIRST_LOOK: u64 = 256 * 1024;

/// The most one later read takes; anything before it is skipped.
const READ_CAP: u64 = 256 * 1024;

/// What a sub-agent's refusal reads like in its parent's record: its
/// final answer's payload, the text after the `Payload:` header line.
const AGENT_REFUSED: &str = "Agent errored: You've hit your usage limit";

const PAYLOAD: &str = "\nPayload:\n";

/// List prices of the default model, dollars per million tokens: input,
/// cached input, output.
const PRICE: (f64, f64, f64) = (10.0, 1.0, 50.0);

/// Dollars of use in a week of an account: sixteen dollars a point, as
/// one account's ledger and its rollouts both gave for 2026-11-17 to 24.
const DOLLARS_PER_WEEK: f64 = 1600.0;

/// The tokens of one or more replies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CxTokens {
    /// Input, cached input included.
    pub input: u64,
    pub cached: u64,
    pub output: u64,
}

impl CxTokens {
    fn of(v: &Value) -> CxTokens {
        let n = |k: &str| v.get(k).and_then(num).unwrap_or(0.0).max(0.0) as u64;
        CxTokens { input: n("input_tokens"), cached: n("cached_input_tokens"), output: n("output_tokens") }
    }

    pub(super) fn add(&mut self, o: &CxTokens) {
        self.input += o.input;
        self.cached += o.cached;
        self.output += o.output;
    }

    /// What they cost in weeks of an account.
    pub(super) fn weeks(&self) -> f64 {
        self.dollars() / DOLLARS_PER_WEEK
    }

    /// What they cost at list price.
    pub(super) fn dollars(&self) -> f64 {
        let fresh = self.input.saturating_sub(self.cached) as f64;
        (fresh * PRICE.0 + self.cached as f64 * PRICE.1 + self.output as f64 * PRICE.2) / 1e6
    }

    pub(super) fn words(&self) -> String {
        format!("in={} cached={} out={}", self.input, self.cached, self.output)
    }

    pub(super) fn parse(detail: &str) -> Option<CxTokens> {
        let get = |k: &str| detail.split(' ').find_map(|w| w.strip_prefix(k)?.strip_prefix('='))?.parse().ok();
        Some(CxTokens { input: get("in")?, cached: get("cached")?, output: get("out")? })
    }
}

/// One record that matters to the allocation.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Note {
    /// A turn was refused for usage; `agent` when it was a sub-agent's.
    /// `turn` is the turn's id (a sub-agent's names its parent's turn).
    Refused { at: i64, agent: bool, turn: Option<String> },
    /// A turn ended without a usage refusal.
    Done { at: i64 },
    /// The account's readings: a week fraction used and when it renews,
    /// the paid balance, a spending cap, and the tokens of the reply
    /// the reading came with.
    Limits { at: i64, week: Option<(f64, i64)>, balance: Option<f64>, cap: Option<Cap>, tokens: Option<CxTokens> },
    Used { at: i64, tokens: CxTokens },
    /// A turn started on this model.
    Model { model: String },
}

/// The notes in one rollout line; most lines carry none and are not parsed.
pub(super) fn notes_of(line: &str) -> Vec<Note> {
    let kinds = [r#""task_complete""#, r#""token_count""#, AGENT_REFUSED, r#""turn_context""#];
    if !kinds.iter().any(|k| line.contains(k)) {
        return Vec::new();
    }
    let Ok(v) = json::parse(line) else { return Vec::new() };
    let Some(p) = v.get("payload") else { return Vec::new() };
    if v.str_at("type") == Some("turn_context") {
        return p.str_at("model").filter(|m| !m.is_empty()).map(|m| vec![Note::Model { model: m.to_string() }]).unwrap_or_default();
    }
    let stamp = v.str_at("timestamp").and_then(|t| marspot::cc_usage::parse_iso_utc(t.to_string()));
    match p.str_at("type") {
        Some("task_complete") => {
            let Some(at) = p.get("completed_at").and_then(num).map(|t| t as i64).or(stamp) else { return Vec::new() };
            let refused = p.at(&["error", "codex_error_info"]).and_then(Value::str) == Some("usage_limit_exceeded");
            let turn = p.str_at("turn_id").map(str::to_string);
            vec![if refused { Note::Refused { at, agent: false, turn } } else { Note::Done { at } }]
        }
        Some("agent_message") => {
            let said = p.get("content").map(Value::arr).unwrap_or_default().iter().any(|c| c.str_at("text").is_some_and(agent_refused));
            let meta = p.get("internal_chat_message_metadata_passthrough");
            let at = meta.and_then(|m| m.get("create_time")).and_then(num).map(|t| t as i64).or(stamp);
            let turn = meta.and_then(|m| m.str_at("turn_id")).map(str::to_string);
            match at {
                Some(at) if said => vec![Note::Refused { at, agent: true, turn }],
                _ => Vec::new(),
            }
        }
        Some("token_count") => {
            let Some(at) = stamp else { return Vec::new() };
            let mut out = Vec::new();
            let tokens = p.at(&["info", "last_token_usage"]).map(CxTokens::of);
            if let Some(r) = p.get("rate_limits").filter(|r| !matches!(r, Value::Null)) {
                out.push(Note::Limits { at, week: week_of(r), balance: balance_of(r), cap: Cap::of(r, CAP_KEYS), tokens });
            }
            if let Some(tokens) = tokens {
                out.push(Note::Used { at, tokens });
            }
            out
        }
        _ => Vec::new(),
    }
}

/// A sub-agent's final answer that is its refusal, not one that quotes it.
fn agent_refused(text: &str) -> bool {
    text.starts_with("Message Type: FINAL_ANSWER\n") && text.split_once(PAYLOAD).is_some_and(|(_, said)| said.starts_with(AGENT_REFUSED))
}

/// The week window of a `rate_limits` record, whichever slot holds it.
fn week_of(r: &Value) -> Option<(f64, i64)> {
    ["primary", "secondary"].iter().filter_map(|k| r.get(k)).find_map(|w| {
        (w.get("window_minutes").and_then(num)? as i64 == 10_080).then_some(())?;
        Some((w.get("used_percent").and_then(num)? / 100.0, w.get("resets_at").and_then(num)? as i64))
    })
}

/// The paid balance of a `rate_limits` record; `None` when it is not a
/// number, never zero for it.
fn balance_of(r: &Value) -> Option<f64> {
    match r.at(&["credits", "balance"])? {
        Value::Str(s) => s.parse().ok(),
        Value::Num(n) => Some(*n),
        _ => None,
    }
}

/// The rollout's spelling of the cap fields.
const CAP_KEYS: [&str; 5] = ["individual_limit", "spend_control_reached", "rate_limit_reached_type", "limit", "remaining_percent"];

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Num(n) => Some(*n),
        _ => None,
    }
}

/// Where each watched rollout has been read to.
#[derive(Debug, Default)]
pub(super) struct Tails {
    at: HashMap<PathBuf, u64>,
}

impl Tails {
    /// The notes appended to `path` since the last read, and whether
    /// this was the first look at it.
    pub(super) fn read(&mut self, path: &Path) -> (Vec<Note>, bool) {
        let len = std::fs::metadata(path).map_or(0, |m| m.len());
        let was = self.at.get(path).copied().filter(|off| *off <= len);
        let first = was.is_none();
        let from = was.unwrap_or_else(|| len.saturating_sub(FIRST_LOOK)).max(len.saturating_sub(READ_CAP));
        if Some(len) == was {
            return (Vec::new(), false);
        }
        let at_start = from == 0 || Some(from) == was;
        let Some((text, end)) = whole_lines(path, from, len, at_start) else { return (Vec::new(), first) };
        self.at.insert(path.to_path_buf(), end);
        (text.lines().flat_map(notes_of).collect(), first)
    }

    pub(super) fn retain(&mut self, keep: &HashSet<PathBuf>) {
        self.at.retain(|p, _| keep.contains(p));
    }
}

/// The whole lines between `from` and `len`, and where they end.  A
/// read that does not start at the start of a line drops the part line
/// it starts in.
fn whole_lines(path: &Path, from: u64, len: u64, at_start: bool) -> Option<(String, u64)> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    f.take(len - from).read_to_end(&mut buf).ok()?;
    let start = if at_start { 0 } else { buf.iter().position(|b| *b == b'\n').map_or(buf.len(), |i| i + 1) };
    let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1).max(start);
    Some((String::from_utf8_lossy(&buf[start..end]).into_owned(), from + end as u64))
}

/// The rollout of thread `thread` under `home`, by its file name.
/// A thread id is a UUIDv7, its first 48 bits the millisecond it was
/// made, and its file sits in that day's directory in local time: so
/// only that day's directory and the one either side are looked in.
pub(super) fn rollout_of_thread(home: &Path, thread: &str) -> Option<PathBuf> {
    let made = made_at(thread)?;
    let suffix = format!("-{thread}.jsonl");
    (-1..=1).find_map(|d| {
        let day = ymd(made + d * DAY);
        let dir = home.join("sessions").join(&day[..4]).join(&day[4..6]).join(&day[6..]);
        std::fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).find(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)))
    })
}

/// When a UUIDv7 id was made, unix seconds.
pub(super) fn made_at(id: &str) -> Option<i64> {
    let hex: String = id.chars().filter(|c| *c != '-').collect();
    // the id comes from another process: all of it must be hex
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) || hex.as_bytes()[12] != b'7' {
        return None;
    }
    Some(i64::from_str_radix(&hex[..12], 16).ok()? / 1000)
}
