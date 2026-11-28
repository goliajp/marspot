//! What the shared core would do with each codex pane, written down
//! and never acted on.
//!
//! Codex's model caps are not known to count against the week, so
//! nothing is reserved for Spark (`κ` unknown): its window only says
//! whether a Spark pane may go somewhere.  An account a pane's rollout
//! has a newer reading for than the feed is judged by that reading.

use super::alloc::{AccountIn, CodexAlloc, HeadsUp, Log, PaneIn, Snapshot, WEEK_S, class_of};
use crate::plugins::allocation::choose::{HELPER_LAST_H, MUST_LAST_H, Mover, loads_of};
use crate::plugins::allocation::notice::{Stranded, stranded};
use crate::plugins::allocation::plan::{Plan, plan};
use crate::plugins::allocation::replay::{Inputs, Kind, Rule, Why};
use crate::plugins::allocation::rest::{LEAVE_BELOW_H, RESTRICTED_USER, is_keeper};
use crate::plugins::allocation::speed::default_rate;
use crate::plugins::allocation::{Class, Load, Quant, lasts};

/// How fast pane `p` uses its account's week, as a fraction an hour.
pub(super) fn pane_rate(a: &CodexAlloc, p: &PaneIn, now: i64) -> f64 {
    let class = class_of(p);
    let (f, n, _) = a.used.get(&p.sid).map_or(default_rate(class), |u| u.rate_or_default(now, class));
    f + n
}

/// The rate a plain pane nothing has measured is taken at.
pub(super) fn plain_rate() -> f64 {
    let (f, n, _) = default_rate(Class::Plain);
    f + n
}

/// A reading older than this is pushed forward by the panes' speeds.
const STALE_ACCOUNT_S: i64 = 30 * 60;

/// A feed older than this stops plan-following moves.
const STALE_FEED_S: i64 = 15 * 60;

/// Renewals this close are the same week (two reads can differ by a second).
const SAME_WEEK_S: i64 = 86_400;

struct Pane<'a> {
    p: &'a PaneIn,
    f: f64,
    n: f64,
    known: bool,
    /// Refused since its last good turn, after its codex started.
    refused: bool,
}

impl Pane<'_> {
    fn rate(&self) -> f64 {
        self.f + self.n
    }
}

struct Scene<'a> {
    now: i64,
    accounts: Vec<Quant>,
    b_f: f64,
    b_n: f64,
    plan: Plan,
    table: Vec<(Option<u8>, Class, f64)>,
    panes: Vec<Pane<'a>>,
    stale_feed: bool,
}

/// The core's view of an account at `now`: the newer of the feed's and
/// a rollout's week reading, pushed forward by `rate_on` when old.
pub(super) fn quant(a: &AccountIn, rollout: Option<(i64, f64, i64)>, now: i64, rate_on: f64) -> Quant {
    let (mut w, mut renews, at) = match rollout {
        Some((at, used, renews)) if at > a.measured_at => (used, renews, at),
        _ => (a.week_used, a.week_renews, a.measured_at),
    };
    if renews <= now {
        (w, renews) = (0.0, renews + WEEK_S);
    }
    let age = now - at;
    let stale = age > STALE_ACCOUNT_S;
    if stale {
        w += rate_on * age as f64 / 3600.0;
    }
    let r = (1.0 - w).max(0.0);
    let hours = |t: i64| (t - now) as f64 / 3600.0;
    let (f, tf) = match a.spark {
        Some((used, at)) if at > now => (if used < 1.0 { r } else { 0.0 }, hours(at)),
        _ => (r, hours(renews)),
    };
    let q = Quant { id: a.profile, r, f, t: hours(renews), tf, open: a.open, stale };
    marspot_agent::sched::reset::with_supply(&q, &Load { restricted: 0.0, plain: rate_on }, 1.0, &a.supply)
}

/// Decisions are made again at least this often with unchanged inputs,
/// for what time alone changes (a renewal, an old reading).
const DECIDE_EVERY_S: i64 = 60;

pub(super) fn decide(a: &mut CodexAlloc, snap: &Snapshot, log: &mut Log) -> Vec<HeadsUp> {
    let Some(feed_at) = snap.feed_at else { return Vec::new() };
    let panes = panes_of(a, snap);
    let key = key_of(a, snap, &panes);
    if a.decided == Some(key) {
        return Vec::new();
    }
    a.decided = Some(key);
    let s = scene(a, snap, panes, snap.now - feed_at > STALE_FEED_S);
    a.band_moves.retain(|t| s.now - t < 3600);
    let mut moves = Vec::new();
    let mut tell = Vec::new();
    for i in 0..s.panes.len() {
        let inputs = inputs(a, &s, i);
        let to = inputs.as_ref().and_then(|x| x.decide(&s.plan));
        if let Some(x) = &inputs {
            a.sched.decide("codex", s.panes[i].p.sid, x, Rule::Bands, to, s.now);
        }
        moves.push(to.map(|(t, why)| format!("{}→P{t}({})", s.panes[i].p.sid, why.word())));
        note_move(a, &s, i, to, inputs.as_ref(), log);
        tell.extend(heads_up(a, &s, i, log));
    }
    let line: Vec<String> = moves.into_iter().flatten().collect();
    let line = if line.is_empty() { "no pane would move".to_string() } else { line.join(" ") };
    if a.said.as_deref() != Some(line.as_str()) {
        log.push(("codex.alloc.plan", line.clone()));
        a.said = Some(line);
    }
    tell
}

fn panes_of<'a>(a: &CodexAlloc, snap: &'a Snapshot) -> Vec<Pane<'a>> {
    let now = snap.now;
    snap.panes
        .iter()
        .map(|p| {
            let class = class_of(p);
            let (f, n, known) = a.used.get(&p.sid).map_or(default_rate(class), |u| u.rate_or_default(now, class));
            let l = a.last.get(&p.sid);
            let refused = a.refusals.get(&p.sid).is_some_and(|r| r.at >= p.started_at && l.and_then(|l| l.done_at).is_none_or(|d| d < r.at));
            Pane { p, f, n, known, refused }
        })
        .collect()
}

/// Everything a decision reads, hashed, with the time to the minute.
fn key_of(a: &CodexAlloc, snap: &Snapshot, panes: &[Pane]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (snap.now / DECIDE_EVERY_S, snap.feed_at).hash(&mut h);
    for ac in &snap.accounts {
        (ac.profile, ac.week_used.to_bits(), ac.week_renews, ac.spark.map(|(u, at)| (u.to_bits(), at)), ac.open, ac.measured_at).hash(&mut h);
        a.limits.get(&ac.profile).map(|(at, used, renews)| (*at, used.to_bits(), *renews)).hash(&mut h);
    }
    for x in panes {
        (x.p.sid, x.p.profile, x.p.idle, x.p.helper, x.p.restricted, x.refused, x.f.to_bits(), x.n.to_bits(), x.known).hash(&mut h);
    }
    h.finish()
}

fn scene<'a>(a: &CodexAlloc, snap: &'a Snapshot, panes: Vec<Pane<'a>>, stale_feed: bool) -> Scene<'a> {
    let now = snap.now;
    let rate_on = |id: u8| panes.iter().filter(|x| x.p.profile == id).map(Pane::rate).sum::<f64>();
    let accounts: Vec<Quant> = snap
        .accounts
        .iter()
        .map(|ac| {
            let mut q = quant(ac, a.limits.get(&ac.profile).copied(), now, rate_on(ac.profile));
            // an account that refused a pane does not take new work
            q.open &= !panes.iter().any(|x| x.refused && x.p.profile == ac.profile);
            q
        })
        .collect();
    let b_f = panes.iter().map(|x| x.f).sum();
    let b_n = panes.iter().map(|x| x.n).sum();
    let plan = plan(&accounts, b_f, b_n, None);
    let table = panes.iter().map(|x| (Some(x.p.profile), class_of(x.p), x.rate())).collect();
    Scene { now, accounts, b_f, b_n, plan, table, panes, stale_feed }
}

/// The pane must move: refused, or its account is spent for it.
fn out(s: &Scene, x: &Pane) -> bool {
    x.refused || s.accounts.iter().find(|q| q.id == x.p.profile).is_some_and(|q| !q.accepts(class_of(x.p)))
}

/// What the decision about pane `i` reads, or `None` when there is none
/// to make (busy and able to work, or on an account the feed does not read).
fn inputs(a: &CodexAlloc, s: &Scene, i: usize) -> Option<Inputs> {
    let x = &s.panes[i];
    s.accounts.iter().find(|q| q.id == x.p.profile)?;
    let mover = Mover { class: class_of(x.p), rate: x.rate(), tokens: None, fable: None };
    let base = |loads, kind| Inputs { accounts: s.accounts.clone(), b_f: s.b_f, b_n: s.b_n, kappa: None, loads, mover, here: x.p.profile, kind };
    if out(s, x) {
        let horizon_h = if x.p.helper { HELPER_LAST_H } else { MUST_LAST_H };
        return Some(base(loads_of(&s.accounts, &s.table, Some(i)), Kind::Out { horizon_h }));
    }
    if !x.p.idle {
        return None;
    }
    let hours = |at: i64| (s.now - at) as f64 / 3600.0;
    let last = a.last.get(&x.p.sid);
    let same = s.panes.iter().filter(|y| y.p.sid != x.p.sid && y.p.profile == x.p.profile && class_of(y.p) == mover.class).map(|y| (y.p.sid, y.rate()));
    let kind = Kind::Rest {
        keeper: is_keeper((x.p.sid, x.rate()), same),
        since_move_h: last.and_then(|l| l.bound_at).map(hours),
        left: last.and_then(|l| l.left).map(|(id, at)| (id, hours(at))).into_iter().collect(),
        band_last_hour: a.band_moves.len(),
        demand: s.b_f + s.b_n,
        stale_feed: s.stale_feed,
        speed_known: x.known,
        keeps_restricted: x.f > RESTRICTED_USER,
        moves_week: last.map_or(0, |l| l.rest_moves.iter().filter(|at| s.now - **at < WEEK_S).count()),
    };
    Some(base(loads_of(&s.accounts, &s.table, None), kind))
}

/// A line when a pane's would-be target changes to a new account, with
/// every input that chose it, counted against the limits once; one
/// notice per stretch with nowhere to go.
fn note_move(a: &mut CodexAlloc, s: &Scene, i: usize, to: Option<(u8, Why)>, inputs: Option<&Inputs>, log: &mut Log) {
    let x = &s.panes[i];
    let sid = x.p.sid;
    let was = a.last.get(&sid).and_then(|l| l.would);
    if let (Some((t, why)), Some(inputs)) = (to, inputs)
        && was != Some(t)
    {
        let detail = format!("to=P{t} why={} {}", why.word(), inputs.encode());
        a.event(s.now, sid, "would_move", Some(x.p.profile), class_of(x.p), detail, log);
        a.count_move(s.now, sid, why);
    }
    if let (Some((t, _)), Some(l)) = (to, a.last.get_mut(&sid)) {
        l.would = Some(t);
    }
    let stuck = if out(s, x) && to.is_none() { stranded(&s.accounts, class_of(x.p)) } else { None };
    if let Some(st) = a.stranded.tell(sid, stuck) {
        let line = match st {
            Stranded::AllOut { back_in_h } => format!("sid={sid}: every codex account is out, the first back in {back_in_h:.1} h"),
            Stranded::RestrictedOut { back_in_h } => format!("sid={sid}: Spark is out on every codex account, back in {back_in_h:.1} h"),
        };
        log.push(("codex.alloc.notice", line));
    }
}

/// A busy pane whose account will not last two hours at the speeds on
/// it, and could go elsewhere, is told once per account and week.
fn heads_up(a: &mut CodexAlloc, s: &Scene, i: usize, log: &mut Log) -> Option<HeadsUp> {
    let x = &s.panes[i];
    let class = class_of(x.p);
    if x.p.idle || out(s, x) {
        return None;
    }
    let qi = s.accounts.iter().position(|q| q.id == x.p.profile)?;
    let q = &s.accounts[qi];
    let load: Load = loads_of(&s.accounts, &s.table, None)[qi];
    let hours = lasts(q, &load, class, 0.0);
    let elsewhere = s.accounts.iter().any(|o| o.id != q.id && o.accepts(class));
    let week = s.now + (q.t * 3600.0) as i64;
    let told = a.told.get(&(x.p.sid, q.id)).is_some_and(|w| (w - week).abs() < SAME_WEEK_S);
    if hours >= LEAVE_BELOW_H.min(q.t) || !elsewhere || told {
        return None;
    }
    a.event(s.now, x.p.sid, "told", Some(q.id), class, format!("week={week} lasts_h={hours:.2}"), log);
    a.note_told(x.p.sid, q.id, week);
    Some(HeadsUp { sid: x.p.sid, profile: q.id, hours })
}
