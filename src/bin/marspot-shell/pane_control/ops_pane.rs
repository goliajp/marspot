//! `pane.open`, `pane.close`, `pane.resize`, `pane.place`: asked of the
//! core, which owns panes and windows.  A core that has not announced
//! it does these (`PaneControlCaps` bit 1, bit 2 for peeking) is not
//! sent them: the answer is `old_session`.

use std::time::{Duration, Instant};

use marspot::shell_proto::{
    CAP_PEEK, CAP_SHADOWS, DoneCode, MsgType, PanePlace, PaneRetire, PaneSize, PaneSpawn, PaneType, PlaceWhere, RetireMode,
    WatchBody, WatchKind, encode_pane_place, encode_pane_retire, encode_pane_size, encode_pane_spawn, encode_pane_type,
};
use marspot_peer::control::{ErrorCode, OpenArgs, Reject, Where};
use marspot_peer::json::{Value, obj};

use super::caller::CallerKey;
use super::conn::{ANSWER_WITHIN, Asked, Conn, done_reject};
use super::dispatch::{Ctx, audit, target};
use super::limits::{SpawnCounts, check_spawn, session_cap};
use super::{Reply, names, panes, shadow_tree};

/// How long a new pane may take to be answered for.
const SPAWN_WITHIN: Duration = Duration::from_secs(10);
/// A shadow pane nobody types into or reads output from ends after this.
const SHADOW_IDLE_S: u32 = 6 * 3600;

/// How a request may be asked again when nothing came back.
#[derive(Clone, Copy, PartialEq)]
enum Again {
    No,
    Yes,
    /// Yes, and a session gone by the second asking is what the first did.
    Gone,
}

/// Ask, and for the requests that can be repeated, ask once more when
/// nothing came back.
fn ask_done(conn: &mut Conn, sid: u64, msg: MsgType, build: impl Fn(u32) -> std::io::Result<Vec<u8>>, again: Again) -> Result<(), Reject> {
    ask_done_within(conn, sid, msg, build, again, ANSWER_WITHIN)
}

fn ask_done_within(
    conn: &mut Conn,
    sid: u64,
    msg: MsgType,
    build: impl Fn(u32) -> std::io::Result<Vec<u8>>,
    again: Again,
    within: Duration,
) -> Result<(), Reject> {
    let repeatable = again != Again::No;
    for attempt in 0..2 {
        match conn.ask(sid, msg, &build, false, within) {
            Ok(Reply::Done(d)) if d.code == DoneCode::Ok => return Ok(()),
            Ok(Reply::Done(d)) if attempt == 1 && again == Again::Gone && d.code == DoneCode::NoSuchSession => return Ok(()),
            Ok(Reply::Done(d)) => return Err(done_reject(d.code, &d.text)),
            Ok(_) => return Err(Reject::new(ErrorCode::Internal, "an answer of the wrong kind")),
            Err(Asked::Refused(r)) => return Err(r),
            Err(Asked::NoAnswer) if repeatable && attempt == 0 => {}
            Err(Asked::NoAnswer) => break,
        }
    }
    Err(Reject::new(ErrorCode::Unknown, "the core did not answer; check with pane.list"))
}

fn parent_of(ctx: &Ctx) -> impl Fn(u64) -> u64 + '_ {
    |sid| ctx.obs.session(sid).map_or(0, |s| panes::shadow_of(&s.entry))
}

fn root_and_depth(ctx: &Ctx, sid: u64) -> (u64, usize) {
    shadow_tree::root_and_depth(&parent_of(ctx), sid)
}

fn shadow_sids(ctx: &Ctx) -> Vec<u64> {
    ctx.obs.live().filter(|s| panes::shadow_of(&s.entry) != 0).map(|s| s.entry.id).collect()
}

fn counts(ctx: &Ctx, parent: u64) -> SpawnCounts {
    let shadows = shadow_sids(ctx);
    let (root, parent_depth) = root_and_depth(ctx, parent);
    SpawnCounts {
        live_sessions: ctx.obs.live().count(),
        shadows_total: shadows.len(),
        shadows_under_root: shadows.iter().filter(|s| root_and_depth(ctx, **s).0 == root).count(),
        parent_depth,
    }
}

pub(super) fn open(conn: &mut Conn, ctx: &Ctx, a: &OpenArgs) -> Result<Value, Reject> {
    let parent = match (&a.parent, ctx.caller) {
        (Some(p), _) => target(ctx, p)?,
        (None, CallerKey::Pane(sid)) => sid,
        (None, CallerKey::External) if a.shadow => {
            return Err(Reject::new(ErrorCode::BadRequest, "a shadow pane needs a parent: pass `parent` when calling from outside the panes"));
        }
        (None, CallerKey::External) => 0,
    };
    conn.need_caps(CAP_SHADOWS)?;
    let now = conn.hub.now_ms();
    {
        let mut limits = conn.hub.limits.lock().unwrap_or_else(|p| p.into_inner());
        check_spawn(&ctx.cfg, &counts(ctx, parent), a.shadow, session_cap())?;
        if !a.shadow && limits.window_panes_of(ctx.caller) >= ctx.cfg.window_panes_per_caller {
            return Err(Reject::new(ErrorCode::LimitCount, format!("this caller has {} panes open in windows, its most", ctx.cfg.window_panes_per_caller)));
        }
        let root = if parent != 0 { root_and_depth(ctx, parent).0 } else { 0 };
        limits.admit_spawn(&ctx.cfg, root, now)?;
    }
    let cwd = a.cwd.clone().or_else(|| names::cwd_of(parent, &ctx.obs)).unwrap_or_default();
    let spawn = PaneSpawn {
        req_id: 0,
        shadow: a.shadow,
        no_focus: true,
        // a pane for a window opens in the window holding this one
        parent_sid: parent,
        idle_s: SHADOW_IDLE_S,
        cols: a.cols,
        rows: a.rows,
        cwd,
    };
    let built = |req_id| encode_pane_spawn(&PaneSpawn { req_id, ..spawn.clone() });
    let sid = match conn.ask(parent, MsgType::PaneSpawn, built, false, SPAWN_WITHIN) {
        Ok(Reply::Spawned(s)) if s.ok => s.sid,
        Ok(Reply::Spawned(s)) => return Err(Reject::new(ErrorCode::SpawnFailed, s.err)),
        Ok(Reply::Done(d)) => return Err(done_reject(d.code, &d.text)),
        Ok(_) => return Err(Reject::new(ErrorCode::Internal, "an answer of the wrong kind")),
        Err(Asked::Refused(r)) => return Err(r),
        Err(Asked::NoAnswer) => return Err(Reject::new(ErrorCode::Unknown, "no answer from the core; check with pane.list before opening again")),
    };
    conn.hub.limits.lock().unwrap_or_else(|p| p.into_inner()).opened(ctx.caller, sid, a.shadow);
    audit(conn, ctx, "pane.open", sid, a.command.clone().map(String::into_bytes), false, if a.shadow { "shadow" } else { "window" });
    if let Some(cmd) = &a.command {
        type_once_at_prompt(conn, ctx, sid, cmd);
    }
    let pos = conn.probe_pos(sid, Duration::from_millis(250));
    Ok(obj([
        ("id", (sid as i64).into()),
        ("name", names::full_name(sid, &ctx.obs).into()),
        ("pos", pos.map_or(Value::Null, |p| (p as i64).into())),
    ]))
}

/// Type `cmd` into a new pane once its shell shows a prompt.
fn type_once_at_prompt(conn: &mut Conn, ctx: &Ctx, sid: u64, cmd: &str) {
    let body = WatchBody { watch_id: 0, kind: WatchKind::Prompt, since_pos: 0, quiet_ms: 0, text: String::new() };
    let _ = conn.wait(sid, ctx.caller, body, Some(0), Instant::now() + SPAWN_WITHIN);
    let t = PaneType { req_id: 0, sid, paste: true, enter: true, only_at_shell: true, bytes: cmd.as_bytes().to_vec(), watch: None };
    let _ = conn.ask(sid, MsgType::PaneType, |req_id| encode_pane_type(&PaneType { req_id, ..t.clone() }), false, ANSWER_WITHIN);
}

pub(super) fn close(conn: &mut Conn, ctx: &Ctx, t: &str) -> Result<Value, Reject> {
    let sid = target(ctx, t)?;
    conn.need_caps(CAP_SHADOWS)?;
    let shadow = ctx.obs.session(sid).is_some_and(|s| panes::shadow_of(&s.entry) != 0);
    let mine = conn.hub.limits.lock().unwrap_or_else(|p| p.into_inner()).opened_by(sid) == Some(ctx.caller);
    let mode = if shadow && mine { RetireMode::Delete } else { RetireMode::Recycle };
    let out = ask_done(conn, sid, MsgType::PaneRetire, |req_id| Ok(encode_pane_retire(&PaneRetire { req_id, sid, mode })), Again::Gone);
    audit(conn, ctx, "pane.close", sid, None, false, out.as_ref().map_or_else(|r| r.code.as_str(), |_| "closed"));
    out.map(|_| obj([("id", (sid as i64).into()), ("recycled", (mode == RetireMode::Recycle).into())]))
}

pub(super) fn resize(conn: &mut Conn, ctx: &Ctx, t: &str, cols: u16, rows: u16) -> Result<Value, Reject> {
    let sid = target(ctx, t)?;
    conn.need_caps(CAP_SHADOWS)?;
    if !ctx.obs.session(sid).is_some_and(|s| panes::shadow_of(&s.entry) != 0) {
        return Err(Reject::new(ErrorCode::NotResizable, "a pane in a window takes its size from the window's layout"));
    }
    let out = ask_done(conn, sid, MsgType::PaneSize, |req_id| Ok(encode_pane_size(&PaneSize { req_id, sid, cols, rows })), Again::Yes);
    audit(conn, ctx, "pane.resize", sid, None, false, out.as_ref().map_or_else(|r| r.code.as_str(), |_| "resized"));
    out.map(|_| obj([("id", (sid as i64).into())]))
}

pub(super) fn place(conn: &mut Conn, ctx: &Ctx, t: &str, w: Where) -> Result<Value, Reject> {
    let sid = target(ctx, t)?;
    let to = match w {
        Where::Window => PlaceWhere::Window,
        Where::Shadow => PlaceWhere::Shadow,
        Where::Peek => PlaceWhere::Peek,
        Where::Unpeek => PlaceWhere::Unpeek,
    };
    conn.need_caps(if matches!(w, Where::Peek | Where::Unpeek) { CAP_PEEK } else { CAP_SHADOWS })?;
    let host_sid = match (w, ctx.caller) {
        // a pane taken out of its window becomes the shadow of the caller's;
        // checked even when the registry already names it a shadow, which
        // can run ahead of where the core holds it
        (Where::Shadow, CallerKey::Pane(c)) if c != sid => {
            shadow_tree::check_adopt(&ctx.cfg, &parent_of(ctx), &shadow_sids(ctx), c, sid)?;
            c
        }
        (Where::Shadow, _) => return Err(Reject::new(ErrorCode::BadRequest, "a shadow pane needs a parent: call from another pane")),
        _ => ctx.obs.session(sid).map_or(0, |s| panes::shadow_of(&s.entry)),
    };
    // the core waits up to ANSWER_WITHIN for the session, then answers itself
    let out = ask_done_within(
        conn,
        sid,
        MsgType::PanePlace,
        |req_id| Ok(encode_pane_place(&PanePlace { req_id, sid, to, host_sid })),
        Again::No,
        ANSWER_WITHIN + Duration::from_secs(2),
    );
    audit(conn, ctx, "pane.place", sid, None, false, out.as_ref().map_or_else(|r| r.code.as_str(), |_| w.as_str()));
    out.map(|_| obj([("id", (sid as i64).into()), ("where", w.as_str().into())]))
}
