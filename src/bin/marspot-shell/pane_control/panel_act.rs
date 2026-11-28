//! What the user asked for on a window's helper panel
//! (`HelperPanelAct`): close a shadow pane, close every shadow pane of
//! a session, deny or allow its directory to control panes; and from a
//! pane's menu, make it the child of another pane or a top pane.
//!
//! Done on a thread of its own, one at a time: a close waits for the
//! core, whose frames the thread that reads these must keep reading.

use std::sync::OnceLock;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use marspot::control_deny::{self, Outcome};
use marspot::shell_proto::{CwdControl, HelperPanelAct, PanelAct, decode_helper_panel_act};
use marspot_peer::control::{ErrorCode, Reject};

use super::super::observe::{self, Observation};
use super::caller::{CallerKey, Peer};
use super::conn::Conn;
use super::dispatch::{Ctx, audit};
use super::{helpers_port, hub, names, ops_pane, shadow_tree};

/// Clicks waiting at most; one more is dropped, and the pane it was
/// about is still on the panel.
const QUEUE: usize = 16;
/// How old a view of the sessions may be.
const VIEW: Duration = Duration::from_millis(100);

/// A frame from the core's reader thread.
pub(super) fn take(payload: &[u8]) {
    static TX: OnceLock<SyncSender<HelperPanelAct>> = OnceLock::new();
    let Ok(act) = decode_helper_panel_act(payload) else { return };
    let tx = TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<HelperPanelAct>(QUEUE);
        let spawned = std::thread::Builder::new().name("helper-panel".into()).spawn(move || {
            for act in rx {
                serve(act);
            }
        });
        if let Err(e) = spawned {
            marspot::lx_warn!("shell.control.panel_thread", &format!("{e}"));
        }
        tx
    });
    if tx.try_send(act).is_err() {
        marspot::lx_warn!("shell.control.panel_act_dropped", "a click on the helper panel was not taken", session = act.sid);
    }
}

fn serve(act: HelperPanelAct) {
    let Some(h) = hub() else { return };
    let mut conn = Conn::new(h, Peer::default(), None);
    let ctx = Ctx {
        caller: CallerKey::External,
        caller_name: "user".into(),
        obs: observe::shared().fresh(VIEW),
        cfg: h.config(),
        view: |_| observe::shared().fresh(VIEW),
    };
    let (done, failed) = run(&mut conn, &ctx, act, &marspot::control_deny::path());
    marspot::lx_event!(
        "HELPER_PANEL_ACT",
        "did what the user asked on the helper panel",
        session = act.sid,
        act = format!("{:?}", act.act),
        done = done,
        failed = failed
    );
}

/// How many panes it closed, or directories it changed, and how many
/// it could not.
pub(super) fn run(conn: &mut Conn, ctx: &Ctx, act: HelperPanelAct, deny: &std::path::Path) -> (u32, u32) {
    let sids = match act.act {
        PanelAct::CloseHelper => vec![act.sid],
        PanelAct::CloseAllOfParent => under(&ctx.obs, act.sid),
        PanelAct::ToggleCwdControl => {
            let ok = set_control(conn, ctx, act, deny);
            return (ok as u32, !ok as u32);
        }
        PanelAct::SetParent => {
            let out = super::ops_parent::set_sid(conn, ctx, act.sid, act.parent);
            if let Err(r) = &out {
                marspot::lx_warn!("shell.control.parent_refused", &r.message, session = act.sid, parent = act.parent);
            }
            return (out.is_ok() as u32, out.is_err() as u32);
        }
        PanelAct::Unknown(_) => return (0, 0),
    };
    let mut counts = (0, 0);
    let top = (act.act == PanelAct::CloseAllOfParent).then_some(act.sid);
    for sid in sids {
        let out = still_to_close(conn, ctx, sid, top).and_then(|()| close(conn, ctx, sid));
        match out {
            Ok(()) => counts.0 += 1,
            Err(r) => {
                counts.1 += 1;
                marspot::lx_warn!("shell.control.panel_close_failed", &r.message, session = sid, code = r.code.as_str());
            }
        }
    }
    counts
}

/// The panes were listed a while ago: one that has gone to a window
/// meanwhile, or to another pane when all of `top`'s are closed, is no
/// longer one the user meant.
///
/// Whose shadow it is, a window's included, is read from its registry
/// entry: its session writes that before the core moves the pane, and a
/// view of the sessions may be older than a move just done.  Either
/// saying it is no longer below `top` is enough.
fn still_to_close(conn: &Conn, ctx: &Ctx, sid: u64, top: Option<u64>) -> Result<(), Reject> {
    let obs = ctx.view_now();
    let parent = marspot_term::session_registry::read_session_entry(sid).ok().map(|e| e.shadow_of);
    let below = |top| shadow_tree::is_under(&parent_now(&obs, sid, parent), sid, top) && shadow_tree::is_under(&parent_in(&obs), sid, top);
    let why = if parent.or_else(|| obs.session(sid).map(|s| s.entry.shadow_of)) == Some(0) {
        "refused: in a window"
    } else if top.is_some_and(|top| !below(top)) {
        "refused: no longer below that pane"
    } else {
        return Ok(());
    };
    audit(conn, ctx, "pane.close", sid, None, false, why);
    Err(Reject::new(ErrorCode::BadRequest, why))
}

fn parent_in(obs: &Observation) -> impl Fn(u64) -> u64 + '_ {
    |sid| obs.session(sid).map_or(0, |s| s.entry.shadow_of)
}

/// [`parent_in`], with `sid`'s own parent as its entry has it, when read.
fn parent_now(obs: &Observation, sid: u64, entry: Option<u64>) -> impl Fn(u64) -> u64 + '_ {
    let view = parent_in(obs);
    move |s| if s == sid { entry.unwrap_or_else(|| view(s)) } else { view(s) }
}

/// A helper is stopped, which its plugin writes down; any other shadow
/// pane is closed as `pane.close` closes it.
fn close(conn: &mut Conn, ctx: &Ctx, sid: u64) -> Result<(), Reject> {
    match helpers_port::stop_sid(conn, ctx, sid) {
        Some(out) => out,
        None => ops_pane::close_shadow(conn, ctx, sid).map(drop),
    }
}

/// The shadow panes below `top`, the deepest first: one closed before
/// those below it would have them end on their own, unrecorded.
fn under(obs: &Observation, top: u64) -> Vec<u64> {
    let parent_of = parent_in(obs);
    let mut found: Vec<(usize, u64)> = obs
        .live()
        .map(|s| s.entry.id)
        .filter(|sid| *sid != top && shadow_tree::is_under(&parent_of, *sid, top))
        .map(|sid| (shadow_tree::root_and_depth(&parent_of, sid).1, sid))
        .collect();
    found.sort_by_key(|(depth, sid)| (std::cmp::Reverse(*depth), *sid));
    found.into_iter().map(|(_, sid)| sid).collect()
}

/// Deny or allow the directory pane `act.sid` is in, as wanted.
fn set_control(conn: &Conn, ctx: &Ctx, act: HelperPanelAct, deny: &std::path::Path) -> bool {
    let sid = act.sid;
    let want = match act.want {
        CwdControl::Flip => None,
        CwdControl::Deny => Some(true),
        CwdControl::Allow => Some(false),
        CwdControl::Unknown(_) => return false,
    };
    let Some(cwd) = names::cwd_of(sid, &ctx.obs) else {
        marspot::lx_warn!("shell.control.panel_no_cwd", "the pane's directory is not known", session = sid);
        return false;
    };
    let (ok, result) = match control_deny::set(deny, &cwd, want) {
        Ok(Outcome::Denied) => (true, "denied".to_string()),
        Ok(Outcome::Allowed) => (true, "allowed".to_string()),
        Ok(Outcome::Unchanged) => (true, "unchanged".to_string()),
        Ok(Outcome::CoveredBy(line)) => (false, format!("refused: denied by the line {line}")),
        Err(e) => {
            marspot::lx_warn!("shell.control.panel_deny_failed", &format!("{e}"), session = sid);
            (false, "failed".to_string())
        }
    };
    audit(conn, ctx, "control.deny", sid, Some(cwd.into_bytes()), false, &result);
    ok
}
