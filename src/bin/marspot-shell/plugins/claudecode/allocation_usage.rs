//! What a pane's session used, from its transcript and its background
//! agents' (`<session>/subagents/*.jsonl`).
//!
//! Each reply's `message.usage` is priced at the API rate of its model
//! and turned into weeks of the account: about 14 dollars a point for
//! Fable and 28 for the other models, from the control plane's dollars
//! per point on Fable-heavy and Opus-heavy days.  Only bytes appended
//! since the last read are parsed; a file is first read from its end,
//! so history before the shell started is not counted twice (what was
//! counted before is in the event log).

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use marspot_peer::json::{self, Value};

use crate::plugins::allocation::Class;

/// Dollars per point of a week, by class.
const DOLLARS_PER_POINT_RESTRICTED: f64 = 14.0;
const DOLLARS_PER_POINT_PLAIN: f64 = 28.0;

/// The most read from one file in one scan.
const READ_CAP: u64 = 4 << 20;

/// Reply ids remembered per file: one reply is written as several
/// records carrying its usage.
const IDS_KEPT: usize = 64;

/// `(input, output)` dollars per million tokens, first match wins.
const PRICES: [(&str, f64, f64); 10] = [
    ("fable-5-2", 8.0, 40.0),
    ("fable", 10.0, 50.0),
    ("mythos", 10.0, 50.0),
    ("opus-5-5", 4.0, 20.0),
    ("opus", 5.0, 25.0),
    ("sonnet-4", 3.0, 15.0),
    ("sonnet", 2.0, 10.0),
    ("haiku-5-5", 0.1, 0.5),
    ("haiku", 1.0, 5.0),
    ("", 4.0, 20.0),
];

/// One priced reply.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Spent {
    pub at: i64,
    pub class: Class,
    /// Weeks.
    pub amount: f64,
}

pub(super) fn class_of(model: &str) -> Class {
    let m = model.to_ascii_lowercase();
    if m.contains("fable") || m.contains("mythos") { Class::Restricted } else { Class::Plain }
}

/// Weeks one usage object costs on `model`.
pub(super) fn weeks(model: &str, usage: &Value) -> f64 {
    let n = |k: &str| usage.get(k).and_then(Value::i64).unwrap_or(0).max(0) as f64;
    let m = model.to_ascii_lowercase();
    let (_, inp, out) = PRICES.iter().find(|(k, _, _)| m.contains(k)).copied().unwrap_or(PRICES[PRICES.len() - 1]);
    let dollars = (n("input_tokens") * inp + n("cache_creation_input_tokens") * inp * 1.25 + n("cache_read_input_tokens") * inp * 0.1 + n("output_tokens") * out) / 1e6;
    let per_point = if class_of(model) == Class::Restricted { DOLLARS_PER_POINT_RESTRICTED } else { DOLLARS_PER_POINT_PLAIN };
    dollars / per_point / 100.0
}

#[derive(Debug, Default)]
struct Cursor {
    offset: u64,
    /// Reply id and what it was counted at so far.
    ids: VecDeque<(String, f64)>,
}

impl Cursor {
    /// What this record adds: a reply seen before counts only its growth.
    fn add(&mut self, id: &str, amount: f64) -> f64 {
        if let Some((_, was)) = self.ids.iter_mut().find(|(i, _)| i == id) {
            let more = (amount - *was).max(0.0);
            *was = was.max(amount);
            return more;
        }
        if self.ids.len() == IDS_KEPT {
            self.ids.pop_front();
        }
        self.ids.push_back((id.to_string(), amount));
        amount
    }
}

/// Where each watched file was read up to.
#[derive(Debug, Default)]
pub(super) struct Readers {
    files: HashMap<PathBuf, Cursor>,
}

impl Readers {
    /// What the session at `transcript` and its background agents used
    /// since the last call; `keep` collects the files read, for `retain`.
    pub(super) fn read(&mut self, transcript: &Path, keep: &mut Vec<PathBuf>) -> Vec<Spent> {
        let mut out = Vec::new();
        // an agent started while its session was watched is read whole
        let watched = self.files.contains_key(transcript);
        self.read_one(transcript, false, &mut out);
        keep.push(transcript.to_path_buf());
        if let Ok(dir) = fs::read_dir(transcript.with_extension("").join("subagents")) {
            for f in dir.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl")) {
                self.read_one(&f, watched, &mut out);
                keep.push(f);
            }
        }
        out
    }

    pub(super) fn retain(&mut self, keep: &[PathBuf]) {
        self.files.retain(|p, _| keep.contains(p));
    }

    fn read_one(&mut self, path: &Path, from_start: bool, out: &mut Vec<Spent>) {
        let Ok(len) = fs::metadata(path).map(|m| m.len()) else { return };
        if !self.files.contains_key(path) {
            self.files.insert(path.to_path_buf(), Cursor { offset: if from_start { 0 } else { len }, ids: VecDeque::new() });
            if !from_start {
                return;
            }
        }
        let Some(cur) = self.files.get_mut(path) else { return };
        if len < cur.offset {
            cur.offset = len;
            return;
        }
        let Ok(mut f) = fs::File::open(path) else { return };
        if f.seek(SeekFrom::Start(cur.offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.take(READ_CAP).read_to_end(&mut buf).is_err() {
            return;
        }
        let Some(end) = buf.iter().rposition(|b| *b == b'\n') else { return };
        cur.offset += end as u64 + 1;
        for line in String::from_utf8_lossy(&buf[..end]).lines() {
            if let Some(s) = spent(line, cur) {
                out.push(s);
            }
        }
    }
}

fn spent(line: &str, cur: &mut Cursor) -> Option<Spent> {
    if !line.contains(r#""usage""#) || !line.contains(r#""assistant""#) {
        return None;
    }
    let v = json::parse(line).ok()?;
    if v.str_at("type") != Some("assistant") {
        return None;
    }
    let msg = v.get("message")?;
    let model = msg.str_at("model")?;
    if model.starts_with('<') {
        return None;
    }
    let amount = weeks(model, msg.get("usage")?);
    let id = msg.str_at("id").unwrap_or("");
    let amount = if id.is_empty() { amount } else { cur.add(id, amount) };
    let at = marspot::cc_usage::parse_iso_utc(v.str_at("timestamp")?.to_string())?;
    (amount > 0.0).then(|| Spent { at, class: class_of(model), amount })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, model: &str, out: u64, ts: &str) -> String {
        format!(r#"{{"type":"assistant","timestamp":"{ts}","message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":1000,"cache_creation_input_tokens":0,"cache_read_input_tokens":100000,"output_tokens":{out}}}}}}}"#)
    }

    #[test]
    fn allocation_usage_prices_by_model() {
        let u = json::parse(r#"{"input_tokens":1000000,"output_tokens":0}"#).unwrap();
        assert!((weeks("claude-fable-5", &u) - 10.0 / 14.0 / 100.0).abs() < 1e-12);
        assert!((weeks("claude-opus-5-5", &u) - 4.0 / 28.0 / 100.0).abs() < 1e-12);
        assert_eq!(class_of("claude-fable-5-2"), Class::Restricted);
        assert_eq!(class_of("claude-opus-5-5"), Class::Plain);
    }

    #[test]
    fn allocation_usage_reads_only_what_was_appended_including_subagents() {
        let dir = std::env::temp_dir().join(format!("marspot-alloc-usage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("s1/subagents")).unwrap();
        let main = dir.join("s1.jsonl");
        let sub = dir.join("s1/subagents/agent-a.jsonl");
        fs::write(&main, rec("old", "claude-opus-5-5", 500, "2026-11-24T14:00:00Z") + "\n").unwrap();
        fs::write(&sub, "").unwrap();
        let mut r = Readers::default();
        let mut keep = Vec::new();
        assert!(r.read(&main, &mut keep).is_empty(), "history before the first read is not counted");
        let append = |p: &Path, s: String| {
            let mut f = fs::OpenOptions::new().append(true).open(p).unwrap();
            std::io::Write::write_all(&mut f, s.as_bytes()).unwrap();
        };
        append(&main, rec("m1", "claude-opus-5-5", 200, "2026-11-24T14:30:00Z") + "\n");
        append(&main, rec("m1", "claude-opus-5-5", 500, "2026-11-24T14:30:01Z") + "\n");
        append(&main, rec("m2", "claude-opus-5-5", 9, "2026-11-24T14:30:02Z"));
        append(&sub, rec("s1", "claude-fable-5", 100, "2026-11-24T14:30:03Z") + "\n");
        // an agent started after the session was first read is read whole
        fs::write(dir.join("s1/subagents/agent-b.jsonl"), rec("b1", "claude-opus-5-5", 1, "2026-11-24T14:30:04Z") + "\n").unwrap();
        let got = r.read(&main, &mut keep);
        assert_eq!(got.len(), 4, "{got:?}");
        let u = |out: u64, m: &str| weeks(m, &json::parse(&format!(r#"{{"input_tokens":1000,"cache_read_input_tokens":100000,"output_tokens":{out}}}"#)).unwrap());
        assert!((got[0].amount - u(200, "claude-opus-5-5")).abs() < 1e-15);
        assert!((got[1].amount - (u(500, "claude-opus-5-5") - u(200, "claude-opus-5-5"))).abs() < 1e-15, "a reply counts once");
        let fable: Vec<&Spent> = got.iter().filter(|s| s.class == Class::Restricted).collect();
        assert_eq!(fable.len(), 1);
        assert_eq!(fable[0].at, 1_795_530_603);
        append(&main, "\n".to_string());
        let rest = r.read(&main, &mut keep);
        assert_eq!(rest.len(), 1, "the line without its newline is read once it is complete");
        fs::remove_dir_all(&dir).unwrap();
    }
}
