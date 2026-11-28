//! `sched.credits = last`: a codex pane whose account is spending paid
//! credits moves to an account that still has plan allowance, at its
//! next clean idle boundary.
//!
//! An account pays when its week is used up and its balance falls.  A
//! pane on it moves through the badge menu's own switch, so only when
//! that switch would be allowed: the thread is idle with no sub-agent at
//! work and no mail queued (peerbus decides), the terminal is quiet, the
//! input box is empty, and the pane has not switched within
//! `COOL_DOWN`.  Never mid-turn.  The target is the open account with a
//! fresh reading and the most allowance left; an account that is itself
//! paying or used up is none.  A pane moves at most once per week its
//! account is used up, and the fleet at most `MOVES_PER_HOUR` an hour.
//! `hold` and `free` move nothing.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use marspot::settings::SchedCredits;

use super::CodexPlugin;
use super::credits::{Acct, SPENT};
use super::pane::PaneCodex;
use super::stop_paying_screen::Looks;
use super::switch::{Switches, movable_argv};
use crate::plugins::peerbus::moving::Held;
use crate::plugins::{LogLevel, PluginHost};

/// Moves ordered across all panes in any hour, at most.
pub(super) const MOVES_PER_HOUR: usize = 4;

/// How long after a pane's last switch, by hand or by this rule, it is
/// left alone.
pub(super) const COOL_DOWN: Duration = Duration::from_secs(10 * 60);

/// A target's week reading must be at most this old.
pub(super) const FRESH_S: i64 = 30 * 60;

/// Orders still waiting for the next balance of the account they left.
const AFTERS_CAP: usize = 32;

const HOUR_S: i64 = 3600;
const WEEK_S: i64 = 7 * 86_400;

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
    /// When each recent order was made, for the fleet's hourly cap.
    orders: VecDeque<i64>,
    /// Per pane, why it could not move last time, said once per change.
    held: HashMap<u64, &'static str>,
    afters: VecDeque<After>,
    pub(super) looks: Looks,
}

pub(super) type Log = Vec<(&'static str, String)>;

/// The open account with a fresh reading and the most plan allowance
/// left, other than `from`; ties go to the lower profile.
pub(super) fn target(accts: &[Acct], from: u8, now: i64) -> Option<&Acct> {
    accts
        .iter()
        .filter(|a| a.profile != from && a.open && a.renews > now && now - a.at <= FRESH_S && a.week_used < SPENT && !a.paying(now))
        .min_by(|x, y| x.week_used.total_cmp(&y.week_used).then(x.profile.cmp(&y.profile)))
}

fn money(b: Option<f64>) -> String {
    b.map_or_else(|| "unknown".to_string(), |b| format!("{b:.2}"))
}

impl StopPaying {
    /// What to do with pane `sid`, on account `from`.  `ready` says why
    /// it cannot switch now, if it cannot; it is asked only of a paying
    /// pane with somewhere to go, since it reads the pane's screen.
    pub(super) fn decide(
        &mut self,
        mode: SchedCredits,
        accts: &[Acct],
        now: i64,
        sid: u64,
        from: u8,
        ready: impl FnOnce() -> Result<(), &'static str>,
        log: &mut Log,
    ) -> Option<Order> {
        let Some(a) = accts.iter().find(|a| a.profile == from).filter(|a| mode == SchedCredits::Last && a.paying(now)) else {
            self.held.remove(&sid);
            return None;
        };
        if self.moved.get(&(sid, from)).is_some_and(|r| (r - a.renews).abs() < 86_400) {
            return self.hold(sid, from, "moved_once", log);
        }
        let Some(to) = target(accts, from, now) else {
            return self.hold(sid, from, "no_target", log);
        };
        let (to, plan_left_to) = (to.profile, 1.0 - to.week_used);
        if let Err(why) = ready() {
            return self.hold(sid, from, why, log);
        }
        self.orders.retain(|t| now - t < HOUR_S);
        if self.orders.len() >= MOVES_PER_HOUR {
            return self.hold(sid, from, "rate", log);
        }
        self.held.remove(&sid);
        self.moved.insert((sid, from), a.renews);
        self.orders.push_back(now);
        let before = a.balance.map(|(_, b)| b);
        if self.afters.len() >= AFTERS_CAP {
            self.afters.pop_front();
        }
        self.afters.push_back(After { sid, from, at: now, before });
        log.push((
            "quota.stop_paying",
            format!("sid={sid} from=P{from} to=P{to} balance_before={} plan_left_to={plan_left_to:.2}", money(before)),
        ));
        Some(Order {
            sid,
            from,
            to,
            before,
            plan_left_to,
        })
    }

    fn hold(&mut self, sid: u64, from: u8, why: &'static str, log: &mut Log) -> Option<Order> {
        if self.held.insert(sid, why) != Some(why) {
            log.push(("quota.stop_paying.held", format!("sid={sid} from=P{from} why={why}")));
        }
        None
    }

    /// The ordered move of pane `sid` did not happen: it may be ordered again.
    pub(super) fn undo(&mut self, sid: u64, log: &mut Log, why: &'static str) {
        self.moved.retain(|(s, _), _| *s != sid);
        self.afters.retain(|a| a.sid != sid);
        self.held.insert(sid, why);
        log.push(("quota.stop_paying.held", format!("sid={sid} why={why}")));
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
        for (tag, line) in log {
            host.log(LogLevel::Info, tag, &line);
        }
    }
}

impl CodexPlugin {
    /// The credits rule over every codex pane, with what the allocation
    /// pass read of each account.
    pub(super) fn stop_paying(&mut self, host: &dyn PluginHost, credit: Option<(i64, Vec<Acct>)>) {
        let Some((now, accts)) = credit else { return };
        let mode = marspot::settings::get().sched.credits;
        let mut log = Vec::new();
        self.paying.follow_afters(&accts, now, &mut log);
        self.paying.forget(|sid| self.panes.contains_key(&sid), now);
        let mut sids: Vec<(u64, u8)> = self.panes.iter().filter_map(|(sid, p)| Some((*sid, p.profile?))).collect();
        sids.sort_unstable();
        let mut orders = Vec::new();
        let looks = self.paying.looks;
        for (sid, from) in sids {
            let (switches, panes) = (&self.switches, &self.panes);
            let ready = || auto_ready(host, switches, &looks, panes, sid);
            orders.extend(self.paying.decide(mode, &accts, now, sid, from, ready, &mut log));
        }
        for o in orders {
            let dir = super::profile_dirs().into_iter().find(|(n, _)| *n == o.to).map(|(_, d)| d);
            let codex = self.panes.get(&o.sid).map(|p| p.codex);
            let begun = match (dir, codex) {
                (Some(dir), Some(codex)) => self.begin_switch(o.sid, codex, o.to, dir, true),
                _ => false,
            };
            if !begun {
                self.paying.undo(o.sid, &mut log, "cannot_begin");
            }
        }
        for (tag, line) in log {
            host.log(LogLevel::Info, tag, &line);
        }
    }
}

/// Why pane `sid` may not be switched now by the rule, if it may not:
/// the menu switch's checks, an empty input box, and the cool-down.
fn auto_ready(host: &dyn PluginHost, switches: &Switches, looks: &Looks, panes: &HashMap<u64, PaneCodex>, sid: u64) -> Result<(), &'static str> {
    let pane = panes.get(&sid).ok_or("gone")?;
    if switches.moving(sid) {
        return Err("switching");
    }
    if switches.began_within(sid, COOL_DOWN) {
        return Err("cool_down");
    }
    if !pane.argv.as_deref().is_some_and(movable_argv) {
        return Err("custom_command");
    }
    let view = crate::plugins::peerbus::threads().get(&sid).filter(|v| v.codex == pane.codex).map(|v| v.idle);
    match view {
        None => return Err("thread_unknown"),
        Some(false) => return Err("busy"),
        Some(true) => {}
    }
    if !looks.settled(host, sid) {
        return Err("terminal_busy");
    }
    if !looks.input_empty(sid) {
        return Err("input_not_empty");
    }
    Ok(())
}
