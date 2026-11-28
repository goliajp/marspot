//! `sched.credits = last`: a codex pane on an account whose week is used
//! up moves to an account that still has plan allowance, at its next
//! clean idle boundary.
//!
//! Codex spends paid credits only once an account's week is used up, and
//! from then on on every turn; so a pane leaves as soon as its account's
//! week is used up and the account has credits (`exhausted`), or its
//! balance is seen falling (`paying`).  It moves through the badge menu's
//! own switch, so only when that switch would be allowed: the thread is
//! idle with no sub-agent at work and no mail queued (peerbus decides),
//! the terminal is quiet, the input box is empty, and the pane has not
//! switched within `COOL_DOWN`.  Never mid-turn.  The target is the open
//! account with a fresh reading and the most allowance left, as long as
//! that allowance covers the scheduling profile's minimum stay at the
//! pane's rate; an account that is itself paying or used up is none.  A
//! pane moves at most once per week its account is used up, and the fleet
//! at most `MOVES_PER_HOUR` an hour; a move that did not start gives its
//! place in the hour back, and the pane is not asked again for `RETRY_S`.
//! `hold` and `free` move nothing.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use marspot::settings::SchedCredits;

use super::credits::{Acct, SPENT};
use super::stop_paying_screen::Looks;
use crate::plugins::peerbus::moving::Held;
use crate::plugins::{LogLevel, PluginHost};

/// Moves ordered across all panes in any hour, at most.
pub(super) const MOVES_PER_HOUR: usize = 4;

/// How long after a pane's last switch, by hand or by this rule, it is
/// left alone.
pub(super) const COOL_DOWN: Duration = Duration::from_secs(10 * 60);

/// A target's week reading must be at most this old.
pub(super) const FRESH_S: i64 = 30 * 60;

/// How long a pane whose ordered move did not start is left alone.
pub(super) const RETRY_S: i64 = 5 * 60;

/// The least plan allowance a target must have left, whatever the stay.
pub(super) const MIN_LEFT: f64 = 0.05;

/// Orders still waiting for the next balance of the account they left.
const AFTERS_CAP: usize = 32;

const HOUR_S: i64 = 3600;
const WEEK_S: i64 = 7 * 86_400;

/// Holds that follow a move that began and failed; they stand in for the
/// once-per-week rule's own reason, so the failure is what stays said.
const FAILED: [&str; 2] = ["switch_failed", "still_loaded"];

/// A move the rule ordered.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Order {
    pub sid: u64,
    pub from: u8,
    pub to: u8,
    pub before: Option<f64>,
    pub plan_left_to: f64,
}

/// One order whose old account's next balance is still to be logged.
#[derive(Clone, Debug, PartialEq)]
struct After {
    sid: u64,
    from: u8,
    at: i64,
    before: Option<f64>,
}

#[derive(Debug, Default)]
pub(super) struct StopPaying {
    /// Per pane and the account it left, the renewal of the used-up week
    /// it left in; kept while the pane is and that week lasts.
    moved: HashMap<(u64, u8), i64>,
    /// When each recent order was made and for which pane, for the
    /// fleet's hourly cap.
    orders: VecDeque<(i64, u64)>,
    /// Per pane, why it could not move last time, said once per change.
    held: HashMap<u64, &'static str>,
    /// Per pane whose ordered move did not start, when and why.
    backoff: HashMap<u64, (i64, &'static str)>,
    afters: VecDeque<After>,
    pub(super) looks: Looks,
    /// The time of the latest decision, which a refusal arriving between
    /// passes is dated by.
    last_now: i64,
}

pub(super) type Log = Vec<(&'static str, String)>;

/// The open account with a fresh reading and the most plan allowance
/// left, at least `need`, other than `from`; ties go to the lower profile.
pub(super) fn target(accts: &[Acct], from: u8, now: i64, need: f64) -> Option<&Acct> {
    accts
        .iter()
        .filter(|a| a.profile != from && a.open && a.renews > now && now - a.at <= FRESH_S && a.week_used < SPENT && !a.paying(now))
        .filter(|a| 1.0 - a.week_used >= need)
        .min_by(|x, y| x.week_used.total_cmp(&y.week_used).then(x.profile.cmp(&y.profile)))
}

/// The allowance a pane using `rate` of a week an hour needs left on a
/// target to stay there `stay_h` hours, and never under `MIN_LEFT`.
pub(super) fn need(stay_h: f64, rate: f64) -> f64 {
    (stay_h * rate).max(MIN_LEFT)
}

fn money(b: Option<f64>) -> String {
    b.map_or_else(|| "unknown".to_string(), |b| format!("{b:.2}"))
}

/// A pane the rule looks at: on account `from`, needing `need` of a week
/// left on a target.
#[derive(Clone, Copy, Debug)]
pub(super) struct Mover {
    pub sid: u64,
    pub from: u8,
    pub need: f64,
}

impl StopPaying {
    /// What to do with pane `m`.  `ready` says why it cannot switch now,
    /// if it cannot; it is asked only of a pane that should leave and has
    /// somewhere to go, since it reads the pane's screen.
    pub(super) fn decide(
        &mut self,
        mode: SchedCredits,
        accts: &[Acct],
        now: i64,
        m: Mover,
        ready: impl FnOnce() -> Result<(), &'static str>,
        log: &mut Log,
    ) -> Option<Order> {
        let Mover { sid, from, need } = m;
        self.last_now = now;
        let on = accts.iter().find(|a| a.profile == from).filter(|_| mode == SchedCredits::Last);
        let Some((a, reason)) = on.and_then(|a| Some((a, a.leave_reason(now)?))) else {
            self.held.remove(&sid);
            return None;
        };
        if self.moved.get(&(sid, from)).is_some_and(|r| (r - a.renews).abs() < 86_400) {
            let why = self.held.get(&sid).copied().filter(|w| FAILED.contains(w)).unwrap_or("moved_once");
            return self.hold(sid, Some(from), why, log);
        }
        if let Some(&(_, why)) = self.backoff.get(&sid).filter(|(at, _)| now - at < RETRY_S) {
            return self.hold(sid, Some(from), why, log);
        }
        let Some(to) = target(accts, from, now, need) else {
            return self.hold(sid, Some(from), "no_target", log);
        };
        let (to, plan_left_to) = (to.profile, 1.0 - to.week_used);
        if let Err(why) = ready() {
            return self.hold(sid, Some(from), why, log);
        }
        self.orders.retain(|(t, _)| now - t < HOUR_S);
        if self.orders.len() >= MOVES_PER_HOUR {
            return self.hold(sid, Some(from), "rate", log);
        }
        // the last hold stays, so a pane refused the same way after every
        // order is said once
        self.backoff.remove(&sid);
        self.moved.insert((sid, from), a.renews);
        self.orders.push_back((now, sid));
        let before = a.balance.map(|(_, b)| b);
        if self.afters.len() >= AFTERS_CAP {
            self.afters.pop_front();
        }
        self.afters.push_back(After { sid, from, at: now, before });
        log.push((
            "quota.stop_paying",
            format!("sid={sid} from=P{from} to=P{to} balance_before={} plan_left_to={plan_left_to:.2} reason={reason}", money(before)),
        ));
        Some(Order { sid, from, to, before, plan_left_to })
    }

    fn hold(&mut self, sid: u64, from: Option<u8>, why: &'static str, log: &mut Log) -> Option<Order> {
        if self.held.insert(sid, why) != Some(why) {
            let from = from.map_or_else(String::new, |f| format!(" from=P{f}"));
            log.push(("quota.stop_paying.held", format!("sid={sid}{from} why={why}")));
        }
        None
    }

    fn left_account(&self, sid: u64) -> Option<u8> {
        self.moved.keys().find(|(s, _)| *s == sid).map(|(_, f)| *f)
    }

    /// The ordered move of pane `sid` did not start: it gives back its
    /// place in the hour and may be ordered again after `RETRY_S`.
    pub(super) fn undo(&mut self, sid: u64, log: &mut Log, why: &'static str) {
        let from = self.left_account(sid);
        self.moved.retain(|(s, _), _| *s != sid);
        self.afters.retain(|a| a.sid != sid);
        if let Some(i) = self.orders.iter().rposition(|(_, s)| *s == sid) {
            self.orders.remove(i);
        }
        self.backoff.insert(sid, (self.last_now, why));
        self.hold(sid, from, why, log);
    }

    /// The move of pane `sid` began and failed: the pane stays where it
    /// is, the old account's next balance is no longer its result, and
    /// the week's one move stays spent.
    pub(super) fn failed(&mut self, sid: u64, log: &mut Log, why: &'static str) {
        let from = self.left_account(sid);
        self.afters.retain(|a| a.sid != sid);
        self.hold(sid, from, why, log);
    }

    /// Log each old account's first balance read after an order left it.
    pub(super) fn follow_afters(&mut self, accts: &[Acct], now: i64, log: &mut Log) {
        self.afters.retain(|o| {
            let Some((at, b)) = accts.iter().find(|a| a.profile == o.from).and_then(|a| a.balance).filter(|(at, _)| *at > o.at) else {
                return now - o.at < WEEK_S;
            };
            let fell = o.before.map_or_else(|| "unknown".to_string(), |was| format!("{:.2}", was - b));
            log.push((
                "quota.stop_paying.after",
                format!(
                    "sid={} from=P{} balance_before={} balance_after={b:.2} read_at={at} fell_by={fell}",
                    o.sid,
                    o.from,
                    money(o.before)
                ),
            ));
            false
        });
    }

    /// Forget panes that are gone and weeks that have renewed.
    pub(super) fn forget(&mut self, live: impl Fn(u64) -> bool, now: i64) {
        self.moved.retain(|(sid, _), renews| live(*sid) && *renews > now);
        self.held.retain(|sid, _| live(*sid));
        self.backoff.retain(|sid, (at, _)| live(*sid) && now - *at < RETRY_S);
    }
}

fn held_word(h: Held) -> &'static str {
    match h {
        Held::Busy => "busy",
        Held::ChildRunning => "sub_agent",
        Held::Queued => "queued",
        Held::Moving => "switching",
        Held::NotBound => "not_bound",
        Held::Unknown => "state_unknown",
    }
}

impl StopPaying {
    /// Peerbus held an ordered move: it may be ordered again later.
    pub(super) fn refused(&mut self, host: &dyn PluginHost, sid: u64, h: Held) {
        let mut log = Vec::new();
        self.undo(sid, &mut log, held_word(h));
        say(host, log);
    }

    /// An ordered move that got its go did not happen, for `why`.
    pub(super) fn switch_failed(&mut self, host: &dyn PluginHost, sid: u64, why: &'static str) {
        let mut log = Vec::new();
        self.failed(sid, &mut log, why);
        say(host, log);
    }
}

pub(super) fn say(host: &dyn PluginHost, log: Log) {
    for (tag, line) in log {
        host.log(LogLevel::Info, tag, &line);
    }
}
