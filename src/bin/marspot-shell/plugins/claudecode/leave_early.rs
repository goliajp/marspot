//! Leaving an account that cannot last two hours at the speed it is
//! used: every resting pane on it goes to an account that lasts six
//! hours with it, except the slowest, which stays to use what is left.
//!
//! Waiting for the refusal moved every pane at once: on 2026-11-24 at
//! 03:20:53 seven panes left P3 in the same second, most of them in a
//! turn. Sending every pane away early instead leaves the last of the
//! account to lapse at its renewal with nobody on it.
//!
//! This is a move a pane cannot do without, so it is not held by the
//! half hour between ordinary moves, by their one per pass, nor by the
//! switch that turns ordinary moves off or to a dry run.

use super::burn::SHORT_H;
use super::*;
pub(super) use super::leave_plan::{plan, Part, Plan, Taking, RECOVERED_H, TARGET_H};
use marspot::cc_usage::CcUsage;
use std::collections::HashSet;

/// What persists of this between passes.
#[derive(Default)]
pub(super) struct State {
    /// Accounts last logged as short, with the pane staying on each.
    said: HashMap<u8, Option<u64>>,
}

fn hours(h: Option<f64>) -> String {
    match h {
        Some(h) if h.is_finite() => format!("{h:.1}h"),
        Some(_) => "no end".into(),
        None => "one step from out, speed not yet known".into(),
    }
}

/// The shorter of two panes' outlooks on one account, `None` (one step
/// from out, speed unknown) the shortest, so the answer does not hang
/// on the order the panes are met in.
fn shorter(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        _ => None,
    }
}

impl ClaudecodePlugin {
    /// This pass's plan: every pane that is judged, not out, and on a
    /// numbered profile takes part.
    pub(super) fn plan_pass<'a>(
        &self,
        result: &'a ScanResult,
        rooms: &[(u8, quota::Room)],
        usage: &CcUsage,
        judged: impl Fn(u64, bool) -> Option<&'a str>,
        out_for: impl Fn(u64, &BindMeta, Option<&str>) -> bool,
    ) -> Plan<'a> {
        let taking: Vec<Taking> = result
            .new_meta
            .iter()
            .filter(|(sid, m)| {
                let choice = result.choices.get(*sid).cloned().unwrap_or_default();
                m.config_dir.is_some() && confirm_choice(result.new_models.get(*sid).map(String::as_str), &choice).is_ok()
            })
            .map(|(sid, m)| (*sid, m.profile_num, judged(*sid, self.fable_need.marked(*sid))))
            .filter(|(sid, _, model)| !out_for(*sid, &result.new_meta[sid], *model))
            .collect();
        let now = rooms::unix_now();
        plan(&result.speeds, rooms, &usage.accounts, &result.profiles, &taking, &result.pane_tokens, now)
    }

    /// One line when an account starts running short, and when the pane
    /// staying on it changes; one when a pane on it is seen to last
    /// `RECOVERED_H` again, so a speed read either side of the line
    /// does not flap.
    pub(super) fn say_short(&mut self, host: &dyn PluginHost, plan: &Plan) {
        let mut now: HashMap<u8, (Option<u64>, Option<f64>, usize)> = HashMap::new();
        let mut recovered: HashSet<u8> = HashSet::new();
        for (sid, part) in &plan.parts {
            if !part.short {
                if part.hours.is_none_or(|h| h >= RECOVERED_H) {
                    recovered.insert(part.profile);
                }
                continue;
            }
            let e = now.entry(part.profile).or_insert((None, part.hours, 0));
            e.1 = shorter(e.1, part.hours);
            e.2 += 1;
            if part.stays {
                e.0 = Some(*sid);
            }
        }
        let mut profiles: Vec<u8> = now.keys().copied().collect();
        profiles.sort();
        for profile in profiles {
            let (stays, h, n) = now[&profile];
            if self.leave.said.get(&profile) != Some(&stays) {
                let who = stays.map_or("none".into(), |s| format!("sid={s}"));
                let line = format!("P{profile} lasts {} at its speed, under {SHORT_H}h; {n} panes on it, {who} stays to use the rest", hours(h));
                host.log(LogLevel::Info, "quota.short", &line);
                self.leave.said.insert(profile, stays);
            }
        }
        let mut over: Vec<u8> = self.leave.said.keys().copied().filter(|p| !now.contains_key(p) && recovered.contains(p)).collect();
        over.sort();
        for p in over {
            self.leave.said.remove(&p);
            host.log(LogLevel::Info, "quota.short_over", &format!("P{p} lasts {RECOVERED_H}h or more again"));
        }
    }

    /// Move a resting pane off an account that is running short.
    /// True when a move was started.
    #[allow(clippy::too_many_arguments)] // one call site, each argument a separate fact
    pub(super) fn leave_short(
        &mut self,
        host: &dyn PluginHost,
        sid: u64,
        meta: &BindMeta,
        result: &ScanResult,
        model: Option<&str>,
        choice: &CliChoice,
        move_in_flight: bool,
        plan: &mut Plan,
        view: &mut super::allocation_view::View,
    ) -> bool {
        if move_in_flight {
            return false;
        }
        let part = plan.part(sid);
        let from = meta.profile_num;
        // the order is the allocation's rule (a), among the accounts lasting with it
        let class = super::allocation_view::class_of_judged(model);
        let chosen = std::cell::RefCell::new(None);
        let rank = |ids: &[u8]| {
            let (to, inputs) = view.rank_among(plan.rooms(), sid, from, class, ids)?;
            *chosen.borrow_mut() = Some((ids.to_vec(), inputs));
            Some(to)
        };
        let Some((to, lasts)) = plan.target(from, model, part.speed, rank) else {
            self.say_once(host, sid, "leave|none", "quota.leave_nowhere", format!("sid={sid} on P{from}: no account lasts {TARGET_H}h with it"));
            return false;
        };
        let pane = self.resting(host, sid, meta, result, move_in_flight, None);
        let held = rebalance::not_now(&pane).map(rebalance::Veto::phrase);
        let why = format!("P{from} lasts {}, P{to} {} with it", hours(part.hours), hours(Some(lasts)));
        if let Some(reason) = held {
            let line = format!("sid={sid} P{from} → P{to} ({why}) waits: {reason}");
            self.say_once(host, sid, &format!("leave|{to}|held"), "quota.leave_waits", line);
            return false;
        }
        host.log(LogLevel::Info, "quota.leaving", &format!("sid={sid} P{from} → P{to}: {why}"));
        self.rebalance_said.insert(sid, format!("leave|{to}|moving"));
        let warned = self.heads_up.get(&sid).is_some_and(|h| h.landed_on(from));
        let word = warned.then(|| heads_up::after_warned_move(false));
        if self.start_profile_cycle_to(host, sid, meta.clone(), to, word, choice.clone()) {
            self.rebalanced.insert(sid, (Instant::now(), to));
            plan.place(to, model, part.speed);
            view.placed(sid, to);
            // the inputs and the accounts it was ranked among: rule (a)
            // over those inputs, kept to these accounts, chose `to`
            let (among, inputs) = chosen.into_inner().map_or((Vec::new(), None), |(ids, i)| (ids, Some(i)));
            let words: Vec<String> = among.iter().map(|p| format!("P{p}")).collect();
            self.record_moved(host, sid, from, to, class, &format!("leave_early among={}", words.join(";")), inputs.as_ref(), Some(&among));
            return true;
        }
        false
    }

    fn say_once(&mut self, host: &dyn PluginHost, sid: u64, key: &str, event: &str, line: String) {
        if self.rebalance_said.get(&sid).map(String::as_str) != Some(key) {
            self.rebalance_said.insert(sid, key.to_string());
            host.log(LogLevel::Info, event, &line);
        }
    }

    /// What is known about a pane that a move away from rest checks.
    pub(super) fn resting(
        &self,
        host: &dyn PluginHost,
        sid: u64,
        meta: &BindMeta,
        result: &ScanResult,
        move_in_flight: bool,
        since_last: Option<Duration>,
    ) -> rebalance::Pane {
        let (work_in_flight, own_timer) = result.new_vetoes.get(&sid).copied().unwrap_or((true, true));
        let view = host.pane_status(sid).ok().flatten();
        rebalance::Pane {
            awaiting_user: view.as_ref().is_some_and(|v| v.quiescent && matches!(v.status, marspot::pane_state::PaneStatus::AwaitingUser)),
            user_here: self.focused_sid == Some(sid),
            work_in_flight,
            own_timer,
            idle_for: SystemTime::now().duration_since(meta.transcript_at).unwrap_or_default(),
            since_last,
            move_in_flight,
            host_load: (self.host_load)(),
            helper_bound: result.helpers.sits_out(sid),
        }
    }
}

/// What a pane on a short account is told while it works.
pub(super) fn heads_up_body(profile: u8, part: Part) -> String {
    let what = match part.hours {
        Some(h) if h.is_finite() && h < SHORT_H => format!("按现在的用量撑不到 {SHORT_H} 小时（约 {h:.1} 小时）"),
        _ => "额度只剩不到 1%".to_string(),
    };
    heads_up::body_for(profile, &what, true)
}
