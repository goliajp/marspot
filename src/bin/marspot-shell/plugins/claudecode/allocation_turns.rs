//! What a pane's session has running past its turns, from its
//! transcript: background agents and shells, compactions, and when a
//! person last wrote to it.
//!
//! A task starts when a tool result says so (`toolUseResult` with
//! `isAsync` and `agentId`, `backgroundTaskId`, a resumed or revived
//! agent, or a monitor's `Monitor started (task …`) and ends at a
//! `<task-notification>` naming it with a status.  A background shell
//! that a foreground command was moved into often never gets that
//! notice, so a shell the transcript still has open counts only while
//! claude has a process under it; an agent runs inside claude and has
//! no process of its own to look for.
//!
//! Only bytes appended since the last read are parsed.  A transcript is
//! first read from up to `HISTORY` bytes before its end, so a task begun
//! before the shell started is known; compactions in that history were
//! written before and are not written again.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use marspot_peer::json::{self, Value};

/// How far back a transcript is read the first time.
const HISTORY: u64 = 4 << 20;
/// The most read from one transcript in one scan.
const READ_CAP: u64 = 4 << 20;
/// Open tasks kept per transcript; past that the oldest is let go.
const LIVE_CAP: usize = 64;
/// Statuses a task notification ends a task with.
const END_STATUS: [&str; 4] = ["completed", "failed", "killed", "stopped"];

#[derive(Clone, Debug, PartialEq)]
struct Task {
    id: String,
    at: i64,
    /// Runs as a process under claude, not inside it.
    shell: bool,
}

/// A compaction: when, the context before it, and the context of the
/// first reply after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Compacted {
    pub at: i64,
    pub before: u64,
    pub after: u64,
    pub auto: bool,
}

#[derive(Debug, Default)]
struct Watch {
    offset: u64,
    /// The claude the open tasks belong to.
    pid: i32,
    live: Vec<Task>,
    /// A compaction whose first reply after has not been read.
    pending: Option<(i64, u64, bool)>,
    prompt_at: Option<i64>,
}

/// What a session has open when its turn ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Open {
    pub tasks: usize,
    /// Since the oldest of them started, seconds.
    pub oldest_s: Option<i64>,
    /// When a person last wrote to it.
    pub prompt_at: Option<i64>,
}

#[derive(Debug, Default)]
pub(super) struct Turns {
    watches: HashMap<PathBuf, Watch>,
}

impl Turns {
    /// Read what `transcript` gained; `pid` is the claude writing it, and
    /// a new one ends the tasks of the one before.  The compactions read.
    pub(super) fn read(&mut self, transcript: &Path, pid: i32) -> Vec<Compacted> {
        let mut out = Vec::new();
        let Ok(len) = fs::metadata(transcript).map(|m| m.len()) else { return out };
        let first = !self.watches.contains_key(transcript);
        let w = self.watches.entry(transcript.to_path_buf()).or_default();
        if first {
            w.offset = len.saturating_sub(HISTORY);
            w.pid = pid;
        } else if w.pid != pid {
            w.pid = pid;
            w.live.clear();
        }
        if len < w.offset {
            w.offset = len;
        }
        let skip_partial = first && w.offset > 0;
        follow(transcript, len, w, skip_partial, if first { None } else { Some(&mut out) });
        out
    }

    /// What is open in `transcript` as of `now`; `child`: claude has a
    /// process under it that is not resting.
    pub(super) fn open(&self, transcript: &Path, child: bool, now: i64) -> Open {
        let Some(w) = self.watches.get(transcript) else { return Open::default() };
        let counted = w.live.iter().filter(|t| child || !t.shell);
        let (tasks, oldest) = counted.fold((0, None::<i64>), |(n, o), t| (n + 1, Some(o.map_or(t.at, |o| o.min(t.at)))));
        Open { tasks, oldest_s: oldest.map(|o| (now - o).max(0)), prompt_at: w.prompt_at }
    }

    /// Forget every transcript not in `keep`.
    pub(super) fn retain(&mut self, keep: impl Fn(&Path) -> bool) {
        self.watches.retain(|p, _| keep(p));
    }
}

fn follow(path: &Path, len: u64, w: &mut Watch, skip_partial: bool, mut out: Option<&mut Vec<Compacted>>) {
    if len == w.offset {
        return;
    }
    let Ok(mut f) = fs::File::open(path) else { return };
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(w.offset)).is_err() || f.take(READ_CAP).read_to_end(&mut buf).is_err() {
        return;
    }
    let Some(end) = buf.iter().rposition(|b| *b == b'\n') else { return };
    let mut from = 0;
    if skip_partial {
        from = buf.iter().position(|b| *b == b'\n').map_or(end, |i| i + 1);
    }
    w.offset += end as u64 + 1;
    if from >= end {
        return;
    }
    for line in String::from_utf8_lossy(&buf[from..end]).lines() {
        take(line, w, out.as_deref_mut());
    }
}

fn take(line: &str, w: &mut Watch, out: Option<&mut Vec<Compacted>>) {
    const KINDS: [&str; 4] = ["\"user\"", "\"assistant\"", "compact_boundary", "queue-operation"];
    if !KINDS.iter().any(|k| line.contains(k)) {
        return;
    }
    let Ok(v) = json::parse(line) else { return };
    if v.get("isSidechain").and_then(Value::bool) == Some(true) {
        return;
    }
    let Some(at) = v.str_at("timestamp").and_then(|t| marspot::cc_usage::parse_iso_utc(t.to_string())) else { return };
    match v.str_at("type") {
        Some("system") if v.str_at("subtype") == Some("compact_boundary") => {
            let meta = v.get("compactMetadata");
            let before = meta.and_then(|m| m.get("preTokens")).and_then(Value::i64).unwrap_or(0).max(0) as u64;
            let auto = meta.and_then(|m| m.str_at("trigger")) == Some("auto");
            w.pending = Some((at, before, auto));
        }
        Some("assistant") => {
            if let (Some((at, before, auto)), Some(u)) = (w.pending, v.at(&["message", "usage"])) {
                let n = |k: &str| u.get(k).and_then(Value::i64).unwrap_or(0).max(0) as u64;
                let after = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens") + n("output_tokens");
                w.pending = None;
                if let Some(out) = out {
                    out.push(Compacted { at, before, after, auto });
                }
            }
        }
        Some("user") => user(&v, at, w),
        Some("queue-operation") if v.str_at("operation") == Some("enqueue") => ended(v.str_at("content").unwrap_or(""), w),
        _ => {}
    }
}

fn user(v: &Value, at: i64, w: &mut Watch) {
    if let Some(r) = v.get("toolUseResult") {
        let s = |k: &str| r.str_at(k).filter(|x| !x.is_empty());
        let agent = if r.get("isAsync").and_then(Value::bool) == Some(true) { s("agentId") } else { None };
        let started = agent.map(|id| (id, false)).or_else(|| s("backgroundTaskId").map(|id| (id, true))).or_else(|| s("resumedAgentId").or_else(|| s("revivedAgentId")).map(|id| (id, false)));
        if let Some((id, shell)) = started {
            start(w, id, at, shell);
        }
    }
    let content = v.at(&["message", "content"]);
    let text = content.and_then(Value::str);
    if let Some(t) = text {
        ended(t, w);
        if is_prompt(t) && v.get("isMeta").and_then(Value::bool) != Some(true) {
            w.prompt_at = Some(at);
        }
        return;
    }
    let blocks = content.map_or(&[][..], Value::arr);
    let mut said = false;
    for b in blocks {
        match b.str_at("type") {
            Some("tool_result") => {
                let t = b.get("content").map(text_of).unwrap_or_default();
                if let Some(id) = monitor_task(&t) {
                    start(w, id, at, true);
                }
                return;
            }
            Some("text") => {
                let t = b.str_at("text").unwrap_or("");
                ended(t, w);
                said |= is_prompt(t);
            }
            _ => {}
        }
    }
    if said && v.get("isMeta").and_then(Value::bool) != Some(true) {
        w.prompt_at = Some(at);
    }
}

/// A person's words, not a notice marspot or claude put in the turn.
fn is_prompt(t: &str) -> bool {
    let t = t.trim_start();
    !t.is_empty() && !t.starts_with("<task-notification>") && !t.starts_with("<local-command-") && !t.starts_with("<system-reminder>")
}

fn start(w: &mut Watch, id: &str, at: i64, shell: bool) {
    if w.live.iter().any(|t| t.id == id) {
        return;
    }
    if w.live.len() >= LIVE_CAP {
        w.live.remove(0);
    }
    w.live.push(Task { id: id.to_string(), at, shell });
}

fn ended(text: &str, w: &mut Watch) {
    let mut rest = text;
    while let Some(i) = rest.find("<task-notification>") {
        let body = &rest[i..];
        let end = body.find("</task-notification>").map_or(body.len(), |e| e + "</task-notification>".len());
        let n = &body[..end];
        if tag(n, "status").is_some_and(|s| END_STATUS.contains(&s.trim())) {
            let mut ids = n;
            while let Some(id) = tag(ids, "task-id") {
                let id = id.trim();
                w.live.retain(|t| t.id != id);
                ids = &ids[ids.find("</task-id>").map_or(ids.len(), |e| e + "</task-id>".len())..];
            }
        }
        rest = &body[end..];
    }
}

fn text_of(v: &Value) -> String {
    if let Some(s) = v.str() {
        return s.to_string();
    }
    v.arr().iter().filter_map(|b| b.str_at("text")).collect()
}

fn monitor_task(text: &str) -> Option<&str> {
    const MARK: &str = "Monitor started (task ";
    let rest = &text[text.find(MARK)? + MARK.len()..];
    let n = rest.bytes().take_while(u8::is_ascii_alphanumeric).count();
    (n > 0).then_some(&rest[..n])
}

fn tag<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let from = s.find(&open)? + open.len();
    let to = s[from..].find(&close)? + from;
    Some(&s[from..to])
}

#[cfg(test)]
#[path = "allocation_turns_tests.rs"]
mod tests;
