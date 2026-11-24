//! How much each session has asked of its account lately, from the
//! `usage` its transcript records with every response.
//!
//! Read incrementally: each pass reads only what was appended since the
//! last one, so a 700 MB transcript costs what its newest turn costs.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use marspot_peer::json::{self, Value};

use super::transcript::{record_type, top_level_str};

/// Responses within this long count.
pub(super) const RECENT_S: i64 = 90 * 60;
/// How far back a transcript seen for the first time is read: a busy
/// session writes about 32 KiB in six minutes.
const SEED: u64 = 4 << 20;
/// At most this much new text in one pass; a burst beyond it is skipped.
const STEP: u64 = 4 << 20;
const MAX_RESPONSES: usize = 4096;

/// What a response weighs, in input-token equivalents, by the API's
/// relative prices: a cache read is a tenth of an input token, a cache
/// write a quarter more, an output token five.
fn weight(usage: &Value) -> f64 {
    let n = |k: &str| usage.get(k).and_then(Value::i64).unwrap_or(0).max(0) as f64;
    n("input_tokens") + 1.25 * n("cache_creation_input_tokens") + 0.1 * n("cache_read_input_tokens") + 5.0 * n("output_tokens")
}

#[derive(Default)]
struct Tokens {
    offset: u64,
    /// The offset is at the start of a line.
    aligned: bool,
    /// One response is several records carrying the same usage.
    last_id: String,
    recent: VecDeque<(i64, f64)>,
}

impl Tokens {
    fn take(&mut self, line: &str) {
        if record_type(line) != Some("assistant") || !line.contains("\"usage\"") {
            return;
        }
        let Some(at) = top_level_str(line, "timestamp").and_then(|t| marspot::cc_usage::parse_iso_utc(t.to_string())) else {
            return;
        };
        let Ok(v) = json::parse(line) else { return };
        let Some(usage) = v.at(&["message", "usage"]) else { return };
        let id = v.at(&["message", "id"]).and_then(Value::str).unwrap_or("");
        if !id.is_empty() && id == self.last_id {
            return;
        }
        self.last_id = id.to_string();
        let w = weight(usage);
        if w > 0.0 {
            self.recent.push_back((at, w));
        }
    }
}

#[derive(Default)]
pub(super) struct TokenCache {
    files: HashMap<PathBuf, Tokens>,
}

impl TokenCache {
    /// The weight of the responses in `path` within `RECENT_S` of `now`.
    pub(super) fn recent(&mut self, path: &Path, now: i64) -> f64 {
        if !self.files.contains_key(path) {
            self.files.insert(path.to_path_buf(), Tokens::default());
        }
        let t = self.files.get_mut(path).expect("just inserted");
        let len = path.metadata().map_or(0, |m| m.len());
        if len < t.offset {
            *t = Tokens::default();
        }
        if t.offset == 0 && !t.aligned {
            t.offset = len.saturating_sub(SEED);
            t.aligned = t.offset == 0;
        }
        if len - t.offset > STEP {
            (t.offset, t.aligned) = (len - STEP, false);
        }
        if len > t.offset {
            read_new(path, t, len);
        }
        while t.recent.front().is_some_and(|(at, _)| *at < now - RECENT_S) || t.recent.len() > MAX_RESPONSES {
            t.recent.pop_front();
        }
        t.recent.iter().map(|(_, w)| w).sum()
    }

    pub(super) fn retain(&mut self, keep: impl Fn(&PathBuf) -> bool) {
        self.files.retain(|p, _| keep(p));
    }
}

/// Take the whole lines between the offset and `len`.
fn read_new(path: &Path, t: &mut Tokens, len: u64) {
    let Ok(mut f) = File::open(path) else { return };
    if f.seek(SeekFrom::Start(t.offset)).is_err() {
        return;
    }
    let mut buf = Vec::with_capacity((len - t.offset) as usize);
    if f.take(len - t.offset).read_to_end(&mut buf).is_err() {
        return;
    }
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else { return };
    let mut text = &buf[..end];
    if !t.aligned {
        // landed inside a record: it starts after the next newline
        let Some(nl) = text.iter().position(|b| *b == b'\n') else {
            t.offset += end as u64 + 1;
            t.aligned = true;
            return;
        };
        text = &text[nl + 1..];
    }
    for line in text.split(|b| *b == b'\n') {
        if let Ok(line) = std::str::from_utf8(line) {
            t.take(line);
        }
    }
    t.offset += end as u64 + 1;
    t.aligned = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn said(id: &str, at: &str, input: u64, output: u64) -> String {
        format!(
            r#"{{"parentUuid":"x","message":{{"id":"{id}","type":"message","usage":{{"input_tokens":{input},"cache_read_input_tokens":1000,"output_tokens":{output}}}}},"type":"assistant","timestamp":"{at}"}}"#
        )
    }

    const NOW: i64 = 1_795_530_644; // 2026-11-24T14:30:44Z

    #[test]
    fn rebalance_pane_tokens_count_each_recent_response_once_and_read_only_what_is_new() {
        let p = std::env::temp_dir().join(format!("marspot-pane-tokens-{}.jsonl", std::process::id()));
        let user = r#"{"type":"user","message":{"content":"\"usage\":{\"input_tokens\":9}"},"timestamp":"2026-11-24T14:00:00Z"}"#;
        let old = said("m0", "2026-11-24T12:00:00Z", 100, 100);
        let a = said("m1", "2026-11-24T14:00:00Z", 10, 20);
        std::fs::write(&p, format!("{old}\n{user}\n{a}\n{a}\n")).unwrap();
        let mut c = TokenCache::default();
        // 10 + 0.1 * 1000 + 5 * 20, once: the second record is the same response
        assert_eq!(c.recent(&p, NOW), 210.0);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        let b = said("m2", "2026-11-24T14:20:00Z", 0, 0);
        write!(f, "{b}\n{}", &said("m3", "2026-11-24T14:21:00Z", 1, 1)[..30]).unwrap();
        assert_eq!(c.recent(&p, NOW), 310.0, "a half-written record waits for its end");
        assert_eq!(c.recent(&p, NOW + 3600), 100.0, "the first response has aged out");
        std::fs::write(&p, format!("{a}\n")).unwrap();
        assert_eq!(c.recent(&p, NOW), 210.0, "a file rewritten shorter is read again");
        std::fs::remove_file(&p).unwrap();
    }
}
