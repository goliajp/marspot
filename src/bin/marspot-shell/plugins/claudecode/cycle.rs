//! Switching a pane to another profile, by hand or by hand-off.

use super::*;

impl ClaudecodePlugin {
    /// Kick off the profile cycle for `shelld_session_id`.  Called
    /// from `on_pane_badge_click`.  Builds a ProfileCyclePaneSession
    /// and hands it to the host; the host freezes the grid + locks
    /// Which model this pane needs room in, when the feed caps one
    /// separately.
    ///
    /// `None` for everything the feed does not meter on its own --
    /// which is every pane except the Fable ones, and for those
    /// nothing about the choice changes.
    pub(super) fn needs_model_for(&self, sid: u64) -> Option<String> {
        self.last_model.get(&sid).cloned()
    }

    /// the keyboard while the state machine runs.
    pub(super) fn start_profile_cycle(
        &mut self,
        host: &dyn PluginHost,
        shelld_sid: u64,
    ) {
        let Some(meta) = self.last_meta.get(&shelld_sid).cloned() else {
            host.log(
                LogLevel::Warn,
                "cycle.no_bind",
                &format!("badge click on unbound shelld_session={}", shelld_sid),
            );
            return;
        };
        let profiles = discover_profiles();
        // One account, or none numbered: there is nowhere to switch to,
        // and that is not a fault to flag on the pane.
        if profiles.len() < 2 || meta.config_dir.is_none() {
            host.log(LogLevel::Info, "cycle.single_account", &format!("pane {shelld_sid}: no other account to switch to"));
            return;
        }
        // The best account for the model this pane is on, from what the
        // accounts say about themselves. Without that there is no
        // choice to make: walking to the next number was a guess, and
        // with most accounts spent it was usually a refused one.
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        let usage = marspot::cc_usage::read();
        let needs = self.needs_model_for(shelld_sid);
        let picked = match (&usage, &home) {
            (Some(u), Some(home)) => {
                let mut rooms = quota::rooms_for(
                    &profiles,
                    &u.accounts,
                    |p| quota::profile_email(home, p),
                    rooms::unix_now(),
                );
                let resets = seen::resets_for(&profiles, &u.accounts, |p| quota::profile_email(home, p));
                self.refused.mark(&mut rooms, &resets, rooms::unix_now());
                host.log(
                    LogLevel::Info,
                    "cycle.rooms",
                    &format!("current=P{} model={needs:?} rooms={rooms:?}", meta.profile_num),
                );
                quota::best_profile(&rooms, meta.profile_num, needs.as_deref()).filter(|p| {
                    rooms
                        .iter()
                        .find(|(n, _)| n == p)
                        .is_some_and(|(_, r)| !r.refused_for(needs.as_deref()))
                })
            }
            _ => None,
        };
        let Some(next_profile) = picked else {
            let why = if usage.is_some() { "no account has room for this model" } else { "no usage feed" };
            host.log(
                LogLevel::Info,
                "cycle.no_target",
                &format!("pane {shelld_sid} stays on P{}: {why}", meta.profile_num),
            );
            let badge = t(if usage.is_some() { Msg::BadgeNoAccountFree } else { Msg::BadgeNoUsageData });
            let _ = host.submit_pty_op(shelld_sid, cycle_blocked_op(badge));
            return;
        };
        host.log(
            LogLevel::Info,
            "cycle.profiles",
            &format!("discovered={profiles:?} current=P{} next=P{next_profile}", meta.profile_num),
        );
        self.switch_by_hand(host, shelld_sid, meta, next_profile);
    }


    /// A switch someone asked for, remembered so the sweep leaves the
    /// pane where they put it.
    ///
    /// The pane still reads as being on the profile it is leaving
    /// until the new process is bound, and the sweep must not start a
    /// move of its own over the top of this one: the same "not landed"
    /// mark an automatic move leaves covers that gap.
    pub(super) fn switch_by_hand(&mut self, host: &dyn PluginHost, sid: u64, meta: BindMeta, target: u8) {
        // Asked for or not, a switch starts a new claude, and it starts
        // on the model the pane is on or not at all.
        let choice = pushed_choice(&meta.uuid);
        let model = self.needs_model_for(sid);
        if let Err(why) = confirm_choice(model.as_deref(), &choice) {
            host.log(
                LogLevel::Warn,
                "cycle.model_unconfirmed",
                &format!(
                    "pane {sid}: not switching to P{target}: {why} (scan={model:?} hook={:?})",
                    choice.model
                ),
            );
            let _ = host.submit_pty_op(sid, cycle_blocked_op(t(Msg::BadgeModelUnknown)));
            return;
        }
        // The person may switch away from a turn that was cut off by the
        // quota and walk off; the new claude is told the same as after
        // an automatic move, and an idle one stays idle -- unless it was
        // told its account was nearly out, and may have stopped for that.
        let warned = self.heads_up.get(&sid).is_some_and(|h| h.landed_on(meta.profile_num));
        let word = if warned {
            // unknown counts as running, as for an automatic move
            heads_up::after_warned_move(self.last_vetoes.get(&sid).is_none_or(|v| v.0))
        } else {
            resume_word()
        };
        if self.start_profile_cycle_to(host, sid, meta, target, Some(word), choice) {
            self.stranded.entry(sid).or_default().last_target = Some(target);
        }
    }

    /// Start the exit → resume cycle towards an explicit target
    /// profile.  Shared tail of the badge left-click (auto-next) and
    /// the badge context-menu "switch to Px" pick (explicit target).
    ///
    /// Returns whether a cycle was actually handed to the host.  Every
    /// caller that keeps a tally of moves needs that answer: a refusal
    /// to build one (no session name yet, a busy background holder)
    /// leaves the pane exactly where it was, and counting it as a move
    /// spends the budget that was supposed to pay for a real one.
    pub(super) fn start_profile_cycle_to(
        &mut self,
        host: &dyn PluginHost,
        shelld_sid: u64,
        meta: BindMeta,
        next_profile: u8,
        say: Option<String>,
        // the model and effort the new claude is started with
        choice: CliChoice,
    ) -> bool {
        if self.shelld.is_none() {
            host.log(
                LogLevel::Warn,
                "cycle.no_shelld",
                "no shelld client; cannot start cycle",
            );
            return false;
        }
        // The claude the decision was made on, at the moment of acting.
        if !still_claude(meta.claude) {
            let msg = format!("pane {shelld_sid}: pid {} is not that claude any more; not cycling", meta.claude.pid);
            host.log(LogLevel::Warn, "cycle.pid_no_longer_claude", &msg);
            return false;
        }
        // The conversation may have gone on in another session since the
        // pane bound this one -- sent to the background, or forked; the
        // old id would bring back the conversation as it was then.
        let mut meta = meta;
        if let Some(proj) = meta.project_dir.as_deref() {
            let config = meta.config_dir.clone().map_or_else(
                || PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude"),
                PathBuf::from,
            );
            let transcripts = config.join("projects").join(encode_project_dir(std::path::Path::new(proj)));
            let latest = continued::latest(&transcripts, &meta.uuid);
            if latest != meta.uuid {
                host.log(
                    LogLevel::Info,
                    "cycle.continued",
                    &format!("pane {shelld_sid}: uuid={} was continued in {latest}; resuming that", meta.uuid),
                );
                meta.uuid = latest;
                meta.has_transcript = true;
            }
        }
        // Before the signal, not after it.  This cycle kills first and
        // resumes second, so a resume the CLI was always going to
        // refuse costs the pane its session rather than just failing.
        let next_dir =
            PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(format!(".claude-profile-{next_profile}"));
        let holder = background_session_holder(&next_dir, &meta.uuid);
        // An idle background job is stopped as part of the cycle — the
        // CLI's own remedy ("claude stop <job> first to resume it here"),
        // and what the user asked for: the switch completes and the
        // conversation comes back in the pane (2026-09-17).  Idle means
        // nothing is in flight, so what stops is a process, not work.
        // A job that is busy is left alone: stopping it would cut off
        // whatever it is doing, and that is not a badge click's to take.
        if let Some(h) = holder.filter(|h| !h.idle) {
            host.log(
                LogLevel::Warn,
                "cycle.background_session",
                &format!(
                    "pane {shelld_sid}: uuid={} is held by a background session (pid {}) that is \
                     busy; not cycling — stopping it would cut off its work, and \
                     `claude --resume` refuses while it runs",
                    meta.uuid, h.id.pid
                ),
            );
            if let Err(e) = host.submit_pty_op(shelld_sid, cycle_blocked_op(t(Msg::BadgeHeldByBgJob))) {
                host.log(
                    LogLevel::Warn,
                    "cycle.blocked_badge_failed",
                    &format!("pane {shelld_sid}: {e}"),
                );
            }
            return false;
        }
        if let Some(h) = holder {
            host.log(
                LogLevel::Info,
                "cycle.stopping_background",
                &format!(
                    "pane {shelld_sid}: uuid={} is held by an idle background session (pid {}); \
                     stopping it so the conversation can resume here under P{next_profile}",
                    meta.uuid, h.id.pid
                ),
            );
        }
        let Some(op) = profile_cycle_op(
            say.as_deref(),
            &meta.uuid,
            meta.has_transcript,
            next_profile,
            meta.claude,
            holder.map(|h| h.id),
            shell_pid_for(shelld_sid),
            meta.project_dir.as_deref(),
            &choice,
        ) else {
            host.log(
                LogLevel::Warn,
                "cycle.no_command",
                &format!("cannot build a P{next_profile} resume line for {}", meta.uuid),
            );
            return false;
        };
        // The one fact this path never recorded: WHICH session it is
        // about to resume.  Everything else was logged — the pane, the
        // profiles, all five steps — so "did switching profile lose my
        // context?" could not be answered from the log at all, only
        // guessed at (2026-08-07).  A pane whose project holds six real
        // conversations, which torajs does, makes that the only
        // question worth asking.
        host.log(
            LogLevel::Info,
            "cycle.resuming",
            &format!(
                "pane {shelld_sid} → P{next_profile} {} uuid={} model={} effort={}",
                if meta.has_transcript { "resuming" } else { "starting fresh," },
                meta.uuid,
                choice.model.as_deref().unwrap_or("(profile default)"),
                choice.effort.as_deref().unwrap_or("(profile default)"),
            ),
        );
        if let Err(e) = host.submit_pty_op(shelld_sid, op) {
            host.log(
                LogLevel::Warn,
                "cycle.submit_failed",
                &format!("pane {shelld_sid}: {e}"),
            );
            return false;
        }
        true
    }
}

impl ClaudecodePlugin {
    /// Move this pane's conversation to profile `n` of `to` (RFC-009).
    pub(super) fn hand_off(&mut self, host: &dyn PluginHost, sid: u64, to: crate::plugins::handoff::Agent, n: u8) {
        use crate::plugins::handoff;
        let Some(meta) = self.last_meta.get(&sid).cloned() else {
            host.log(LogLevel::Warn, "handoff.no_bind", &format!("pane {sid}: no claude to hand off from"));
            return;
        };
        let Some((_, dir)) = handoff::profiles(to).into_iter().find(|(p, _)| *p == n) else {
            host.log(LogLevel::Warn, "handoff.profile_gone", &format!("{} profile {n} no longer exists", to.key()));
            return;
        };
        if !still_claude(meta.claude) {
            let msg = format!("pane {sid}: pid {} is not that claude any more", meta.claude.pid);
            host.log(LogLevel::Warn, "handoff.pid_no_longer_claude", &msg);
            return;
        }
        let leaving = handoff::Leaving {
            agent: handoff::Agent::Claude,
            proc: meta.claude,
            shell_pid: shell_pid_for(sid),
            home: config_dir_or_default(meta.config_dir.as_deref()),
            session_id: Some(meta.uuid.clone()),
            cwd: None,
            go_on: self.heads_up.get(&sid).is_some_and(|h| h.landed_on(meta.profile_num)),
        };
        host.log(LogLevel::Info, "handoff.start", &format!("pane {sid} claude → {} P{n}: {leaving:?}", to.key()));
        let Some(op) = handoff::switch_op(sid, leaving, to, n, dir) else {
            host.log(LogLevel::Warn, "handoff.no_command", &format!("pane {sid}: cannot build the {} line", to.key()));
            return;
        };
        if let Err(e) = host.submit_pty_op(sid, op) {
            host.log(LogLevel::Warn, "handoff.submit_failed", &format!("pane {sid}: {e}"));
        }
    }
}
