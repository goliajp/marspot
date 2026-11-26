//! The scheduler, shell side.  For now it only watches: every account
//! decision the shell makes is weighed again by the selector and the
//! answer logged as `svc.sched.decide`, next to what the shell did.
//! Nothing here changes a decision.
//!
//! A decision repeated every scan (a resting pane's) goes through a
//! `Shadow`, which logs a pane's answer once, when it changes; a
//! one-off decision (a move made, a helper placed) through `once`.

use marspot_agent::quota::replay::{Inputs, Rule, Why};
use marspot_agent::quota::{Load, Quant};
use marspot_agent::sched::price::pool;
use marspot_agent::sched::state::State;
use marspot_agent::sched::{Choice, POLICY, Price, alloc, price};

/// What the scheduler remembers for one agent's decisions.
#[derive(Default)]
pub(crate) struct Shadow {
    state: State,
}

impl Shadow {
    /// Weighs a decision about pane `sid` again; logs the answer when it
    /// differs from the last one logged for that pane.
    pub(crate) fn decide(&mut self, agent: &str, sid: u64, inputs: &Inputs, rule: Rule, existing: Option<(u8, Why)>, now: i64) {
        let week = self.state.week_eff(now);
        let c = alloc::decide(inputs, rule, &|_| true, week, &POLICY);
        let to = existing.map(|e| e.0);
        let key = slot(c.action) | slot(to) << 9 | (c.decided_by as u64) << 18;
        if self.state.said(sid, key, now) {
            marspot::lx_info!("svc.sched.decide", &line(agent, sid, inputs, rule, existing, &c));
        }
    }

    /// Prices every account of a scan, held against the last prices
    /// used; a price crossing 0.5 is logged as `svc.sched.price_cross`.
    pub(crate) fn prices(&mut self, agent: &str, accounts: &[Quant], loads: &[Load]) {
        let pool = pool(accounts, loads);
        for (q, l) in accounts.iter().zip(loads) {
            let (p, c) = self.state.price(q.id, price(q, l, pool));
            for (crossed, which, to) in [(c.week, "week", p.week), (c.window, "window", p.window)] {
                if crossed {
                    marspot::lx_info!("svc.sched.price_cross", &format!("agent={agent} account=P{} which={which} price={to:.2}", q.id));
                }
            }
        }
    }
}

/// Weighs a one-off decision again and logs the answer; `allow` is the
/// accounts the shell chose among, when it did not consider all.
pub(crate) fn once(agent: &str, sid: u64, inputs: &Inputs, rule: Rule, existing: Option<(u8, Why)>, allow: Option<&[u8]>) {
    let c = alloc::decide(inputs, rule, &|q| allow.is_none_or(|a| a.contains(&q.id)), 0.0, &POLICY);
    marspot::lx_info!("svc.sched.decide", &line(agent, sid, inputs, rule, existing, &c));
}

fn slot(to: Option<u8>) -> u64 {
    to.map_or(256, u64::from)
}

fn act(to: Option<u8>) -> String {
    to.map_or_else(|| "stay".into(), |t| format!("P{t}"))
}

fn line(agent: &str, sid: u64, i: &Inputs, rule: Rule, existing: Option<(u8, Why)>, c: &Choice<u8>) -> String {
    let rule_word = match rule {
        Rule::Bands => "bands",
        Rule::Waste { .. } => "waste",
    };
    let to = existing.map(|e| e.0);
    let why = existing.map_or("stay", |e| e.1.word());
    let runner = c.runner_up.map_or_else(|| "-".into(), |(a, t)| format!("{}:{:.4}/{:.3}", act(a), t.money, t.efficiency));
    let here = i.accounts.iter().position(|q| q.id == i.here);
    let p = here.map_or(Price::default(), |h| price(&i.accounts[h], &i.loads[h], pool(&i.accounts, &i.loads)));
    format!(
        "pane={sid} sid=- agent={} rule={rule_word} why={why} decided_by={} act={} existing={} agree={} money={:.4} eff={:.3} fair={:.3} runner={runner} week_eff={:.3} price={:.2}/{:.2}",
        agent,
        c.decided_by.word(),
        act(c.action),
        act(to),
        u8::from(c.action == to),
        c.terms.money,
        c.terms.efficiency,
        c.terms.fairness,
        c.week_eff_after,
        p.week,
        p.window
    )
}
