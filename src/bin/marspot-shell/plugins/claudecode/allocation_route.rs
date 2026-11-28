//! The pane plan's routing table: built on every plan pass and read
//! where new work is placed (a helper started on Claude, a pane that
//! appears, a refused pane, a badge click), each placement logged as
//! `sched.route.shadow` next to the account today's rule chose.  Each
//! placement is also noted for the go-live ladder, with whether it put
//! an other-model newcomer on an account holding only Fable-usable
//! quota.  Once the ladder has the table live (step A), helpers, refused
//! panes and badge clicks go where it says; a pane that appears was
//! started already and is only shadowed.
//!
//! A placement takes its expected use out of the table at once, so a
//! batch of helpers spreads over the entries.  A helper or a young pane
//! counts in the plan only at fleet level, so each placement is kept
//! for its expected life (at most `KEEP_S`) and taken out of every table
//! a pass builds until then.
//!
//! Each full pass also places one probe newcomer of each class without
//! keeping it (`probe`), noted for the ladder like a real placement.

use marspot_agent::sched::forecast::Forecast;
use marspot_agent::sched::level2::route::{Newcomer, OTHERS_CAP, Routing};
use marspot_agent::sched::level2::{H2, Input, Plan};

use crate::host::published::Published;
use crate::plugins::allocation::{Class, RHO0};

/// A placement is taken out of the tables this long at most, seconds:
/// a pane is in the plan's input once it is half an hour old.
const KEEP_S: i64 = 1800;
/// Placements kept at most, and notes for the ladder and live
/// placements awaiting the worker.
const KEPT_CAP: usize = 64;
const NOTED_CAP: usize = 64;
/// A helper's life when nothing has been measured, hours: the forecast's floor.
const HELPER_LIFE_H: f64 = 10.0 / 60.0;
/// A newcomer's high band over its level, by class: the forecast's
/// default band.
fn high_of(class: Class) -> f64 {
    if class == Class::Restricted { 2.5 } else { 1.5 }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Route {
    table: Routing,
    kept: Vec<(i64, u8, Newcomer)>,
    helper_life_h: f64,
    /// The other agents' line as last read.
    others: Vec<(&'static str, f64)>,
    built: bool,
    /// The ladder has the table place live work.
    live: bool,
    /// Placements not yet counted by the ladder: whether each went wrong.
    noted: Vec<bool>,
    /// Live placements of the last hour: when, and the account.
    live_placed: Vec<(i64, u8)>,
}

static ROUTE: Published<Route> = Published::new();

/// A pass's table from its plan, with the placements still kept and
/// the other agents' line: `others` when read on this pass (a full
/// search reads their feeds), else the last one read.
pub(super) fn rebuild(input: &Input, plan: &Plan, forecast: Option<&Forecast>, others: Option<Vec<(&'static str, f64)>>, now: i64) {
    let life = forecast.and_then(|f| f.helpers.iter().find(|h| h.agent == "claude")).map_or(HELPER_LIFE_H, |h| f64::from(h.p75_s) / 3600.0);
    let mut table = Routing::of(input, plan);
    ROUTE.update(|r| {
        r.kept.retain(|(at, _, m)| now - at < KEEP_S.min((m.life_h * 3600.0) as i64));
        for (_, to, m) in &r.kept {
            table.take(*to, m);
        }
        if let Some(o) = others {
            r.others = o;
        }
        r.table = table.with_others(&r.others);
        r.helper_life_h = life;
        r.built = true;
    });
}

/// The table as one line, for the plan's log.
pub(super) fn line() -> Option<String> {
    ROUTE.get().filter(|r| r.built).map(|r| r.table.line())
}

/// Where the table places a newcomer of `class` at `rate` (weeks an
/// hour) for `life_h`, leaving `from` when it moves.  The table's account
/// and its room before and after, in points, or `-` without a table or an
/// entry.  A new placement is kept; a pane that moves is not: it is in
/// the plan's input already, on the account it leaves, and the next
/// input has it on the one it went to, so its use only comes out of this
/// table.
fn place(class: Class, rate: f64, life_h: Option<f64>, from: Option<u8>, now: i64) -> (String, Option<u8>) {
    ROUTE.update(|r| {
        if !r.built {
            return ("table=-".into(), None);
        }
        let m = Newcomer { class, rate, high: rate * high_of(class), life_h: life_h.unwrap_or(r.helper_life_h) };
        match r.table.place(&m, from) {
            Some(p) => {
                if from.is_none() {
                    if r.kept.len() >= KEPT_CAP {
                        r.kept.remove(0);
                    }
                    r.kept.push((now, p.to, m));
                }
                if r.noted.len() < NOTED_CAP {
                    r.noted.push(class == Class::Plain && !p.plain_only);
                }
                (format!("table=P{} room={:.2}>{:.2}", p.to, 100.0 * p.before, 100.0 * p.after), Some(p.to))
            }
            None => ("table=-".into(), None),
        }
    })
}

/// One probe newcomer of each class placed through the table as a
/// helper would be, at the default rate and the helper life, and noted
/// for the ladder like a real placement; the table and its kept
/// placements are left as they were.  Run once per full plan pass, so
/// the ladder's count does not wait on the person starting work.  The
/// lines for `sched.route.shadow`.
pub(super) fn probe() -> Vec<String> {
    ROUTE.update(|r| {
        if !r.built {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for class in [Class::Plain, Class::Restricted] {
            let m = Newcomer { class, rate: RHO0, high: RHO0 * high_of(class), life_h: r.helper_life_h };
            let placed = match r.table.clone().place(&m, None) {
                Some(p) => {
                    if r.noted.len() < NOTED_CAP {
                        r.noted.push(class == Class::Plain && !p.plain_only);
                    }
                    format!("table=P{} room={:.2}>{:.2}", p.to, 100.0 * p.before, 100.0 * p.after)
                }
                None => "table=-".into(),
            };
            lines.push(format!("kind=probe class={} {placed}", class_word(class)));
        }
        lines
    })
}

/// The ladder's say on whether the table places live work.
pub(super) fn set_live(on: bool) {
    ROUTE.update(|r| r.live = on);
}

fn live() -> bool {
    ROUTE.get().is_some_and(|r| r.live)
}

/// The placements noted since the last call, oldest first: whether each
/// went wrong.
pub(super) fn take_noted() -> Vec<bool> {
    ROUTE.update(|r| std::mem::take(&mut r.noted))
}

/// The live placements of the last hour: when, and the account.
pub(super) fn live_placed(now: i64) -> Vec<(i64, u8)> {
    ROUTE.update(|r| {
        r.live_placed.retain(|(at, _)| now - at < 3600);
        r.live_placed.clone()
    })
}

/// `to`, the table's placement, when the table is live.  The caller
/// still filters it, so it is not kept here; see `used`.
fn live_pick(to: Option<u8>) -> Option<u8> {
    to.filter(|_| live())
}

/// A live placement on `to` the caller acted on: kept for the ladder's
/// check of refusals after it.  A pick the caller filtered out placed
/// nothing and is not counted.
pub(in crate::plugins) fn used(to: u8, now: i64) {
    ROUTE.update(|r| {
        if r.live_placed.len() >= NOTED_CAP {
            r.live_placed.remove(0);
        }
        r.live_placed.push((now, to));
    });
}

fn class_word(class: Class) -> &'static str {
    if class == Class::Restricted { "fable" } else { "plain" }
}

fn today(to: Option<u8>) -> String {
    to.map_or("-".into(), |t| format!("P{t}"))
}

/// The other agents with accounts here, each with what its best open,
/// freshly read account has left of its week, weeks.  Reads their feeds.
pub(super) fn others_now(now: i64) -> Vec<(&'static str, f64)> {
    use crate::plugins::handoff::{Agent, profiles};
    use crate::plugins::claudecode::quota::helper_pick::{Accounts, rooms_scene};
    let best = |a: &Accounts| rooms_scene(&a.rooms, &a.resets, None, now).accounts.iter().filter(|q| q.open && !q.stale).map(|q| q.r).fold(0.0, f64::max);
    let numbers = |agent| profiles(agent).iter().map(|(n, _)| *n).collect::<Vec<u8>>();
    let (codex, kimi) = (numbers(Agent::Codex), numbers(Agent::Kimi));
    let mut v = Vec::new();
    if !codex.is_empty() {
        v.push(("codex", best(&crate::plugins::usage::for_helpers::codex_accounts(&codex, marspot::cc_usage::read_codex(), now))));
    }
    if !kimi.is_empty() {
        v.push(("kimi", best(&crate::plugins::kimi::accounts::accounts_now(&kimi, now))));
    }
    v
}

/// A Claude helper today's rule would start on `to`, with the other
/// agents' line; the table's account when the table is live.  Reads the
/// other agents' feeds: off the shell's main loop.
pub(in crate::plugins) fn shadow_helper(class: Class, rate: f64, to: Option<u8>, now: i64) -> Option<u8> {
    let others = others_now(now);
    // which agent the table would send it to, were the agent left open
    let agent = ROUTE.update(|r| {
        r.others.clone_from(&others);
        r.table.others = others.iter().take(OTHERS_CAP).copied().collect();
        if !r.built {
            return "-";
        }
        let m = Newcomer { class, rate, high: rate * high_of(class), life_h: r.helper_life_h };
        r.table.agent_for(&m).unwrap_or("claude")
    });
    let (placed, picked) = place(class, rate, None, None, now);
    let others: Vec<String> = others.iter().map(|(a, l)| format!("{a}={:.1}", 100.0 * l)).collect();
    let line = format!("kind=helper class={} today={} {placed} agent={agent} others={}", class_word(class), today(to), if others.is_empty() { "-".into() } else { others.join(",") });
    marspot::lx_info!("sched.route.shadow", &line);
    live_pick(picked)
}

/// A pane that appeared on `on`, or a pane a badge click or a refusal
/// moves off `from` to `to`: the line, and the table's account when the
/// table is live and the pane moves.
pub(super) fn shadow_pane(kind: &str, sid: u64, class: Class, rate: f64, from: Option<u8>, to: Option<u8>, now: i64) -> (String, Option<u8>) {
    let rate = if rate > 0.0 { rate } else { RHO0 };
    let (placed, picked) = place(class, rate, Some(H2), from, now);
    let line = format!("kind={kind} sid={sid} class={} today={} {placed}", class_word(class), today(to));
    (line, if from.is_some() { live_pick(picked) } else { None })
}

#[cfg(test)]
#[path = "allocation_route_tests.rs"]
mod tests;
