//! Whose child a pane is: the one place every change to it is checked,
//! whatever asked for it (`pane.parent`, the sidebar's menu, a move into
//! the shadows), and where a new pane's parent is settled.
//!
//! Panes nest two levels at most: a parent is a top pane, and a pane
//! with children of its own is never made a child.  A new pane asked
//! for under a child opens under that child's top pane instead.

use std::time::Duration;

use marspot::shell_proto::{CAP_SHADOWS, MsgType, PanePlace, PlaceWhere, encode_pane_place};
use marspot_peer::control::{ErrorCode, Reject};
use marspot_peer::json::{Value, obj};

use super::config::Config;
use super::conn::{ANSWER_WITHIN, Conn};
use super::dispatch::{Ctx, audit, target};
use super::ops_pane::{Again, ask_done_within};
use super::shadow_gate::{GATE_WITHIN, Tree};
use super::shadow_tree;

pub(super) const OWN_PARENT: &str = "a pane cannot be its own parent";
pub(super) const PARENT_IS_CHILD: &str = "a child pane cannot have child panes; pick a top pane";
pub(super) const HAS_CHILDREN: &str = "a pane with child panes cannot become a child pane";

/// Why `sid` cannot become the child of `parent` (0: a top pane, which
/// any pane may become), if it cannot: the two-level rule, a loop, and
/// the count limits.
pub(super) fn check(cfg: &Config, tree: &Tree, sid: u64, parent: u64) -> Result<(), Reject> {
    if parent == 0 {
        return Ok(());
    }
    let refuse = |why: &str| Err(Reject::new(ErrorCode::BadRequest, why));
    if parent == sid {
        return refuse(OWN_PARENT);
    }
    if tree.parent_of(parent) != 0 {
        return refuse(PARENT_IS_CHILD);
    }
    if tree.has_children(sid) {
        return refuse(HAS_CHILDREN);
    }
    shadow_tree::check_adopt(cfg, &|s| tree.parent_of(s), &tree.shadows(), parent, sid)
}

/// The parent a new pane asked for under `parent` gets: `parent`'s top
/// pane, which is `parent` itself unless it is a child.
pub(super) fn for_new_pane(tree: &Tree, parent: u64) -> u64 {
    let top = shadow_tree::root_and_depth(&|s| tree.parent_of(s), parent).0;
    if top != parent {
        marspot::lx_event!("PANE_PARENT_LIFTED", "a new pane asked for under a child pane opens under its top pane",
            asked = parent, parent = top);
    }
    top
}

/// `pane.parent`: make `t` the child of `parent`, or a top pane
/// (`None`), where it is shown.
pub(super) fn set(conn: &mut Conn, ctx: &Ctx, t: &str, parent: Option<&str>) -> Result<Value, Reject> {
    let sid = target(ctx, t)?;
    let parent = parent.map(|p| target(ctx, p)).transpose()?.unwrap_or(0);
    set_sid(conn, ctx, sid, parent)
}

pub(super) fn set_sid(conn: &mut Conn, ctx: &Ctx, sid: u64, parent: u64) -> Result<Value, Reject> {
    conn.need_caps(CAP_SHADOWS)?;
    let gate = conn.hub.shadow_gate.enter(GATE_WITHIN)?;
    let checked = check(&ctx.cfg, &gate.tree(&ctx.view_now()), sid, parent);
    if let Err(r) = checked {
        audit(conn, ctx, "pane.parent", sid, None, false, &format!("refused: {}", r.message));
        return Err(r);
    }
    // the core waits up to ANSWER_WITHIN for the session, then answers itself
    let out = ask_done_within(
        conn,
        sid,
        MsgType::PanePlace,
        |req_id| Ok(encode_pane_place(&PanePlace { req_id, sid, to: PlaceWhere::Parent, host_sid: parent, force: false })),
        Again::No,
        ANSWER_WITHIN + Duration::from_secs(2),
    );
    if out.as_ref().map_or_else(|r| r.code == ErrorCode::Unknown, |_| true) {
        gate.record(sid, parent);
    }
    audit(conn, ctx, "pane.parent", sid, None, false, out.as_ref().map_or_else(|r| r.code.as_str(), |_| "set"));
    out.map(|_| obj([("id", (sid as i64).into()), ("parent", if parent == 0 { Value::Null } else { (parent as i64).into() })]))
}
