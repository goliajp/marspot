//! What goes into the audit log, and how it is kept short enough to
//! hold the past: one caller's run of the same op on the same pane
//! within a second is one line with a count, and refusals are one line
//! per caller and error per ten seconds.  Written on a thread of its
//! own (`audit_log`), never on the main loop.

use std::collections::HashMap;

use marspot_peer::json::{Value, obj};

/// How close two records must be to share a line.
pub(super) const MERGE_MS: u64 = 1_000;
/// How often one caller's refusals with one error get a line.
pub(super) const REJECT_EVERY_MS: u64 = 10_000;
/// Open groups at most; the oldest is written out to make room.
const MAX_OPEN: usize = 64;

/// One request that changed something (or tried to).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rec {
    pub at_ms: u64,
    pub caller: String,
    pub pid: Option<i32>,
    pub exe: String,
    pub op: &'static str,
    pub pane: String,
    pub sid: u64,
    /// What was typed, when anything was; dropped from the line when the
    /// terminal had echo off (only its length and hash are kept).
    pub content: Option<Vec<u8>>,
    pub echo_off: bool,
    pub result: String,
}

#[derive(Debug)]
struct Group {
    rec: Rec,
    first_ms: u64,
    count: u32,
    bytes: usize,
}

#[derive(Debug)]
struct Refusals {
    window_ms: u64,
    count: u32,
    op: &'static str,
    pane: String,
}

#[derive(Default)]
pub(crate) struct Merger {
    open: HashMap<(String, &'static str, u64), Group>,
    refused: HashMap<(String, String), Refusals>,
}

/// The first 80 characters of `bytes`, control characters left out.
fn preview(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().filter(|c| !c.is_control()).take(80).collect()
}

fn line(g: &Group, utc: &dyn Fn(u64) -> String) -> String {
    let r = &g.rec;
    let mut kv = vec![
        ("at".to_string(), Value::from(utc(g.first_ms))),
        ("caller".into(), r.caller.as_str().into()),
        ("pid".into(), r.pid.map_or(Value::Null, |p| (p as i64).into())),
        ("exe".into(), r.exe.as_str().into()),
        ("op".into(), r.op.into()),
        ("pane".into(), r.pane.as_str().into()),
        ("sid".into(), (r.sid as i64).into()),
        ("count".into(), (g.count as i64).into()),
        ("bytes".into(), (g.bytes as i64).into()),
    ];
    if let Some(c) = &r.content {
        kv.push(("sha256_16".into(), marspot_peer::control::sha256::hex_prefix(c, 16).into()));
        if !r.echo_off {
            kv.push(("text".into(), preview(c).into()));
        }
    }
    if r.echo_off {
        kv.push(("echo_off".into(), true.into()));
    }
    kv.push(("result".into(), r.result.as_str().into()));
    Value::Obj(kv).to_json()
}

impl Merger {
    /// Take in `rec`; the lines to write now.
    pub fn add(&mut self, rec: Rec, utc: &dyn Fn(u64) -> String) -> Vec<String> {
        let mut out = self.flush_due(rec.at_ms, utc);
        let key = (rec.caller.clone(), rec.op, rec.sid);
        let bytes = rec.content.as_ref().map_or(0, Vec::len);
        if let Some(g) = self.open.get_mut(&key) {
            g.count += 1;
            g.bytes += bytes;
            g.rec = rec;
            return out;
        }
        if self.open.len() >= MAX_OPEN
            && let Some(oldest) = self.open.iter().min_by_key(|(_, g)| g.rec.at_ms).map(|(k, _)| k.clone())
            && let Some(g) = self.open.remove(&oldest)
        {
            out.push(line(&g, utc));
        }
        let first_ms = rec.at_ms;
        self.open.insert(key, Group { rec, first_ms, count: 1, bytes });
        out
    }

    /// A refused request; the lines to write now.
    pub fn refused(&mut self, caller: &str, code: &str, op: &'static str, pane: &str, now: u64, utc: &dyn Fn(u64) -> String) -> Vec<String> {
        let mut out = self.flush_due(now, utc);
        let key = (caller.to_string(), code.to_string());
        if let Some(r) = self.refused.get_mut(&key) {
            r.count += 1;
            r.op = op;
            r.pane = pane.to_string();
            return out;
        }
        if self.refused.len() >= MAX_OPEN {
            let all: Vec<_> = self.refused.drain().collect();
            out.extend(all.iter().filter(|(_, r)| r.count > 0).map(|(k, r)| refusal_line(k, r, utc)));
        }
        out.push(refusal_line(&key, &Refusals { window_ms: now, count: 1, op, pane: pane.to_string() }, utc));
        self.refused.insert(key, Refusals { window_ms: now, count: 0, op, pane: pane.to_string() });
        out
    }

    /// When something held has to be written, if anything is held.
    pub fn due(&self) -> Option<u64> {
        let open = self.open.values().map(|g| g.rec.at_ms + MERGE_MS);
        let refused = self.refused.values().map(|r| r.window_ms + REJECT_EVERY_MS);
        open.chain(refused).min()
    }

    /// The lines whose time has come.
    pub fn flush_due(&mut self, now: u64, utc: &dyn Fn(u64) -> String) -> Vec<String> {
        let mut out = Vec::new();
        let due: Vec<_> = self.open.iter().filter(|(_, g)| now >= g.rec.at_ms + MERGE_MS).map(|(k, _)| k.clone()).collect();
        let mut groups: Vec<Group> = due.into_iter().filter_map(|k| self.open.remove(&k)).collect();
        groups.sort_by_key(|g| g.first_ms);
        out.extend(groups.iter().map(|g| line(g, utc)));
        let closed: Vec<_> = self.refused.iter().filter(|(_, r)| now >= r.window_ms + REJECT_EVERY_MS).map(|(k, _)| k.clone()).collect();
        for k in closed {
            if let Some(r) = self.refused.remove(&k)
                && r.count > 0
            {
                out.push(refusal_line(&k, &r, utc));
            }
        }
        out
    }

    /// Everything held, as lines.
    pub fn flush_all(&mut self, utc: &dyn Fn(u64) -> String) -> Vec<String> {
        self.flush_due(u64::MAX - REJECT_EVERY_MS, utc)
    }
}

fn refusal_line((caller, code): &(String, String), r: &Refusals, utc: &dyn Fn(u64) -> String) -> String {
    obj([
        ("at", utc(r.window_ms).into()),
        ("caller", caller.as_str().into()),
        ("op", r.op.into()),
        ("pane", r.pane.as_str().into()),
        ("refused", code.as_str().into()),
        ("count", (r.count as i64).into()),
    ])
    .to_json()
}
