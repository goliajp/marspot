//! The scheduler, shell side.  For now it only watches: every account
//! decision the shell makes is weighed again by the selector and the
//! answer logged as `svc.sched.decide`, next to what the shell did.
//! Nothing here changes a decision.
//!
//! A decision repeated every scan (a resting pane's) goes through a
//! `Shadow`, which logs a pane's answer once, when it changes; a
//! one-off decision (a move made, a helper placed) through `once`; a
//! move the user asked for is logged by `overridden`, not weighed.
//!
//! The selector always weighs by the one objective (`sel_rule=waste`);
//! where the shell decided by the bands (`rule=bands`), `agree=` says
//! whether two different rules happened to agree.

pub(crate) mod supply;

use marspot_agent::quota::replay::{Inputs, Kind, Rule, Why};
use marspot_agent::quota::{Load, Quant};
use marspot_agent::sched::price::pool;
use marspot_agent::sched::state::State;
use marspot_agent::sched::{Choice, POLICY, Price, alloc};

/// What the scheduler remembers for one agent's decisions.
#[derive(Default)]
pub(crate) struct Shadow {
    state: State,
}

/// One decision as the shell made it: about pane `sid` (none for a
/// helper not yet in a pane), with what the shell did, if anything:
/// the account and its reason word as the shell's own log writes it.
pub(crate) struct Made<'a> {
    pub agent: &'a str,
    pub sid: Option<u64>,
    pub inputs: &'a Inputs,
    pub rule: Rule,
    pub did: Option<(u8, &'a str)>,
}

impl Shadow {
    /// Weighs a decision about pane `sid` again; logs the answer when it
    /// differs from the last one logged for that pane.
    pub(crate) fn decide(&mut self, agent: &str, sid: u64, inputs: &Inputs, rule: Rule, existing: Option<(u8, Why)>, now: i64) {
        let week = self.state.week_eff(now);
        let c = alloc::decide(inputs, weighed_by(rule), &|_| true, week, &POLICY);
        let to = existing.map(|e| e.0);
        let key = slot(c.action) | slot(to) << 9 | (c.decided_by as u64) << 18;
        if self.state.said(sid, key, now) {
            let m = Made { agent, sid: Some(sid), inputs, rule, did: existing.map(|(t, w)| (t, w.word())) };
            marspot::lx_info!("svc.sched.decide", &line(&m, Some(&c), self.state.held(inputs.here)));
        }
    }

    /// `once`, with the prices this shadow holds.
    pub(crate) fn once(&self, m: &Made<'_>, allow: Option<&[u8]>) {
        weigh(m, allow, self.state.held(m.inputs.here));
    }

    /// Prices every account of a scan, held against the last prices
    /// used; a price crossing 0.5 is logged as `svc.sched.price_cross`.
    pub(crate) fn prices(&mut self, agent: &str, accounts: &[Quant], loads: &[Load]) {
        let pool = pool(accounts, loads);
        for (q, l) in accounts.iter().zip(loads) {
            let (p, c) = self.state.price(q.id, marspot_agent::sched::price(q, l, pool));
            for (crossed, which, to) in [(c.week, "week", p.week), (c.window, "window", p.window)] {
                if crossed {
                    marspot::lx_info!("svc.sched.price_cross", &format!("agent={agent} account=P{} which={which} price={to:.2}", q.id));
                }
            }
        }
    }
}

/// Weighs a one-off decision again and logs the answer; `allow` is the
/// accounts the shell chose among, when it did not consider all.  No
/// prices are held here, so the line says `price=-`.
pub(crate) fn once(m: &Made<'_>, allow: Option<&[u8]>) {
    weigh(m, allow, None);
}

/// Logs a move the user asked for (a badge click) without weighing it:
/// the objective does not judge an override.
pub(crate) fn overridden(m: &Made<'_>) {
    marspot::lx_info!("svc.sched.decide", &line(m, None, None));
}

fn weigh(m: &Made<'_>, allow: Option<&[u8]>, held: Option<Price>) {
    let c = alloc::decide(m.inputs, weighed_by(m.rule), &|q| allow.is_none_or(|a| a.contains(&q.id)), 0.0, &POLICY);
    marspot::lx_info!("svc.sched.decide", &line(m, Some(&c), held));
}

/// The objective the selector weighs by: the shell's own when it is
/// the objective, else the objective with no resting moves counted.
fn weighed_by(rule: Rule) -> Rule {
    match rule {
        Rule::Waste { .. } => rule,
        Rule::Bands => Rule::waste(0),
    }
}

fn slot(to: Option<u8>) -> u64 {
    to.map_or(256, u64::from)
}

fn act(to: Option<u8>) -> String {
    to.map_or_else(|| "stay".into(), |t| format!("P{t}"))
}

fn point(k: &Kind) -> &'static str {
    match k {
        Kind::Rest { .. } => "rest",
        Kind::Out { .. } => "out",
        Kind::New { .. } => "new",
    }
}

/// The line for `m`; `c` is the selector's answer, none for an override.
fn line(m: &Made<'_>, c: Option<&Choice<u8>>, held: Option<Price>) -> String {
    let rule = match m.rule {
        Rule::Bands => "bands",
        Rule::Waste { .. } => "waste",
    };
    // a pane's id is the id of the session it runs
    let sid = m.sid.map_or_else(|| "-".into(), |s| s.to_string());
    let to = m.did.map(|d| d.0);
    let why = m.did.map_or("stay", |d| d.1);
    let price = held.map_or_else(|| "-".into(), |p| format!("{:.2}/{:.2}", p.week, p.window));
    let head = format!("sid={sid} pane={sid} agent={} point={} rule={rule} sel_rule=waste why={why}", m.agent, point(&m.inputs.kind));
    let Some(c) = c else {
        return format!("{head} decided_by=- act=- existing={} agree=- money=- eff=- fair=- runner=- price={price}", act(to));
    };
    let runner = c.runner_up.map_or_else(|| "-".into(), |(a, t)| format!("{}:{:.4}/{:.3}", act(a), t.money, t.efficiency));
    format!(
        "{head} decided_by={} act={} existing={} agree={} money={:.4} eff={:.3} fair={:.3} runner={runner} price={price}",
        c.decided_by.word(),
        act(c.action),
        act(to),
        u8::from(c.action == to),
        c.terms.money,
        c.terms.efficiency,
        c.terms.fairness,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: &str = include_str!("../../../../../crates/marspot-agent/tests/fixtures/alloc_decisions_20261126.tsv");

    fn row(kind: &str) -> Inputs {
        let l = ROWS.lines().find(|l| l.split('\t').nth(2) == Some(kind)).expect("a row of that kind");
        Inputs::decode(l.split('\t').nth(5).unwrap_or_default()).expect("the row decodes")
    }

    #[test]
    fn sched_decide_line_names_session_pane_point_and_both_rules() {
        let i = row("would_move");
        let m = Made { agent: "first", sid: Some(442), inputs: &i, rule: Rule::Bands, did: Some((4, "leave_early")) };
        let c = alloc::decide(&i, weighed_by(m.rule), &|_| true, 0.0, &POLICY);
        let l = line(&m, Some(&c), Some(Price { week: 0.4, window: 1.0 }));
        let head = format!("sid=442 pane=442 agent=first point={} rule=bands sel_rule=waste why=leave_early ", point(&i.kind));
        assert!(l.starts_with(&head), "{l}");
        assert!(l.ends_with(" price=0.40/1.00") && !l.contains("week_eff"), "{l}");
        // a helper not yet in a pane
        let h = row("placed");
        let l = line(&Made { agent: "second", sid: None, inputs: &h, rule: Rule::waste(0), did: Some((2, "out")) }, None, None);
        assert!(l.starts_with("sid=- pane=- agent=second point=new rule=waste sel_rule=waste why=out "), "{l}");
        assert!(l.contains(" decided_by=- act=- existing=P2 agree=- ") && l.ends_with(" price=-"), "{l}");
    }
}
