//! `<state dir>/helpers/events.log`: what happened to each helper, one
//! line a fact, appended and never rewritten.  A line is tab-separated:
//! when it was recorded, when it happened (ms after the epoch), the
//! helper's session id, the event, then `key=value` arguments with tab,
//! newline and backslash escaped.
//!
//! At 8 MiB the file is rotated and the new one starts with a `carried`
//! line per live helper: its whole state, which replays as the events it
//! sums up did.  `started` alone would not do: the next start of the
//! shell would find no `task_delivered` and close every helper.

use std::collections::HashSet;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use marspot::pidtree::ProcId;
use marspot_peer::control::{AgentKind, Isolation};

use super::table::{Event, Helper, Started, Table};

pub(super) const ROTATE_AT: u64 = 8 * 1024 * 1024;

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub(super) fn dir() -> PathBuf {
    marspot_term::paths::state_root().join("helpers")
}

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

fn unesc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

fn started_kv(s: &Started) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("parent", s.parent.to_string()),
        ("caller", s.caller.clone()),
        ("agent", s.agent.as_str().into()),
        ("account", s.account.clone()),
        ("model", s.model.clone()),
        ("mode", s.mode.clone()),
        ("isolation", s.isolation.as_str().into()),
        ("budget", s.budget_min.to_string()),
        ("task", s.task_line.clone()),
        ("sha256", s.task_sha.clone()),
        ("label", s.label.clone()),
    ];
    v.extend(s.conv.clone().map(|c| ("conv", c)));
    v
}

fn proc_text(p: &ProcId) -> String {
    format!("{}:{}", p.pid, p.start_us)
}

fn proc_of(text: &str) -> Option<ProcId> {
    let (pid, start_us) = text.split_once(':')?;
    Some(ProcId { pid: pid.parse().ok()?, start_us: start_us.parse().ok()? })
}

fn kv(ev: &Event) -> Vec<(&'static str, String)> {
    match ev {
        Event::Started(s) => started_kv(s),
        Event::ResultDelivered { sha } if !sha.is_empty() => vec![("sha256", sha.clone())],
        Event::AccountChanged(a) => vec![("account", a.clone())],
        Event::BlockedNoticed { prompt } if !prompt.is_empty() => vec![("prompt", prompt.clone())],
        Event::ParentAgent(p) => vec![("agent_proc", proc_text(p))],
        Event::ConversationChanged(c) => vec![("conv", c.clone())],
        Event::NoticeSuppressed { blocked: true } => vec![("stop", "blocked".into())],
        Event::Carried(h) => {
            let mut v = started_kv(&h.started);
            let list = |n: &[u64]| n.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
            v.extend([
                ("started_ms", h.started_ms.to_string()),
                ("preparing", h.preparing.to_string()),
                ("delivered_ms", h.delivered_ms.map_or(String::new(), |d| d.to_string())),
                ("now_account", h.account.clone()),
                ("results", h.results_delivered.to_string()),
                ("result_sha", h.result_sha.clone()),
                ("result_new", h.result_new.to_string()),
                ("noticed", h.noticed.to_string()),
                ("blocked_told", h.blocked_told.to_string()),
                ("blocked_prompt", h.blocked_prompt.clone()),
                ("notices", list(&h.notices)),
                ("suppressed", h.suppressed.to_string()),
                ("held_blocked", h.held_blocked.to_string()),
            ]);
            v.extend(h.conv.clone().map(|c| ("now_conv", c)));
            v.extend(h.heir.as_ref().map(|p| ("heir", proc_text(p))));
            v
        }
        _ => Vec::new(),
    }
}

/// One event as its line, without the newline.
pub(super) fn line(recorded_ms: u64, at_ms: u64, sid: u64, ev: &Event) -> String {
    let mut l = format!("{recorded_ms}\t{at_ms}\t{sid}\t{}", ev.word());
    for (k, v) in kv(ev) {
        l.push('\t');
        l.push_str(k);
        l.push('=');
        l.push_str(&esc(&v));
    }
    l
}

fn started_of(get: &dyn Fn(&str) -> Option<String>) -> Option<Started> {
    Some(Started {
        parent: get("parent")?.parse().ok()?,
        caller: get("caller")?,
        agent: AgentKind::parse(&get("agent")?)?,
        account: get("account").unwrap_or_default(),
        model: get("model").unwrap_or_default(),
        mode: get("mode").unwrap_or_default(),
        isolation: if get("isolation").as_deref() == Some("container") { Isolation::Container } else { Isolation::Host },
        budget_min: get("budget")?.parse().ok()?,
        task_line: get("task").unwrap_or_default(),
        task_sha: get("sha256").unwrap_or_default(),
        label: get("label").unwrap_or_default(),
        conv: get("conv"),
    })
}

fn carried_of(sid: u64, get: &dyn Fn(&str) -> Option<String>) -> Option<Helper> {
    let started = started_of(get)?;
    let flag = |k: &str| get(k).as_deref() == Some("true");
    let mut h = Helper::new(sid, started, get("started_ms")?.parse().ok()?);
    h.preparing = flag("preparing");
    h.delivered_ms = get("delivered_ms").and_then(|d| d.parse().ok());
    h.account = get("now_account").unwrap_or_else(|| h.account.clone());
    h.results_delivered = get("results").and_then(|r| r.parse().ok()).unwrap_or(0);
    h.result_sha = get("result_sha").unwrap_or_default();
    h.result_new = flag("result_new");
    h.noticed = flag("noticed");
    h.blocked_told = flag("blocked_told");
    h.blocked_prompt = get("blocked_prompt").unwrap_or_default();
    h.conv = get("now_conv").or(h.conv);
    h.heir = get("heir").as_deref().and_then(proc_of);
    h.notices = get("notices").unwrap_or_default().split(',').filter_map(|t| t.parse().ok()).collect();
    h.suppressed = get("suppressed").and_then(|r| r.parse().ok()).unwrap_or(0);
    h.held_blocked = flag("held_blocked");
    Some(h)
}

/// A line back to (when it happened, helper, event); None for a line
/// this build cannot read, which replays as nothing.
pub(super) fn parse(l: &str) -> Option<(u64, u64, Event)> {
    let mut f = l.split('\t');
    let _recorded: u64 = f.next()?.parse().ok()?;
    let at: u64 = f.next()?.parse().ok()?;
    let sid: u64 = f.next()?.parse().ok()?;
    let word = f.next()?;
    let pairs: Vec<(&str, String)> = f.filter_map(|p| p.split_once('=')).map(|(k, v)| (k, unesc(v))).collect();
    let get = |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
    let ev = match word {
        "started" => Event::Started(started_of(&get)?),
        "preparing" => Event::Preparing,
        "task_delivered" => Event::TaskDelivered,
        "result_changed" => Event::ResultChanged,
        "result_delivered" => Event::ResultDelivered { sha: get("sha256").unwrap_or_default() },
        "input_seen" => Event::InputSeen,
        "idle_noticed" => Event::IdleNoticed,
        "blocked_noticed" => Event::BlockedNoticed { prompt: get("prompt").unwrap_or_default() },
        "exited_noticed" => Event::ExitedNoticed,
        "notice_suppressed" => Event::NoticeSuppressed { blocked: get("stop").as_deref() == Some("blocked") },
        "account_changed" => Event::AccountChanged(get("account")?),
        "parent_agent" => Event::ParentAgent(proc_of(&get("agent_proc")?)?),
        "conversation_changed" => Event::ConversationChanged(get("conv")?),
        "budget_expired" => Event::BudgetExpired,
        "stopped" => Event::Stopped,
        "carried" => Event::Carried(Box::new(carried_of(sid, &get)?)),
        _ => return None,
    };
    Some((at, sid, ev))
}

/// What a log says.
#[derive(Debug, Default)]
pub(super) struct Replayed {
    pub table: Table,
    /// The helpers it says stopped.
    pub stopped: HashSet<u64>,
}

/// Replay the log at `path`; none there is an empty one.  A line that
/// cannot be read is skipped by itself.
pub(super) fn replay(path: &Path) -> std::io::Result<Replayed> {
    let mut r = Replayed::default();
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(r),
        Err(e) => return Err(e),
    };
    for (at, sid, ev) in body.split(|b| *b == b'\n').filter_map(|l| parse(std::str::from_utf8(l).ok()?)) {
        match ev {
            Event::Stopped => {
                r.stopped.insert(sid);
            }
            Event::Started(_) | Event::Carried(_) => {
                r.stopped.remove(&sid);
            }
            _ => {}
        }
        r.table.apply(sid, at, &ev);
    }
    Ok(r)
}

/// Whether `f` ends in the middle of a line: a write that was cut short.
fn ends_in_a_line(f: &mut std::fs::File) -> std::io::Result<bool> {
    let mut last = *b"\n";
    if f.seek(SeekFrom::End(0))? > 0 {
        f.seek(SeekFrom::End(-1))?;
        f.read_exact(&mut last)?;
    }
    Ok(last != *b"\n")
}

/// Append `body` on a line of its own.
fn append_to(path: &Path, mut body: String) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().create(true).read(true).append(true).open(path)?;
    if ends_in_a_line(&mut f)? {
        body.insert(0, '\n');
    }
    f.write_all(body.as_bytes())
}

/// Append `ev`, rotating first when the file would pass `rotate_at`:
/// the new file opens with a `carried` line per helper in `table` (the
/// table before `ev` applies).
pub(super) fn append(path: &Path, rotate_at: u64, table: &Table, sid: u64, at_ms: u64, ev: &Event) {
    let now = now_ms();
    let mut body = line(now, at_ms, sid, ev);
    body.push('\n');
    if std::fs::metadata(path).is_ok_and(|m| m.len() + body.len() as u64 > rotate_at) {
        crate::pane_control::rotate_log(path);
        let mut carried: String = table.map.values().map(|h| line(now, now, h.sid, &Event::Carried(Box::new(h.clone()))) + "\n").collect();
        carried.push_str(&body);
        body = carried;
    }
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Err(e) = append_to(path, body) {
        marspot::lx_warn!("helpers.log_write_failed", &format!("{}: {e}", path.display()));
    }
}
