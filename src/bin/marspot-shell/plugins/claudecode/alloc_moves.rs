//! The allocation core's choices, acted on by the main thread: the view
//! each sweep chooses by, the resting moves the core decided, and a
//! `moved` line in `alloc_events` for every move made by its choice.
//!
//! A resting move the core decides is one of three: following the plan
//! while supply exceeds demand, leaving what only the restricted panes
//! can use, or saving a restricted window that would lapse.  These
//! follow `claudecode.rebalance`: `dryrun` logs each with every input
//! that chose it and moves nothing, `live` moves, `off` does neither.
//! Leaving an account that runs short is `leave_early`'s, live in every
//! mode, and not taken from here.

use super::allocation_adapter::class_word;
use super::allocation_exec::{self, Order};
use super::allocation_events::{Event, EventLog};
use super::allocation_inputs::snapshot;
use super::allocation_view::View;
use super::rebalance::Veto;
use super::rebalance_boundary::{Outcome, Target};
use super::*;
use crate::plugins::allocation::Class;
use crate::plugins::allocation::replay::{Inputs, Rule, Why};
use crate::agentsvc::sched;
use marspot::agent::AgentKind;
use marspot_agent::sched::level2::Kind;
use marspot::settings::Rebalance;

/// One pane's resting move, as the sweep hands it over.
pub(super) struct Resting<'a> {
    pub sid: u64,
    pub meta: &'a BindMeta,
    /// What the pane is judged as: a pane refused Fable as Fable.
    pub model: Option<&'a str>,
    pub choice: &'a CliChoice,
    pub in_flight: bool,
    pub mode: Rebalance,
    pub already_moved_one: bool,
}

impl ClaudecodePlugin {
    /// This sweep's view: the worker's, or, before the worker has sent
    /// one, the same view with every pane at the default speed.  Kept
    /// for a badge click between scans.
    pub(super) fn view_for(&mut self, result: &ScanResult) -> View {
        let view = match &result.alloc {
            Some(v) => v.clone(),
            None => {
                let marks: HashMap<u64, i64> = self.fable_need.since_by_pane().collect();
                View::of_defaults(&snapshot(result, &HashMap::new(), &marks, rooms::unix_now()))
            }
        };
        view.publish();
        self.alloc_view = Some(view.clone());
        view
    }

    /// A `moved` fact: from where to where, why, and what chose it.
    ///
    /// `why` may carry words after the reason, such as the accounts a
    /// pane leaving early was ranked among; those accounts are `among`.
    #[allow(clippy::too_many_arguments)] // one fact, each field a separate piece
    pub(super) fn record_moved(&mut self, host: &dyn PluginHost, sid: u64, from: u8, to: u8, class: Class, why: &str, inputs: Option<&Inputs>, among: Option<&[u8]>) {
        let log = self.alloc_moved.get_or_insert_with(|| EventLog::new(allocation_adapter::events_dir(&marspot::paths::state_root())));
        let in_force = self.alloc_view.as_ref().map_or(Rule::Bands, |v| v.rule);
        let rule = in_force.words();
        if let Some(i) = inputs {
            let reason = why.split(' ').next().unwrap_or_default();
            let made = sched::Made { agent: AgentKind::Claude.as_str(), sid: Some(sid), inputs: i, rule: in_force, did: Some((to, reason)) };
            if reason == "badge" {
                sched::overridden(&made);
            } else {
                sched::once(&made, among);
            }
        }
        let detail = match inputs {
            Some(i) => format!("from=P{from} why={why} {}{rule}", i.encode()),
            None => format!("from=P{from} why={why}"),
        };
        let ev = Event { at: rooms::unix_now(), sid, kind: "moved".into(), profile: Some(to), class: class_word(class).into(), detail };
        if let Err(e) = log.append(&ev) {
            host.log(LogLevel::Warn, "alloc.events_failed", &format!("{e}"));
        }
    }

    /// Act on the resting move the core decided for a pane whose account
    /// is not out, as `claudecode.rebalance` says.  True when a move was
    /// started.
    ///
    /// At most one per pass: several panes restarting at once all land
    /// on the same account, and the next pass sees where they went.
    pub(super) fn follow_core(&mut self, host: &dyn PluginHost, view: &mut View, plan: &mut leave_early::Plan, result: &ScanResult, p: Resting) -> bool {
        let sid = p.sid;
        let from = p.meta.profile_num;
        // once the planner's executor is live, it alone moves panes to save lapsing quota
        let decided = view.rest.get(&sid).filter(|(_, why, _)| matches!(why, Why::Plan | Why::UOut | Why::Rescue) || (*why == Why::Waste && !result.plan_live)).cloned();
        let transcript_rests = matches!(result.new_activity.get(&sid), Some(CcActivity::AwaitingUser));
        let status = host.pane_status(sid).ok().flatten();
        let rest = status.as_ref().filter(|v| matches!(v.status, marspot::pane_state::PaneStatus::AwaitingUser)).map(|v| v.held);
        match &decided {
            Some((to, why, _)) => self.boundaries.stand(sid, Target { from, to: *to, word: why.word() }),
            None if transcript_rests => self.boundaries.drop_target(sid),
            None => {}
        }
        let Some((target, why, inputs)) = decided else {
            self.rebalance_said.remove(&sid);
            let veto = if transcript_rests { Veto::CoreStays } else { Veto::TranscriptBusy };
            self.boundary(host, sid, from, rest, Outcome::Held(veto));
            return false;
        };
        // an account that refused a pane since the feed was written is no target
        if plan.rooms().iter().find(|(n, _)| *n == target).is_none_or(|(_, r)| r.refused_for(p.model)) {
            self.boundary(host, sid, from, rest, Outcome::Held(Veto::RefusedThere));
            return false;
        }
        let pane = self.resting(status.as_ref(), sid, p.meta, result, p.in_flight, self.rebalanced.get(&sid).map(|(at, _)| at.elapsed()));
        let dry = p.mode == Rebalance::DryRun;
        // the account speeds the feed's readings show can see a target
        // running short that the panes' own speeds do not.  A u_out move
        // is a planned wait: the core sends the pane where it waits for
        // a renewal knowing that account will not last, so this check
        // would hold every one of them
        let short_there = why != Why::UOut && plan.lasts(target, p.model, plan.speed_of(sid)) < leave_early::TARGET_H;
        let held = rebalance::not_now(&pane)
            // waiting for the user while the screen still moves
            .map(|v| if v == Veto::Working && rest.is_some() { Veto::ScreenBusy } else { v })
            .or(short_there.then_some(Veto::ShortThere))
            .or((p.already_moved_one && !dry).then_some(Veto::AnotherMoved));
        let word = why.word();
        let line = match held {
            Some(v) => format!("sid={sid} P{from} → P{target} ({word}) waits: {}", v.phrase()),
            None if dry => format!("sid={sid} P{from} → P{target} ({word}) {}{}", inputs.encode(), view.rule.words()),
            None => format!("sid={sid} P{from} → P{target}: {word}"),
        };
        match (held, dry) {
            (Some(v), _) => self.boundary(host, sid, from, rest, Outcome::Held(v)),
            (None, true) => self.boundary(host, sid, from, rest, Outcome::WouldMove),
            (None, false) => {}
        }
        // one line when a pane starts waiting for a target, one each time
        // the reason it waits changes, and one when it moves
        let key = format!("{target}|{}", held.map_or("go", Veto::key));
        let new_answer = self.rebalance_said.get(&sid) != Some(&key);
        if new_answer {
            self.rebalance_said.insert(sid, key);
        }
        let event = match (held, dry) {
            (Some(_), _) => "rebalance.waiting",
            (None, true) => "rebalance.would_move",
            (None, false) => "rebalance.moving",
        };
        if new_answer || (held.is_none() && !dry) {
            host.log(LogLevel::Info, event, &line);
        }
        if held.is_some() || dry {
            return false;
        }
        // A resting pane had nothing interrupted, unless it was told
        // its account was nearly out: it may have stopped for that, and
        // is told to go on, not to stay idle, whatever the move is for.
        // Nothing runs under it, or it would not be moved.
        let warned = self.heads_up.get(&sid).is_some_and(|h| h.landed_on(from));
        let said = warned.then(|| heads_up::after_warned_move(false));
        if !self.start_profile_cycle_to(host, sid, p.meta.clone(), target, said, p.choice.clone()) {
            return false;
        }
        self.boundary(host, sid, from, rest, Outcome::Moved);
        self.rebalanced.insert(sid, (Instant::now(), target));
        plan.place(target, p.model, plan.speed_of(sid));
        view.placed(sid, target);
        self.record_moved(host, sid, from, target, inputs.mover.class, word, Some(&inputs), None);
        true
    }

    /// Make a switch the planner's executor ordered, unless the pane has
    /// changed since the scan: it works again, the user came to it, a
    /// move is on its way, a clean switch now finds work running, or the
    /// target refused it.  True when the switch was started.
    #[allow(clippy::too_many_arguments)] // one order and the pane's state around it
    pub(super) fn follow_plan(&mut self, host: &dyn PluginHost, view: &mut View, plan: &mut leave_early::Plan, result: &ScanResult, meta: &BindMeta, model: Option<&str>, choice: &CliChoice, in_flight: bool, o: Order) -> bool {
        let (sid, from) = (o.sid, meta.profile_num);
        if from != o.from {
            return false;
        }
        let status = host.pane_status(sid).ok().flatten();
        let pane = self.resting(status.as_ref(), sid, meta, result, in_flight, None);
        let clean = matches!(o.kind, Kind::Clean | Kind::Short);
        let refused = plan.rooms().iter().find(|(n, _)| *n == o.to).is_none_or(|(_, r)| r.refused_for(model));
        let held = [
            (pane.move_in_flight, Veto::MoveOnItsWay),
            (!pane.awaiting_user, Veto::Working),
            (pane.user_here, Veto::UserHere),
            (clean && pane.work_in_flight, Veto::WorkInFlight),
            (pane.own_timer, Veto::OwnTimer),
            (pane.host_load.is_some_and(|l| l > marspot::host_load::BUSY), Veto::Overloaded),
            (refused, Veto::RefusedThere),
        ]
        .into_iter()
        .find_map(|(on, v)| on.then_some(v));
        let line = format!("sid={sid} P{from} → P{} kind={}", o.to, o.kind.word());
        if let Some(v) = held {
            host.log(LogLevel::Info, "sched.switch.held", &format!("{line}: {}", v.phrase()));
            return false;
        }
        if !self.start_profile_cycle_to(host, sid, meta.clone(), o.to, Some(allocation_exec::say::after_switch(from, o.to)), choice.clone()) {
            return false;
        }
        host.log(LogLevel::Info, "sched.switch.done", &line);
        self.rebalanced.insert(sid, (Instant::now(), o.to));
        plan.place(o.to, model, plan.speed_of(sid));
        view.placed(sid, o.to);
        self.record_moved(host, sid, from, o.to, allocation_view::class_of_judged(model), "plan", None, None);
        true
    }

    /// One look at a pane for `rebalance.boundary`.
    fn boundary(&mut self, host: &dyn PluginHost, sid: u64, from: u8, rest: Option<Duration>, o: Outcome) {
        let target = self.boundaries.standing(sid, from);
        for line in self.boundaries.look(sid, Instant::now(), rest, target, o) {
            host.log(LogLevel::Info, "rebalance.boundary", &line);
        }
    }

    /// Forget the boundaries of panes that are gone.
    pub(super) fn retain_boundaries(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        for line in self.boundaries.retain(|sid| result.sessions_seen.contains(&sid)) {
            host.log(LogLevel::Info, "rebalance.boundary", &line);
        }
    }
}
