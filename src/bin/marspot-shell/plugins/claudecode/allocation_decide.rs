//! The plan and each pane's would-be move, worked out and written
//! down, never acted on.
//!
//! Each would-move is written with every input the pure decision read
//! (`allocation::replay`), so the log can be fed back to it.

use super::allocation_adapter::{Allocation, WEEK_S};
use super::allocation_inputs::{PaneIn, Snapshot};
use super::allocation_line::{kappa_check, plan_key, plan_line, say, say_keyed};
use super::allocation_view::View;
use super::fable_need::MARK_FOR;
use crate::plugins::allocation::choose::{HELPER_LAST_H, MUST_LAST_H, Mover, held_back, loads_of};
use crate::plugins::allocation::notice::{Stranded, stranded};
use crate::plugins::allocation::plan::Plan;
use crate::plugins::allocation::replay::{Inputs, Kind, Why};
use crate::plugins::allocation::rest::{RESTRICTED_USER, is_keeper};
use crate::plugins::allocation::speed::default_rate;
use crate::plugins::allocation::{Class, KAPPA_FABLE, Quant, shortage};

/// A feed older than this stops plan-following moves.
pub(super) const STALE_FEED_S: i64 = 15 * 60;

/// A pane whose need of Fable ended this recently is still kept off
/// Fable-less accounts.
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
    b_f: f64,
    b_n: f64,
    plan: Plan,
    table: Vec<(Option<u8>, Class, f64)>,
    panes: Vec<Pane<'a>>,
    stale_feed: bool,
    view: View,
}

/// Works out the plan and each pane's move, writes down what changed,
/// and hands back the view the main thread chooses by, with the resting
/// moves decided this scan; `None` when there is no feed.
pub(super) fn decide(a: &mut Allocation, snap: &Snapshot, log: &mut Vec<(&'static str, String)>) -> Option<View> {
    let feed_at = snap.feed_at?;
    let now = snap.now;
    let stale_feed = now - feed_at > STALE_FEED_S;
    let word = if stale_feed { "the feed is over 15 min old: no plan-following moves" } else { "the feed is current" }.to_string();
    say(a, log, "alloc.stale_feed", word, stale_feed);
    kappa_check(a, snap, log);
    let mut s = scene(a, snap, stale_feed);
    let short = shortage(&s.accounts, s.b_f + s.b_n);
    a.band_moves.retain(|t| now - t < 3600);
    let mut moves = Vec::new();
    for i in 0..s.panes.len() {
        let inputs = inputs(a, &s, i);
        let to = inputs.as_ref().and_then(|x| x.decide(&s.plan));
        moves.push(to.map(|(t, why)| format!("{}→P{t}({})", s.panes[i].p.sid, why.word())));
        note_move(a, &s, i, to, inputs.as_ref(), log);
        if let (Some((t, why)), Some(inputs)) = (to, inputs)
            && why.resting()
        {
            s.view.rest.insert(s.panes[i].p.sid, (t, why, inputs));
        }
    }
    let key = plan_key(&s.accounts, &s.table, short, &moves);
    let line = plan_line(&s.accounts, &s.plan, &s.table, short, &moves);
    say_keyed(a, log, "alloc.plan", key, line);
    Some(s.view)
}

fn scene<'a>(a: &Allocation, snap: &'a Snapshot, stale_feed: bool) -> Scene<'a> {
    let now = snap.now;
    let panes: Vec<Pane> = snap
        .panes
        .iter()
        .map(|p| {
            let (f, n, known) = a.used.get(&p.sid).map_or(default_rate(p.class), |u| u.rate_or_default(now, p.class));
            Pane { p, f, n, known }
        })
        .collect();
    let rates: Vec<(f64, f64)> = panes.iter().map(|x| (x.f, x.n)).collect();
    let slope_f: f64 = a.slopes.values().filter_map(|s| s.1.rate()).map(|r| r.max(0.0) * KAPPA_FABLE).sum();
    let view = View::from_rates(snap, &rates, slope_f);
    let plan = crate::plugins::allocation::plan::plan(&view.accounts, view.b_f, view.b_n, Some(KAPPA_FABLE));
    let table = panes.iter().map(|x| (Some(x.p.profile), x.p.class, x.rate())).collect();
    Scene { now, accounts: view.accounts.clone(), b_f: view.b_f, b_n: view.b_n, plan, table, panes, stale_feed, view }
}

/// What the decision about pane `i` reads, or `None` when there is none
/// to make (busy and able to work).
fn inputs(a: &Allocation, s: &Scene, i: usize) -> Option<Inputs> {
    let x = &s.panes[i];
    let mover = Mover { class: x.p.class, rate: x.rate() };
    let base = |loads, kind| Inputs { accounts: s.accounts.clone(), b_f: s.b_f, b_n: s.b_n, kappa: Some(KAPPA_FABLE), loads, mover, here: x.p.profile, kind };
    if x.p.out {
        let horizon_h = if x.p.helper { HELPER_LAST_H } else { MUST_LAST_H };
        return Some(base(loads_of(&s.accounts, &s.table, Some(i)), Kind::Out { horizon_h }));
    }
    if !x.p.resting {
        return None;
    }
    let hours = |at: i64| (s.now - at) as f64 / 3600.0;
    let last = a.last.get(&x.p.sid);
    let left: Vec<(u8, f64)> = last.and_then(|l| l.left).map(|(id, at)| (id, hours(at))).into_iter().collect();
    // the pane that stays when an account runs low is the slowest of its class
    let same = s.panes.iter().filter(|y| y.p.sid != x.p.sid && y.p.profile == x.p.profile && y.p.class == x.p.class).map(|y| (y.p.sid, y.rate()));
    // marked now, or the mark ended under a day ago
    let marked_since = x.p.marked_at.or(last.and_then(|l| l.marked_at));
    let marked = marked_since.is_some_and(|at| s.now - at < MARK_FOR.as_secs() as i64 + KEEP_MARKED_S);
    let kind = Kind::Rest {
        keeper: is_keeper((x.p.sid, x.rate()), same),
        since_move_h: last.and_then(|l| l.bound_at).map(hours),
        left,
        band_last_hour: a.band_moves.len(),
        demand: s.b_f + s.b_n,
        stale_feed: s.stale_feed,
        speed_known: x.known,
        keeps_restricted: marked || x.f > RESTRICTED_USER,
        moves_week: last.map_or(0, |l| l.rest_moves.iter().filter(|at| s.now - **at < WEEK_S).count()),
    };
    Some(base(loads_of(&s.accounts, &s.table, None), kind))
}

/// One line when a pane's would-be target changes to a new account,
/// with every input that chose it, counted against the limits once;
/// one notice per stretch with nowhere to go.
fn note_move(a: &mut Allocation, s: &Scene, i: usize, to: Option<(u8, Why)>, inputs: Option<&Inputs>, log: &mut Vec<(&'static str, String)>) {
    let x = &s.panes[i];
    let sid = x.p.sid;
    let was = a.last.get(&sid).and_then(|l| l.would);
    if let (Some((t, why)), Some(inputs)) = (to, inputs)
        && was != Some(t)
    {
        let detail = format!("to=P{t} why={} {}", why.word(), inputs.encode());
        a.event(s.now, sid, "would_move", Some(x.p.profile), x.p.class, detail, log);
        // in a dry run what live would have done counts against the
        // limits; live, the move made does
        if !a.live {
            a.count_move(s.now, sid, why);
        }
    }
    // the target stands until the pane binds or another account is
    // chosen: a pane going busy, or U jittering it out of a move, and
    // back is the same would-move, written and counted once
    if let (Some((t, _)), Some(l)) = (to, a.last.get_mut(&sid)) {
        l.would = Some(t);
    }
    let stuck = if x.p.out && to.is_none() { stranded(&s.accounts, x.p.class).or_else(|| held(&s.plan, inputs)) } else { None };
    if let Some(st) = a.told.tell(sid, stuck) {
        let line = match st {
            Stranded::AllOut { back_in_h } => format!("sid={sid}: every account is out, the first back in {back_in_h:.1} h"),
            Stranded::RestrictedOut { back_in_h } => format!("sid={sid}: Fable is out on every account, back in {back_in_h:.1} h; the model stays"),
            Stranded::HeldBack { would } => format!("sid={sid}: waits rather than take what the Fable panes are short of on P{would}"),
        };
        log.push(("alloc.notice", line));
    }
}

/// A plain pane with nowhere to go only because Fable is short.
fn held(plan: &Plan, inputs: Option<&Inputs>) -> Option<Stranded> {
    let x = inputs?;
    let Kind::Out { horizon_h } = x.kind else { return None };
    held_back(&x.accounts, plan, &x.loads, x.mover, Some(x.here), horizon_h).map(|would| Stranded::HeldBack { would })
}
