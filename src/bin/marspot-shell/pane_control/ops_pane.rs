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
use super::recent::Opened;
use super::shadow_gate::{GATE_WITHIN, Tree};
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

fn counts(tree: &Tree, parent: u64) -> SpawnCounts {
    let parent_of = |s| tree.parent_of(s);
    let shadows = tree.shadows();
    let (root, parent_depth) = shadow_tree::root_and_depth(&parent_of, parent);
    SpawnCounts {
        live_sessions: tree.live(),
        shadows_total: shadows.len(),
        shadows_under_root: shadows.iter().filter(|s| shadow_tree::root_and_depth(&parent_of, **s).0 == root).count(),
        parent_depth,
    }
}

pub(super) fn open(conn: &mut Conn, ctx: &Ctx, a: &OpenArgs) -> Result<Value, Reject> {
    open_within(conn, ctx, a, SPAWN_WITHIN)
}

/// The count limits and the spawn rate for a new pane under `parent`.
fn check_open(conn: &Conn, ctx: &Ctx, tree: &Tree, parent: u64, shadow: bool) -> Result<(), Reject> {
    let now = conn.hub.now_ms();
    let mut limits = conn.hub.limits.lock().unwrap_or_else(|p| p.into_inner());
    check_spawn(&ctx.cfg, &counts(tree, parent), shadow, session_cap())?;
    if !shadow && limits.window_panes_of(ctx.caller) >= ctx.cfg.window_panes_per_caller {
        return Err(Reject::new(ErrorCode::LimitCount, format!("this caller has {} panes open in windows, its most", ctx.cfg.window_panes_per_caller)));
    }
    let root = if parent != 0 { shadow_tree::root_and_depth(&|s| tree.parent_of(s), parent).0 } else { 0 };
    limits.admit_spawn(&ctx.cfg, root, now)
}

/// The pane a new pane opens under (0: none, a window pane from outside).
fn open_parent(ctx: &Ctx, a: &OpenArgs) -> Result<u64, Reject> {
    Ok(match (&a.parent, ctx.caller) {
        (Some(p), _) => target(ctx, p)?,
        (None, CallerKey::Pane(sid)) => sid,
        (None, CallerKey::External) if a.shadow => {
            return Err(Reject::new(ErrorCode::BadRequest, "a shadow pane needs a parent: pass `parent` when calling from outside the panes"));
        }
        (None, CallerKey::External) => 0,
    })
}

pub(super) fn open_within(conn: &mut Conn, ctx: &Ctx, a: &OpenArgs, within: Duration) -> Result<Value, Reject> {
    let parent = open_parent(ctx, a)?;
    conn.need_caps(CAP_SHADOWS)?;
    let gate = conn.hub.shadow_gate.enter(GATE_WITHIN)?;
    // the view from before the wait in the gate can be far behind
    check_open(conn, ctx, &gate.tree(&ctx.view_now()), parent, a.shadow)?;
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
    let shadow_parent = if a.shadow { parent } else { 0 };
    let sid = match conn.ask(parent, MsgType::PaneSpawn, built, false, within) {
        Ok(Reply::Spawned(s)) if s.ok => s.sid,
        Ok(Reply::Spawned(s)) => return Err(Reject::new(ErrorCode::SpawnFailed, s.err)),
        Ok(Reply::Done(d)) => return Err(done_reject(d.code, &d.text)),
        Ok(_) => return Err(Reject::new(ErrorCode::Internal, "an answer of the wrong kind")),
        Err(Asked::Refused(r)) => return Err(r),
        Err(Asked::NoAnswer) => {
            gate.record_unanswered(shadow_parent);
            return Err(Reject::new(ErrorCode::Unknown, "no answer from the core; check with pane.list before opening again"));
        }
    };
    gate.record(sid, shadow_parent);
    drop(gate);
    // the name a list shows next: the observation may not have the pane yet
    let cwd = names::cwd_of_entry(sid).unwrap_or_else(|| spawn.cwd.clone());
    let name = marspot::pane_name::PaneRef::new(sid, cwd.clone()).full_name();
    conn.hub.recent.lock().unwrap_or_else(|p| p.into_inner()).opened(Opened { sid, cwd, parent: shadow_parent }, Instant::now());
    conn.hub.limits.lock().unwrap_or_else(|p| p.into_inner()).opened(ctx.caller, sid, a.shadow);
    audit(conn, ctx, "pane.open", sid, a.command.clone().map(String::into_bytes), false, if a.shadow { "shadow" } else { "window" });
    if let Some(cmd) = &a.command {
        type_once_at_prompt(conn, ctx, sid, cmd);
    }
    let pos = conn.probe_pos(sid, Duration::from_millis(250));
    Ok(obj([
        ("id", (sid as i64).into()),
        ("name", name.into()),
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
    if out.is_ok() {
        conn.hub.recent.lock().unwrap_or_else(|p| p.into_inner()).closed(sid, Instant::now());
    }
    audit(conn, ctx, "pane.close", sid, None, false, out.as_ref().map_or_else(|r| r.code.as_str(), |_| "closed"));
    out.map(|_| obj([("id", (sid as i64).into()), ("recycled", (mode == RetireMode::Recycle).into())]))
}

pub(super) fn resize(conn: &mut Conn, ctx: &Ctx, t: &str, cols: u16, rows: u16) -> Result<Value, Reject> {
    let sid = target(ctx, t)?;
    conn.need_caps(CAP_SHADOWS)?;
    if !is_shadow(conn, ctx, sid) {
        return Err(Reject::new(ErrorCode::NotResizable, "a pane in a window takes its size from the window's layout"));
    }
    let out = ask_done(conn, sid, MsgType::PaneSize, |req_id| Ok(encode_pane_size(&PaneSize { req_id, sid, cols, rows })), Again::Yes);
    audit(conn, ctx, "pane.resize", sid, None, false, out.as_ref().map_or_else(|r| r.code.as_str(), |_| "resized"));
    out.map(|_| obj([("id", (sid as i64).into())]))
}

/// Pane `sid` is a shadow in the view, or, when the view is older than
/// the pane, it was opened as one.  The core checks again.
fn is_shadow(conn: &Conn, ctx: &Ctx, sid: u64) -> bool {
    match ctx.obs.session(sid) {
        Some(s) => panes::shadow_of(&s.entry) != 0,
        None => conn.hub.recent.lock().unwrap_or_else(|p| p.into_inner()).opened_after(ctx.obs.at, Instant::now()).any(|o| o.sid == sid && o.parent != 0),
    }
}

pub(super) fn place(conn: &mut Conn, ctx: &Ctx, t: &str, w: Where) -> Result<Value, Reject> {
    let sid = target(ctx, t)?;
    place_sid(conn, ctx, sid, w)
}

pub(super) fn place_sid(conn: &mut Conn, ctx: &Ctx, sid: u64, w: Where) -> Result<Value, Reject> {
    let to = match w {
        Where::Window => PlaceWhere::Window,
        Where::Shadow => PlaceWhere::Shadow,
        Where::Peek => PlaceWhere::Peek,
        Where::Unpeek => PlaceWhere::Unpeek,
    };
    conn.need_caps(if matches!(w, Where::Peek | Where::Unpeek) { CAP_PEEK } else { CAP_SHADOWS })?;
    // only a move into the shadows can close a loop or add to a count
    let gate = if matches!(w, Where::Shadow) { Some(conn.hub.shadow_gate.enter(GATE_WITHIN)?) } else { None };
    let host_sid = match (w, ctx.caller, &gate) {
        // a pane taken out of its window becomes the shadow of the caller's;
        // checked even when the registry already names it a shadow, which
        // can run ahead of where the core holds it
        (Where::Shadow, CallerKey::Pane(c), Some(g)) if c != sid => {
            let tree = g.tree(&ctx.view_now());
            shadow_tree::check_adopt(&ctx.cfg, &|s| tree.parent_of(s), &tree.shadows(), c, sid)?;
            c
        }
        (Where::Shadow, ..) => return Err(Reject::new(ErrorCode::BadRequest, "a shadow pane needs a parent: call from another pane")),
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
    if let Some(g) = gate
        && out.as_ref().map_or_else(|r| r.code == ErrorCode::Unknown, |_| true)
    {
        g.record(sid, host_sid);
    }
    audit(conn, ctx, "pane.place", sid, None, false, out.as_ref().map_or_else(|r| r.code.as_str(), |_| w.as_str()));
    out.map(|_| obj([("id", (sid as i64).into()), ("where", w.as_str().into())]))
}
