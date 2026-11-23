//! `pane.list`, `pane.read` and `pane.audit`: answered on the
//! connection's thread.  A read with a wait sleeps on the session's
//! report first; the screen itself is read from what the session
//! publishes, never asked of it.  A plain read asks the session nothing
//! at all, so it answers no `pos`: a caller that needs one to wait
//! from gets it from its send, run or wait.

use std::time::{Duration, Instant};

use marspot::shell_proto::{WatchBody, WatchKind, WatchReason};
use marspot_peer::control::{ReadArgs, Reject, Wait, screen};
use marspot_peer::json::{Value, obj};

use marspot_term::terminal::Terminal;

use super::super::observe::Observation;
use super::conn::Conn;
use super::dispatch::{Ctx, target};
use super::{names, panes};

/// The most text one read hands back.
pub(super) const MAX_TEXT: usize = 1024 * 1024;
/// How long a read waits to learn the pane's output position.
const POS_WITHIN: Duration = Duration::from_millis(250);

pub(super) fn list(conn: &Conn, ctx: &Ctx) -> Value {
    let views = conn.hub.fresh_pane_views(Duration::from_millis(250));
    let cells = panes::cells();
    let open = conn.hub.open_windows();
    let table = marspot_peer::panes::live(&marspot_peer::peers::live_entries());
    let agent_of = |code: Option<u16>| {
        table.iter().find(|p| p.code.is_some() && p.code == code).and_then(|p| p.agent.clone()).map_or(Value::Null, Value::from)
    };
    let row = |sid: u64, cwd: String, parent: u64| {
        let r = marspot::pane_name::PaneRef::new(sid, cwd.clone());
        let view = views.get(&sid);
        // a closed window keeps its place in the saved layout, to be reopened
        let shown = cells.get(&sid).filter(|(w, _, _)| open.contains(&(w - 1)));
        let cell = shown.map_or(Value::Null, |(w, x, y)| format!("w({w},{x},{y})").into());
        obj([
            ("id", (sid as i64).into()),
            ("name", r.full_name().into()),
            ("cwd", cwd.into()),
            ("shadow", (parent != 0).into()),
            ("parent", if parent != 0 { (parent as i64).into() } else { Value::Null }),
            ("detached", (parent == 0 && shown.is_none()).into()),
            ("cell", cell),
            ("agent", agent_of(r.code)),
            ("state", view.map_or("unknown", |v| v.state.as_str()).into()),
            ("dormant", view.is_some_and(|v| v.dormant).into()),
            ("busy", view.and_then(|v| v.busy).map_or(Value::Null, Value::from)),
        ])
    };
    let live: std::collections::HashSet<u64> = ctx.obs.live().map(|s| s.entry.id).collect();
    let recent = conn.hub.recent.lock().unwrap_or_else(|p| p.into_inner());
    let now = Instant::now();
    let mut out = Vec::new();
    for (sid, cwd, _) in names::live_panes(&ctx.obs) {
        let Some(s) = ctx.obs.session(sid).filter(|_| live.contains(&sid) && !recent.closed_lately(sid, now)) else { continue };
        out.push(row(sid, cwd, panes::shadow_of(&s.entry)));
    }
    for o in recent.opened_after(ctx.obs.at, now).filter(|o| !live.contains(&o.sid)) {
        out.push(row(o.sid, o.cwd.clone(), o.parent));
    }
    obj([("panes", Value::Arr(out))])
}

/// The watch a `wait` asks for.
pub(super) fn watch_body(w: &Wait) -> WatchBody {
    let (kind, quiet_ms, text) = match w {
        Wait::Text(t) => (WatchKind::Text, 0, t.clone()),
        Wait::Quiet(ms) => (WatchKind::Quiet, *ms, String::new()),
        Wait::CommandEnd => (WatchKind::CommandEnd, 0, String::new()),
        Wait::Prompt => (WatchKind::Prompt, 0, String::new()),
        Wait::Exit => (WatchKind::Exit, 0, String::new()),
        Wait::Change => (WatchKind::Change, 0, String::new()),
    };
    WatchBody { watch_id: 0, kind, since_pos: 0, quiet_ms, text }
}

/// The `reason` a report is answered with.
pub(super) fn reason_word(r: WatchReason, waited_for_exit: bool) -> &'static str {
    match r {
        WatchReason::SessionEnded if !waited_for_exit => "session_ended",
        WatchReason::Restarting => "restarting",
        _ => "met",
    }
}

pub(super) fn read(conn: &mut Conn, ctx: &Ctx, a: &ReadArgs) -> Result<Value, Reject> {
    let sid = target(ctx, &a.target)?;
    let Some(w) = &a.wait else { return Ok(screen_of(sid, a.history_lines, None, &ctx.obs)) };
    let deadline = Instant::now() + Duration::from_secs(a.timeout_s.into());
    let Some(f) = conn.wait(sid, ctx.caller, watch_body(w), a.since, deadline)? else {
        let mut v = screen_of(sid, a.history_lines, conn.probe_pos(sid, POS_WITHIN), &ctx.obs);
        v.set("reason", "timeout".into());
        return Ok(v);
    };
    // the matched line and the history come from one replay of the record
    let replay = match (w, f.line) {
        (Wait::Text(_), Some(_)) => super::matched::replay(sid),
        _ => None,
    };
    let mut v = screen_from(sid, a.history_lines, Some(f.pos), &ctx.obs, replay.as_ref().map(|r| &r.term));
    v.set("reason", reason_word(f.reason, *w == Wait::Exit).into());
    if let Some(st) = f.status {
        v.set("exit_status", i64::from(st).into());
    }
    if let (Wait::Text(needle), Some(line)) = (w, f.line) {
        matched_line(&mut v, replay.as_ref(), line, needle);
    }
    Ok(v)
}

/// The text of the line a text wait matched, read by its row; when the
/// record no longer holds that row as the session numbered it, the
/// newest screen line holding the text, and `matched_line_from` says
/// which of the two it is.
fn matched_line(v: &mut Value, replay: Option<&super::matched::Replay>, line: u64, needle: &str) {
    let squeezed: String = needle.split_whitespace().collect();
    let by_row = replay
        .and_then(|r| r.line_at(line))
        .filter(|l| l.contains(needle) || l.split_whitespace().collect::<String>().contains(&squeezed));
    let (text, from) = match by_row {
        Some(l) => (Some(l), "row"),
        None => (v.str_at("text").unwrap_or("").lines().rev().find(|l| l.contains(needle)).map(String::from), "screen"),
    };
    if let Some(t) = text {
        v.set("matched_line", screen::filter(&t).into());
        v.set("matched_line_from", from.into());
    }
}

/// What pane `sid` shows, with its size and cursor.
pub(super) fn screen_of(sid: u64, history_lines: u32, pos: Option<u64>, obs: &Observation) -> Value {
    screen_from(sid, history_lines, pos, obs, None)
}

/// [`screen_of`], with the history from `replay` when one is at hand.
fn screen_from(sid: u64, history_lines: u32, pos: Option<u64>, obs: &Observation, replay: Option<&Terminal>) -> Value {
    let raw = marspot::pane_read::screen_with(sid, history_lines.min(u16::MAX as u32) as u16, replay).unwrap_or_default();
    let (text, truncated) = cap_text(screen::filter(&raw));
    let (cols, rows, cursor) = shape(sid);
    let mut v = obj([
        ("pane", names::full_name(sid, obs).into()),
        ("id", (sid as i64).into()),
        ("text", text.into()),
        ("pos", pos.map_or(Value::Null, |p| (p as i64).into())),
        ("cols", (cols as i64).into()),
        ("rows", (rows as i64).into()),
        ("cursor", cursor.map_or(Value::Null, |(c, r)| Value::Arr(vec![(c as i64).into(), (r as i64).into()]))),
    ]);
    if truncated {
        v.set("truncated", true.into());
    }
    v
}

/// At most [`MAX_TEXT`] bytes, cut from the oldest end at a line.
pub(super) fn cap_text(text: String) -> (String, bool) {
    if text.len() <= MAX_TEXT {
        return (text, false);
    }
    let mut cut = text.len() - MAX_TEXT;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    let start = text[cut..].find('\n').map_or(cut, |i| cut + i + 1);
    (text[start..].to_string(), true)
}

/// The pane's size and cursor from its published frame; the registry's
/// size and no cursor when there is no live frame.
fn shape(sid: u64) -> (u16, u16, Option<(u16, u16)>) {
    let Ok(entry) = marspot_term::session_registry::read_session_entry(sid) else { return (0, 0, None) };
    let frame = marspot_term::grid_shm::SessionRegions::of_entry(&entry)
        .and_then(|r| marspot_term::grid_shm::peek_frame(&r).ok().flatten())
        .filter(|f| f.view_offset == 0);
    match frame {
        Some(f) => (f.cols, f.rows, Some((f.cursor_col, f.cursor_row))),
        None => (entry.cols, entry.rows, None),
    }
}

pub(super) fn audit(conn: &Conn, limit: u32) -> Value {
    conn.hub.audit.flush();
    let lines = super::audit_log::tail(&super::audit_log::log_path(), limit as usize);
    obj([("records", Value::Arr(lines.into_iter().map(Value::from).collect()))])
}
