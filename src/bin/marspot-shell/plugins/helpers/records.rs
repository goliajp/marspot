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

use std::io::Write;
use std::path::{Path, PathBuf};

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
    vec![
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
    ]
}

fn kv(ev: &Event) -> Vec<(&'static str, String)> {
    match ev {
        Event::Started(s) => started_kv(s),
        Event::AccountChanged(a) => vec![("account", a.clone())],
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
                ("result_new", h.result_new.to_string()),
                ("noticed", h.noticed.to_string()),
                ("notices", list(&h.notices)),
                ("suppressed", h.suppressed.to_string()),
                ("held_blocked", h.held_blocked.to_string()),
            ]);
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
    h.result_new = flag("result_new");
    h.noticed = flag("noticed");
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
        "result_delivered" => Event::ResultDelivered,
        "input_seen" => Event::InputSeen,
        "idle_noticed" => Event::IdleNoticed,
        "blocked_noticed" => Event::BlockedNoticed,
        "exited_noticed" => Event::ExitedNoticed,
        "notice_suppressed" => Event::NoticeSuppressed { blocked: get("stop").as_deref() == Some("blocked") },
        "account_changed" => Event::AccountChanged(get("account")?),
        "budget_expired" => Event::BudgetExpired,
        "stopped" => Event::Stopped,
        "carried" => Event::Carried(Box::new(carried_of(sid, &get)?)),
        _ => return None,
    };
    Some((at, sid, ev))
}

/// The table the log at `path` says.
pub(super) fn replay(path: &Path) -> Table {
    let mut t = Table::default();
    let body = std::fs::read_to_string(path).unwrap_or_default();
    for (at, sid, ev) in body.lines().filter_map(parse) {
        t.apply(sid, at, &ev);
    }
    t
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
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path);
    match file.and_then(|mut f| f.write_all(body.as_bytes())) {
        Ok(()) => {}
        Err(e) => marspot::lx_warn!("helpers.log_write_failed", &format!("{}: {e}", path.display())),
    }
}
