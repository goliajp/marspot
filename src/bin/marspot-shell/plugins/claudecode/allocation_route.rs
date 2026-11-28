//! The pane plan's routing table, in shadow: built on every plan pass
//! and read where new work is placed (a helper started on Claude, a
//! pane that appears, a badge click), each placement logged as
//! `sched.route.shadow` next to the account today's rule chose.  Nothing
//! is placed differently.
//!
//! A placement takes its expected use out of the table at once, so a
//! batch of helpers spreads over the entries.  A helper or a young pane
//! counts in the plan only at fleet level, so each placement is kept
//! for its expected life (at most `KEEP_S`) and taken out of every table
//! a pass builds until then.

use marspot_agent::sched::forecast::Forecast;
use marspot_agent::sched::level2::route::{Newcomer, Routing};
use marspot_agent::sched::level2::{H2, Input, Plan};

use crate::host::published::Published;
use crate::plugins::allocation::{Class, RHO0};

/// A placement is taken out of the tables this long at most, seconds:
/// a pane is in the plan's input once it is half an hour old.
const KEEP_S: i64 = 1800;
/// Placements kept at most.
const KEPT_CAP: usize = 64;
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
    built: bool,
}

static ROUTE: Published<Route> = Published::new();

/// A pass's table from its plan, with the placements still kept.
pub(super) fn rebuild(input: &Input, plan: &Plan, forecast: Option<&Forecast>, now: i64) {
    let life = forecast.and_then(|f| f.helpers.iter().find(|h| h.agent == "claude")).map_or(HELPER_LIFE_H, |h| f64::from(h.p75_s) / 3600.0);
    let mut table = Routing::of(input, plan);
    ROUTE.update(|r| {
        r.kept.retain(|(at, _, m)| now - at < KEEP_S.min((m.life_h * 3600.0) as i64));
        for (_, to, m) in &r.kept {
            table.take(*to, m);
        }
        r.table = table;
        r.helper_life_h = life;
        r.built = true;
    });
}

/// The table as one line, for the plan's log.
pub(super) fn line() -> Option<String> {
    ROUTE.get().filter(|r| r.built).map(|r| r.table.line())
}

/// Where the table places a newcomer of `class` at `rate` (weeks an
/// hour) for `life_h`, leaving `from` when it moves; the placement is
/// kept.  The table's account and its room before and after, in points,
/// or `-` without a table or an entry.
fn place(class: Class, rate: f64, life_h: Option<f64>, from: Option<u8>, now: i64) -> String {
    ROUTE.update(|r| {
        if !r.built {
            return "table=-".into();
        }
        let m = Newcomer { class, rate, high: rate * high_of(class), life_h: life_h.unwrap_or(r.helper_life_h) };
        match r.table.place(&m, from) {
            Some(p) => {
                if r.kept.len() >= KEPT_CAP {
                    r.kept.remove(0);
                }
                r.kept.push((now, p.to, m));
                format!("table=P{} room={:.2}>{:.2}", p.to, 100.0 * p.before, 100.0 * p.after)
            }
            None => "table=-".into(),
        }
    })
}

fn class_word(class: Class) -> &'static str {
    if class == Class::Restricted { "fable" } else { "plain" }
}

fn today(to: Option<u8>) -> String {
    to.map_or("-".into(), |t| format!("P{t}"))
}

/// The other agents with accounts here, each with what its best open,
/// freshly read account has left of its week, weeks.  Reads their feeds.
fn others_now(now: i64) -> Vec<(&'static str, f64)> {
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

/// A Claude helper today's rule started on `to`, with the other agents'
/// line.  Reads the other agents' feeds: off the shell's main loop.
pub(in crate::plugins) fn shadow_helper(class: Class, rate: f64, to: Option<u8>, now: i64) {
    let others = others_now(now);
    // which agent the table would send it to, were the agent left open
    let agent = ROUTE.get().filter(|r| r.built).map_or("-", |r| {
        let m = Newcomer { class, rate, high: rate * high_of(class), life_h: r.helper_life_h };
        r.table.clone().with_others(&others).agent_for(&m)
    });
    let placed = place(class, rate, None, None, now);
    let others: Vec<String> = others.iter().map(|(a, l)| format!("{a}={:.1}", 100.0 * l)).collect();
    let line = format!("kind=helper class={} today={} {placed} agent={agent} others={}", class_word(class), today(to), if others.is_empty() { "-".into() } else { others.join(",") });
    marspot::lx_info!("sched.route.shadow", &line);
}

/// A pane that appeared on `on`, or a pane a badge click moves off
/// `from` to `to`.
pub(super) fn shadow_pane(kind: &str, sid: u64, class: Class, rate: f64, from: Option<u8>, to: Option<u8>, now: i64) -> String {
    let rate = if rate > 0.0 { rate } else { RHO0 };
    let placed = place(class, rate, Some(H2), from, now);
    format!("kind={kind} sid={sid} class={} today={} {placed}", class_word(class), today(to))
}

#[cfg(test)]
#[path = "allocation_route_tests.rs"]
mod tests;
