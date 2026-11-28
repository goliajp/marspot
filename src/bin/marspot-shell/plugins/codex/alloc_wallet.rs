//! Each codex account's paid balance as its panes' rollouts read it,
//! and the facts that come of it, for the record only.
//!
//! Every reading carries the account's week and its credits balance.
//! The readings of one pass, from every pane, are taken in time order
//! per account, and each drop of the balance from the reading before is
//! one fact: `spend` when the week was already used up (codex takes
//! credits only then), with the pane, the amount and the tokens of the
//! reply the reading came with; `wallet_drop` otherwise, a drop nothing
//! explains (credits ending, or another product spending them).  A
//! balance that does not read is no reading: no fact, the last one held.
//!
//! The spend facts telescope: however many readings a pass skips, their
//! sum over a span is the balance's drop over it.  A restart takes the
//! last balance from the newest `spend` or `wallet_drop` fact, so the
//! tail read again yields no fact twice.  Nothing here decides anything.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use super::alloc::{CodexAlloc, Log, Snapshot, class_word};
use super::alloc_per_point::{Period, PeriodStore};
use super::alloc_rollout::CxTokens;
use crate::agentsvc::accounts::wallet::{self, Cap, View};
use crate::plugins::allocation::Class;
use crate::plugins::claudecode::allocation_events::{DAY, Event};

/// A balance this much lower is a drop; the balance has ten decimals.
const DROP_EPS: f64 = 1e-6;

/// Drops nothing explains kept per account, and for how long.
const DROPS_KEPT: usize = 64;
const DROPS_FOR_S: i64 = 7 * DAY;

/// Spends of the last day kept per account; a reply every few seconds
/// for a day is far fewer.
const SPENDS_KEPT: usize = 32_768;

/// A week used at least this much is used up.
const FULL: f64 = 1.0 - 1e-9;

/// One reading as a pane's rollout gave it.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Seen {
    pub at: i64,
    pub sid: u64,
    pub profile: u8,
    pub class: Class,
    pub week: Option<(f64, i64)>,
    pub balance: Option<f64>,
    pub cap: Option<Cap>,
    pub tokens: CxTokens,
}

#[derive(Debug, Default)]
struct Acct {
    /// The newest reading a fact was decided from.
    at: i64,
    balance: Option<f64>,
    week_full: bool,
    cap: Option<Cap>,
    spends: VecDeque<(i64, f64)>,
    drops: VecDeque<(i64, f64)>,
    spending_told: bool,
}

pub(super) struct Wallets {
    pending: Vec<Seen>,
    /// Per pane, the model its last turn started on.
    models: HashMap<u64, String>,
    accounts: HashMap<u8, Acct>,
    pub(super) periods: PeriodStore,
}

impl Wallets {
    /// `events` is the codex events directory; the period files sit
    /// beside it.
    pub(super) fn new(events: &Path) -> Wallets {
        Wallets { pending: Vec::new(), models: HashMap::new(), accounts: HashMap::new(), periods: PeriodStore::new(events.parent().unwrap_or(events)) }
    }

    pub(super) fn seen(&mut self, s: Seen) {
        self.pending.push(s);
    }

    pub(super) fn model(&mut self, sid: u64, model: String) {
        self.models.insert(sid, model);
    }

    /// A kept `spend` or `wallet_drop` fact, read back on a restart.
    pub(super) fn restore(&mut self, e: &Event, now: i64) {
        let word = |k: &str| e.detail.split(' ').find_map(|w| w.strip_prefix(k)?.strip_prefix('='))?.parse::<f64>().ok();
        let (Some(profile), Some(amount), Some(balance)) = (e.profile, word("amount"), word("balance")) else { return };
        let a = self.accounts.entry(profile).or_default();
        if e.at >= a.at {
            a.at = e.at;
            a.balance = Some(balance);
            a.week_full = e.kind == "spend";
        }
        if e.kind == "spend" {
            if e.at > now - DAY {
                push_capped(&mut a.spends, (e.at, amount), SPENDS_KEPT);
            }
        } else if e.at > now - DROPS_FOR_S {
            push_capped(&mut a.drops, (e.at, amount), DROPS_KEPT);
        }
    }

    fn views(&self) -> Vec<(u8, View)> {
        let mut out: Vec<(u8, View)> = self
            .accounts
            .iter()
            .filter_map(|(p, a)| {
                let view = View { at: a.at, balance: a.balance?, week_full: a.week_full, cap: a.cap.clone(), spends: a.spends.iter().copied().collect(), drops: a.drops.iter().copied().collect() };
                Some((*p, view))
            })
            .collect();
        out.sort_by_key(|(p, _)| *p);
        out
    }
}

fn push_capped(q: &mut VecDeque<(i64, f64)>, item: (i64, f64), cap: usize) {
    if q.len() >= cap {
        q.pop_front();
    }
    q.push_back(item);
}

impl CodexAlloc {
    /// This pass's readings, in time order, into facts and periods; then
    /// what the accounts' balances read is published.
    pub(super) fn settle_wallets(&mut self, snap: &Snapshot, log: &mut Log) {
        let mut pending = std::mem::take(&mut self.wallets.pending);
        pending.sort_by_key(|s| s.at);
        for s in pending {
            let spent = self.wallet_fact(&s, log);
            let model = self.wallets.models.get(&s.sid).cloned();
            if let Some(done) = self.wallets.periods.take(&s, model.as_deref(), spent) {
                self.period_done(s.profile, done, snap.now, log);
            }
        }
        let now = snap.now;
        for a in self.wallets.accounts.values_mut() {
            while a.spends.front().is_some_and(|(at, _)| *at <= now - DAY) {
                a.spends.pop_front();
            }
            while a.drops.front().is_some_and(|(at, _)| *at <= now - DROPS_FOR_S) {
                a.drops.pop_front();
            }
        }
        let live: std::collections::HashSet<u64> = snap.panes.iter().map(|p| p.sid).collect();
        self.wallets.models.retain(|sid, _| live.contains(sid));
        if let Err(e) = self.wallets.periods.save(now) {
            log.push(("codex.wallet.periods_failed", e.to_string()));
        }
        wallet::publish("codex", self.wallets.views());
    }

    /// The fact one reading makes, if any; the amount spent when it is a spend.
    fn wallet_fact(&mut self, s: &Seen, log: &mut Log) -> f64 {
        let a = self.wallets.accounts.entry(s.profile).or_default();
        if s.at <= a.at {
            return 0.0;
        }
        a.at = s.at;
        if s.cap.is_some() {
            a.cap = s.cap.clone();
        }
        let full = s.week.is_some_and(|(used, _)| used >= FULL);
        a.week_full = full;
        if !full {
            a.spending_told = false;
        }
        let Some(balance) = s.balance else { return 0.0 };
        let drop = a.balance.map_or(0.0, |was| was - balance);
        a.balance = Some(balance);
        if drop <= DROP_EPS {
            return 0.0;
        }
        let (kind, detail) = if full {
            push_capped(&mut a.spends, (s.at, drop), SPENDS_KEPT);
            let model = self.wallets.models.get(&s.sid).map_or("-", String::as_str);
            ("spend", format!("amount={drop:.6} balance={balance:.6} {} model={model}", s.tokens.words()))
        } else {
            push_capped(&mut a.drops, (s.at, drop), DROPS_KEPT);
            let week = s.week.map_or_else(|| "-".to_string(), |(u, _)| format!("{u:.2}"));
            log.push(("codex.wallet.drop", format!("P{} amount={drop:.6} balance={balance:.6} week={week} sid={}", s.profile, s.sid)));
            ("wallet_drop", format!("amount={drop:.6} balance={balance:.6} week={week}"))
        };
        if full && !std::mem::replace(&mut a.spending_told, true) {
            log.push(("codex.wallet.spending", format!("P{} spends credits: week used up, balance {balance:.2} sid={}", s.profile, s.sid)));
        }
        let ev = Event { at: s.at, sid: s.sid, kind: kind.to_string(), profile: Some(s.profile), class: class_word(s.class).to_string(), detail };
        if let Err(e) = self.events.append(&ev) {
            log.push(("codex.alloc.events_failed", format!("{e}")));
        }
        if full { drop } else { 0.0 }
    }

    fn period_done(&mut self, profile: u8, done: Period, now: i64, log: &mut Log) {
        match self.wallets.periods.record(profile, &done, now) {
            Ok(Some(line)) => log.push(("codex.wallet.per_point", line)),
            Ok(None) => {}
            Err(e) => log.push(("codex.wallet.per_point_failed", e.to_string())),
        }
    }
}
