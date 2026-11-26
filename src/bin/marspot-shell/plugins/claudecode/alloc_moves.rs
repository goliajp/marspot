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
use super::allocation_events::{Event, EventLog};
use super::allocation_inputs::snapshot;
use super::allocation_view::View;
use super::*;
use crate::plugins::allocation::Class;
use crate::plugins::allocation::replay::{Inputs, Rule, Why};
use marspot::agent::AgentKind;
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
    /// pane leaving early was ranked among.
    #[allow(clippy::too_many_arguments)] // one fact, each field a separate piece
    pub(super) fn record_moved(&mut self, host: &dyn PluginHost, sid: u64, from: u8, to: u8, class: Class, why: &str, inputs: Option<&Inputs>) {
        let log = self.alloc_moved.get_or_insert_with(|| EventLog::new(allocation_adapter::events_dir(&marspot::paths::state_root())));
        let in_force = self.alloc_view.as_ref().map_or(Rule::Bands, |v| v.rule);
        let rule = in_force.words();
        if let Some(i) = inputs {
            let reason = Why::from_word(why.split(' ').next().unwrap_or_default()).unwrap_or(Why::Out);
            crate::agentsvc::sched::once(AgentKind::Claude.as_str(), sid, i, in_force, Some((to, reason)), None);
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
        let Some((target, why, inputs)) = view.rest.get(&sid).filter(|(_, why, _)| matches!(why, Why::Plan | Why::UOut | Why::Rescue | Why::Waste)).cloned() else {
            self.rebalance_said.remove(&sid);
            return false;
        };
        let from = p.meta.profile_num;
        // an account that refused a pane since the feed was written is no target
        if plan.rooms().iter().find(|(n, _)| *n == target).is_none_or(|(_, r)| r.refused_for(p.model)) {
            return false;
        }
        let pane = self.resting(host, sid, p.meta, result, p.in_flight, self.rebalanced.get(&sid).map(|(at, _)| at.elapsed()));
        let dry = p.mode == Rebalance::DryRun;
        // the account speeds the feed's readings show can see a target
        // running short that the panes' own speeds do not.  A u_out move
        // is a planned wait: the core sends the pane where it waits for
        // a renewal knowing that account will not last, so this check
        // would hold every one of them
        let short_there = why != Why::UOut && plan.lasts(target, p.model, plan.speed_of(sid)) < leave_early::TARGET_H;
        let held = rebalance::not_now(&pane)
            .or(short_there.then_some("it would run short there at the speed its readings show"))
            .or((p.already_moved_one && !dry).then_some("another pane moved this pass"));
        let word = why.word();
        let line = match held {
            Some(reason) => format!("sid={sid} P{from} → P{target} ({word}) waits: {reason}"),
            None if dry => format!("sid={sid} P{from} → P{target} ({word}) {}{}", inputs.encode(), view.rule.words()),
            None => format!("sid={sid} P{from} → P{target}: {word}"),
        };
        // one line when a pane starts waiting for a target and one when it
        // moves; the reason it waits flips every pass or two, which is
        // not news
        let key = format!("{target}|{}", held.is_some());
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
        self.rebalanced.insert(sid, (Instant::now(), target));
        plan.place(target, p.model, plan.speed_of(sid));
        view.placed(sid, target);
        self.record_moved(host, sid, from, target, inputs.mover.class, word, Some(&inputs));
        true
    }
}
