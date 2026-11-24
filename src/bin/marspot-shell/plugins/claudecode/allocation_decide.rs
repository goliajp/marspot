//! The plan and each pane's would-be move, worked out and written
//! down, never acted on.

use super::allocation_adapter::Allocation;
use super::allocation_inputs::{PaneIn, Snapshot};
use super::allocation_line::{kappa_check, plan_line, say};
use crate::plugins::allocation::choose::{HELPER_LAST_H, MUST_LAST_H, Mover, loads_of, pick};
use crate::plugins::allocation::notice::{Stranded, stranded};
use crate::plugins::allocation::plan::{Plan, plan};
use crate::plugins::allocation::rest::{RESTRICTED_USER, RestMove, Resting, is_keeper, rest_move};
use crate::plugins::allocation::{Class, KAPPA_FABLE, Quant, RHO0, shortage};

/// A feed older than this stops plan-following moves.
pub(super) const STALE_FEED_S: i64 = 15 * 60;

/// A pane marked as needing Fable is kept off Fable-less accounts this long.
const KEEP_MARKED_S: i64 = 24 * 3600;

pub(super) struct Pane<'a> {
    pub p: &'a PaneIn,
    /// Restricted and plain speed, weeks an hour.
    pub f: f64,
    pub n: f64,
    pub known: bool,
}

impl Pane<'_> {
    pub(super) fn rate(&self) -> f64 {
        self.f + self.n
    }
}

/// Everything one scan's decisions read.
struct Scene<'a> {
    now: i64,
    accounts: Vec<Quant>,
    plan: Plan,
    table: Vec<(Option<u8>, Class, f64)>,
    panes: Vec<Pane<'a>>,
    demand: f64,
    stale_feed: bool,
}

pub(super) fn decide(a: &mut Allocation, snap: &Snapshot, log: &mut Vec<(&'static str, String)>) {
    let Some(feed_at) = snap.feed_at else { return };
    let now = snap.now;
    let stale_feed = now - feed_at > STALE_FEED_S;
    let word = if stale_feed { "the feed is over 15 min old: no plan-following moves" } else { "the feed is current" }.to_string();
    say(a, log, "alloc.stale_feed", word, stale_feed);
    kappa_check(a, snap, log);
    let s = scene(a, snap, stale_feed);
    let short = shortage(&s.accounts, s.demand);
    a.band_moves.retain(|t| now - t < 3600);
    let mut moves = Vec::new();
    for i in 0..s.panes.len() {
        let to = would_move(a, &s, i);
        moves.push(to.map(|(t, why)| format!("{}→P{t}({why})", s.panes[i].p.sid)));
        note_move(a, &s, i, to, log);
    }
    let line = plan_line(&s.accounts, &s.plan, &s.table, short, &moves);
    say(a, log, "alloc.plan", line, true);
}

fn scene<'a>(a: &Allocation, snap: &'a Snapshot, stale_feed: bool) -> Scene<'a> {
    let now = snap.now;
    let panes: Vec<Pane> = snap
        .panes
        .iter()
        .map(|p| {
            let (f, n, known) = a.used.get(&p.sid).map_or((0.0, RHO0, false), |u| u.rate_or_default(now));
            Pane { p, f, n, known }
        })
        .collect();
    let accounts: Vec<Quant> = snap
        .accounts
        .iter()
        .map(|ac| ac.quant(now, panes.iter().filter(|x| x.p.profile == ac.profile).map(Pane::rate).sum()))
        .collect();
    // the Fable readings also see what Opus panes' background agents use
    let pane_f: f64 = panes.iter().filter(|x| x.p.class == Class::Restricted).map(Pane::rate).sum();
    let slope_f: f64 = a.slopes.values().filter_map(|s| s.1.rate()).map(|r| r.max(0.0) * KAPPA_FABLE).sum();
    let b_f = pane_f.max(slope_f);
    let b_n: f64 = panes.iter().filter(|x| x.p.class == Class::Plain).map(Pane::rate).sum();
    let plan = plan(&accounts, b_f, b_n, Some(KAPPA_FABLE));
    let table = panes.iter().map(|x| (Some(x.p.profile), x.p.class, x.rate())).collect();
    Scene { now, accounts, plan, table, panes, demand: b_f + b_n, stale_feed }
}

fn would_move(a: &Allocation, s: &Scene, i: usize) -> Option<(u8, &'static str)> {
    let x = &s.panes[i];
    let mover = Mover { class: x.p.class, rate: x.rate() };
    if x.p.out {
        let horizon = if x.p.helper { HELPER_LAST_H } else { MUST_LAST_H };
        return pick(&s.accounts, &s.plan, &loads_of(&s.accounts, &s.table, Some(i)), mover, Some(x.p.profile), horizon).map(|t| (t, "out"));
    }
    if !x.p.resting {
        return None;
    }
    let hours = |at: i64| (s.now - at) as f64 / 3600.0;
    let loads = loads_of(&s.accounts, &s.table, None);
    let last = a.last.get(&x.p.sid);
    let left: Vec<(u8, f64)> = last.and_then(|l| l.left).map(|(id, at)| (id, hours(at))).into_iter().collect();
    let same = s.panes.iter().filter(|y| y.p.sid != x.p.sid && y.p.profile == x.p.profile).map(|y| (y.p.sid, y.rate()));
    let marked = x.p.marked_at.is_some_and(|at| s.now - at < KEEP_MARKED_S);
    let r = Resting {
        here: x.p.profile,
        mover,
        loads: &loads,
        keeper: is_keeper((x.p.sid, x.rate()), same),
        since_move_h: last.and_then(|l| l.bound_at).map(hours),
        left: &left,
        band_last_hour: a.band_moves.len(),
        demand: s.demand,
        stale_feed: s.stale_feed,
        speed_known: x.known,
        keeps_restricted: marked || x.f > RESTRICTED_USER,
    };
    match rest_move(&s.accounts, &s.plan, &r)? {
        RestMove::LeaveEarly(t) => Some((t, "leave_early")),
        RestMove::Band(t) => Some((t, "plan")),
    }
}

/// One line when a pane's would-be target changes to a new account,
/// with the inputs that chose it; one notice per stretch with nowhere to go.
fn note_move(a: &mut Allocation, s: &Scene, i: usize, to: Option<(u8, &'static str)>, log: &mut Vec<(&'static str, String)>) {
    let x = &s.panes[i];
    let sid = x.p.sid;
    let was = a.last.get(&sid).and_then(|l| l.would);
    if let Some((t, why)) = to
        && was != Some(t)
    {
        let detail = format!("to=P{t} why={why} rate={:.5} restricted={:.5} known={} demand={:.5}", x.rate(), x.f, x.known, s.demand);
        a.event(s.now, sid, "would_move", Some(x.p.profile), x.p.class, detail, log);
        if why == "plan" {
            a.band_moves.push_back(s.now);
        }
    }
    if let Some(l) = a.last.get_mut(&sid) {
        l.would = to.map(|(t, _)| t);
    }
    let stuck = if x.p.out && to.is_none() { stranded(&s.accounts, x.p.class) } else { None };
    if let Some(st) = a.told.tell(sid, stuck) {
        let line = match st {
            Stranded::AllOut { back_in_h } => format!("sid={sid}: every account is out, the first back in {back_in_h:.1} h"),
            Stranded::RestrictedOut { back_in_h } => format!("sid={sid}: Fable is out on every account, back in {back_in_h:.1} h; the model stays"),
        };
        log.push(("alloc.notice", line));
    }
}
