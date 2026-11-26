//! Moving panes between accounts without being asked: off an account
//! that has stopped answering, and, when resting, onto the quota that
//! lapses first.

use super::*;
use super::fable_need::Judged;

const HEADS_UP_FILE: &str = "heads_up.tsv";
const REFUSED_FILE: &str = "refused.tsv";

#[path = "refused_choice.rs"]
pub(super) mod refused_choice;

impl ClaudecodePlugin {
    /// Every pane sitting on an account that cannot answer moves to
    /// one that can.
    ///
    /// Driven by what the accounts say about themselves, not by
    /// reading the screen or the transcript for the words a refusal
    /// happens to print.  Those words move, they arrive as an agent's
    /// failure as readily as a turn's, and the record carrying them is
    /// pushed out of any window worth reading — one session's sat
    /// 125 KiB deep in a 755 MB file while the CLI wrote menus after
    /// it.  The account's own status has none of those problems: it is
    /// one field, it is true for every pane on that account at once,
    /// and it is already on this machine.
    pub(super) fn sweep_unusable_profiles(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        if result.new_meta.is_empty() {
            return;
        }
        if std::env::var_os("HOME").is_none() {
            return;
        }
        let Some(usage) = result.usage.as_ref() else {
            // No feed is not "all clear" — it is "no answer", and a
            // pane is not moved on no answer.
            return;
        };
        // read by the worker, so this sweep touches no profile on disk
        let profiles: Vec<u8> = result.profiles.iter().map(|(n, _)| *n).collect();
        let email = |p: u8| result.profiles.iter().find(|(n, _)| *n == p).and_then(|(_, e)| e.clone());
        let mut rooms = quota::rooms_for(&profiles, &usage.accounts, email, rooms::unix_now());
        self.note_refusals(host, result);
        let resets = seen::resets_for(&profiles, &usage.accounts, email);
        self.refused.mark(&mut rooms, &resets, rooms::unix_now());
        self.persist_refused(host);
        // Whether a profile is out is a question about a pane, not
        // about a profile: a pane pinned to a model can be shut out of
        // an account whose week is fine, and a pane that has just been
        // refused knows something the feed has not collected yet.
        let is_out = |profile: u8, model: Option<&str>, refusal: Option<&quota::Refusal>| -> bool {
            rooms
                .iter()
                .find(|(p, _)| *p == profile)
                .is_some_and(|(_, r)| r.refused_for_now(model, refusal))
        };
        // However the pane got where it is -- by hand, by a move, or
        // started there -- one rule: what it is running against what its
        // profile has left for that model.
        // A pane refused Fable is judged as a Fable pane.
        let judged = |sid: u64, marked: bool| {
            let model = result.new_models.get(&sid).map(String::as_str);
            if marked && !quota::needs_fable(model) { Some(fable_need::FABLE) } else { model }
        };
        // Judged as Fable, its background agents' refusal of Fable
        // under this claude is its own: the account may read Unknown,
        // and then only a refusal says it is out.
        let out_for = |sid: u64, meta: &BindMeta, model: Option<&str>| -> bool {
            let agent = result
                .agent_refused
                .get(&sid)
                .filter(|r| quota::needs_fable(model) && quota::needs_fable(Some(&r.model)));
            is_out(meta.profile_num, model, meta.refused_at.as_ref())
                || agent.is_some_and(|r| is_out(meta.profile_num, model, Some(r)))
        };
        // Forget what was tried for a pane that is out of trouble —
        // but not for one that is merely between bindings.  A cycle
        // kills the CLI and the scan cannot bind the pane again until
        // the new process exists, so for a tick or two `new_meta` has
        // nothing to say about it.  Dropping the tally there loses the
        // fact that a move is already on its way, and the next pass
        // starts a second one on top of the first (seen on pane 442,
        // 2026-09-29: three cycles in 40 seconds).
        self.fable_need.retain(&result.sessions_seen, rooms::unix_now());
        let marks = &self.fable_need;
        self.stranded.retain(|sid, _| {
            if !result.sessions_seen.contains(sid) {
                return false;
            }
            match result.new_meta.get(sid) {
                Some(m) => out_for(*sid, m, judged(*sid, marks.marked(*sid))),
                None => true,
            }
        });
        let mut plan = self.plan_pass(result, &rooms, usage, judged, out_for);
        self.say_short(host, &plan);
        let mut view = self.view_for(result);
        self.moved_at.retain(|sid, _| result.sessions_seen.contains(sid));
        self.rebalanced.retain(|sid, _| result.sessions_seen.contains(sid));
        self.rebalance_said.retain(|sid, _| result.sessions_seen.contains(sid));
        self.heads_up.retain(|sid, _| result.sessions_seen.contains(sid));
        let (mode, mut rebalanced_this_pass) = (marspot::settings::get().cc_rebalance, false);
        self.unjudged.retain(|sid| result.sessions_seen.contains(sid));
        // each move loads its target for the next: ordinary panes first, helpers fit around them
        let mut sids: Vec<(bool, u64)> = result.new_meta.keys().map(|&sid| (result.helpers.is_helper(sid), sid)).collect();
        sids.sort();
        for (_, sid) in sids {
            let Some(meta) = result.new_meta.get(&sid).cloned() else { continue };
            // Started with no config dir, it is on the CLI's default
            // login, which is no numbered profile's: there is nothing
            // to compare it with and nowhere to resume it as itself.
            if meta.config_dir.is_none() {
                continue;
            }
            // What the pane is running comes first. A pane whose model
            // and effort cannot both be confirmed takes no part: it is
            // not judged and not moved, and nothing about it is assumed.
            let model = result.new_models.get(&sid).cloned();
            let choice = result.choices.get(&sid).cloned().unwrap_or_default();
            if let Err(why) = confirm_choice(model.as_deref(), &choice) {
                if self.unjudged.insert(sid) {
                    host.log(
                        LogLevel::Info,
                        "quota.unjudged",
                        &format!(
                            "sid={sid} on P{} takes no part in switching: {why} (scan={model:?} hook={:?}/{:?})",
                            meta.profile_num, choice.model, choice.effort
                        ),
                    );
                }
                continue;
            }
            if self.unjudged.remove(&sid) {
                host.log(
                    LogLevel::Info,
                    "quota.judged",
                    &format!("sid={sid} on P{} takes part in switching: {model:?} {:?}/{:?}", meta.profile_num, choice.model, choice.effort),
                );
            }
            let in_flight = self.rebalance_in_flight(sid, meta.profile_num);
            let marked = self.fable_need.marked(sid);
            let as_judged = judged(sid, marked);
            // out only for want of the Fable it was refused: the move
            // answers that refusal, and the session hears about it
            let for_fable = as_judged != model.as_deref() && !out_for(sid, &meta, model.as_deref());
            if !out_for(sid, &meta, as_judged) {
                let part = plan.part(sid);
                self.heads_up_if_due(host, sid, &rooms, &meta, result, as_judged, part);
                if part.short {
                    rebalanced_this_pass |= !part.stays && self.leave_short(host, sid, &meta, result, as_judged, &choice, in_flight, &mut plan, &mut view);
                    continue;
                }
                if mode != marspot::settings::Rebalance::Off {
                    let p = alloc_moves::Resting { sid, meta: &meta, model: as_judged, choice: &choice, in_flight, mode, already_moved_one: rebalanced_this_pass };
                    rebalanced_this_pass |= self.follow_core(host, &mut view, &mut plan, result, p);
                }
                continue;
            }
            if in_flight {
                // a move of its own is on its way; it lands, or the
                // next pass after `LANDING` judges it again
                continue;
            }
            let first_time = !self.stranded.contains_key(&sid);
            self.stranded.entry(sid).or_default();
            // a pane out only of Fable gets its one line from the move
            if first_time && !for_fable {
                host.log(
                    LogLevel::Info,
                    "quota.stranded",
                    &format!("sid={sid} is on P{}, which is out", meta.profile_num),
                );
            }
            let model = as_judged.map(str::to_string);
            self.move_off_refused(host, sid, &mut plan, &mut view, meta, result, choice, Judged { model, for_fable });
        }
        self.persist_heads_up(host);
        self.persist_fable_need(host);
    }

    /// Each pane's Fable refusal noted, and each account's refusal or
    /// lost login logged once.
    fn note_refusals(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        for (sid, meta) in &result.new_meta {
            self.note_fable_refusal(host, *sid, meta, result);
            let Some(r) = meta.refused_at.as_ref() else { continue };
            if self.refused.note(meta.profile_num, r) {
                let what = if r.logged_out {
                    "could not log in".to_string()
                } else if r.model.is_empty() {
                    "refused every model".to_string()
                } else {
                    format!("refused {}", r.model)
                };
                let event = if r.logged_out { "quota.logged_out" } else { "quota.refused" };
                host.log(LogLevel::Info, event, &format!("P{} {what} at {}", meta.profile_num, r.at));
            }
        }
    }

    pub(super) fn load_heads_up(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = fs::read_to_string(dir.join(HEADS_UP_FILE)) {
            self.heads_up = heads_up::Ledger::decode(&text);
        }
    }

    pub(super) fn load_refused(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        if let Ok(text) = fs::read_to_string(dir.join(REFUSED_FILE)) {
            self.refused = seen::Seen::decode(&text);
        }
    }

    fn persist_refused(&mut self, host: &dyn PluginHost) {
        let Some(text) = self.refused.changed() else { return };
        let Ok(dir) = host.state_dir() else { return };
        if let Err(e) = fs::write(dir.join(REFUSED_FILE), text) {
            host.log(LogLevel::Warn, "quota.refused_persist_failed", &format!("{e}"));
        }
    }

    fn persist_heads_up(&mut self, host: &dyn PluginHost) {
        let Some(text) = self.heads_up.changed() else { return };
        let Ok(dir) = host.state_dir() else { return };
        if let Err(e) = fs::write(dir.join(HEADS_UP_FILE), text) {
            host.log(LogLevel::Warn, "quota.heads_up_persist_failed", &format!("{e}"));
        }
    }

    /// Move one pane off an account that has stopped answering.
    fn move_off_refused(
        &mut self,
        host: &dyn PluginHost,
        sid: u64,
        plan: &mut leave_early::Plan,
        view: &mut allocation_view::View,
        meta: BindMeta,
        result: &ScanResult,
        // the model and effort the sweep confirmed for this pane
        choice: CliChoice,
        judged: Judged,
    ) {
        // One question, answered from structure: is a command of the
        // user's running here now — then leave the pane alone until it
        // ends, because moving kills the process and the command.
        //
        // There used to be a second question — was this pane working,
        // so should it be told to carry on.  It is gone: the sentence
        // goes to every pane that is moved.  The reading
        // was wrong half the time in the direction that costs the
        // most: of six panes moved in silence on 2026-09-30, three had
        // a turn killed mid-flight by the very refusal that triggered
        // the move.  They look idle because they are: the CLI printed
        // its error, closed the turn, and the transcript's last record
        // is a finished one.  Nothing in the structure separates
        // "finished" from "cut off" after the fact.
        //
        // To a pane that was idle it costs one line: it asks the
        // session to continue *if* something was interrupted.
        let tool_executing = matches!(result.new_activity.get(&sid), Some(CcActivity::ToolPending { executing: true }));
        let needs_model = judged.room_model(plan.rooms(), meta.profile_num, result.new_models.get(&sid));
        // The idle policy's rule too: a process of its own under claude
        // (unknown counts as one) is work a move would kill.
        let work_running = result.new_vetoes.get(&sid).is_none_or(|v| v.0);
        let warned = self.heads_up.get(&sid).is_some_and(|h| h.landed_on(meta.profile_num));
        let helper = result.helpers.is_helper(sid);
        let class = allocation_view::class_of_judged(needs_model.as_deref());
        let from = meta.profile_num;
        let r = refused_choice::Refused { sid, meta: &meta, needs_model: needs_model.as_deref(), class, helper, tool_executing, work_running, warned };
        let (decision, inputs) = self.refused_choice(plan, view, result, &r);
        match decision {
            quota::Move::Hold(why) => {
                let tally = self.stranded.entry(sid).or_default();
                if tally.hold_said != Some(why) {
                    tally.hold_said = Some(why);
                    let (event, line) = judged.hold_line(sid, meta.profile_num, why);
                    host.log(LogLevel::Info, event, &line);
                }
            }
            quota::Move::To(target) => {
                let fable_word = judged.for_fable.then(|| self.fable_need.word(sid, meta.profile_num, target)).flatten();
                let told_fable = fable_word.is_some();
                // told once per refusal: moved again with nothing new
                // refused, it goes for what the feed reads
                let because = match (judged.for_fable, told_fable) {
                    (true, true) => ": it was refused Fable",
                    (true, false) => ": the usage feed shows its Fable spent",
                    (false, _) => "",
                };
                host.log(LogLevel::Info, "quota.moving", &format!("sid={sid} P{from} → P{target}{because}"));
                // the move ends what runs under claude; the session is
                // told, or it waits on background shells that are gone.
                // A pane moved for the Fable it was refused hears which
                // request to start again, whatever else it was told
                let word = match (fable_word, warned, work_running) {
                    (Some(said), _, true) => heads_up::after_ended_work(&said),
                    (Some(said), _, false) => said,
                    (None, true, _) => heads_up::after_warned_move(work_running),
                    (None, false, true) => heads_up::after_ended_work(&resume_word()),
                    (None, false, false) => resume_word(),
                };
                if self.start_profile_cycle_to(host, sid, meta, target, Some(word), choice) {
                    if told_fable {
                        self.fable_need.told(sid);
                    }
                    self.moved_at.insert(sid, Instant::now());
                    plan.place(target, needs_model.as_deref(), plan.speed_of(sid));
                    // a helper is counted where it went by the helpers' tally
                    if !helper {
                        view.placed(sid, target);
                    }
                    self.record_moved(host, sid, from, target, class, if helper { "helper" } else { "out" }, inputs.as_ref());
                    let tally = self.stranded.entry(sid).or_default();
                    tally.moves += 1;
                    tally.last_target = Some(target);
                    if helper {
                        let at = rooms::unix_now();
                        with_helpers::account_changed(with_helpers::AccountChanged { sid, agent: "claude", from, to: target, at });
                    }
                }
            }
        }
    }

    /// A resting-pane move aimed at a profile the pane is not bound to
    /// yet, and recent enough to still be on its way.
    fn rebalance_in_flight(&self, sid: u64, bound_to: u8) -> bool {
        self.rebalanced
            .get(&sid)
            .is_some_and(|(at, to)| *to != bound_to && at.elapsed() < rebalance::LANDING)
    }
}
