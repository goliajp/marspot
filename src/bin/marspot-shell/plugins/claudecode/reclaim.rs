//! Reclaiming the claude in a pane that has been idle long enough.

use super::*;

impl ClaudecodePlugin {
    /// Reclaim the claude in any pane the state machine says has been
    /// quiet long enough, and that shows no CPU of its own.
    ///
    /// Every gate here is a veto; nothing "votes for" hibernating.
    /// The state machine already refuses to call a pane quiet without
    /// three agreeing observations, so this adds only what the machine
    /// cannot see: whether the subtree is burning CPU behind a silent
    /// transcript, and how long ago we last looked.
    pub(super) fn run_idle_policy(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        let Some(threshold) = hibernate_after() else {
            return;
        };
        self.run_idle_policy_at(host, result, threshold);
    }

    /// [`run_idle_policy`] with the threshold handed in.
    ///
    /// The threshold is the user's setting, and the policy used to read
    /// it for itself — so every test of the policy ran against whatever
    /// the machine running it had configured.  On a machine with
    /// reclamation switched off, three tests failed and two more passed
    /// for the wrong reason ("nothing was reclaimed" holds trivially when
    /// nothing is allowed to be), while the same suite was green on the
    /// bench host, which has no settings file.  The decision takes the
    /// number; where the number comes from is the caller's business.
    pub(super) fn run_idle_policy_at(
        &mut self,
        host: &dyn PluginHost,
        result: &ScanResult,
        threshold: Duration,
    ) {
        // Not wired to a shelld yet: nothing to reclaim into.
        if self.shelld.is_none() {
            return;
        }
        for (sid, (cpu_now, sampled_at)) in &result.new_cpu {
            // No "have I already done this one" check: a pane we
            // reclaimed reports `Dormant`, which composes to a state
            // that is not `AwaitingUser`, so the gate below excludes it
            // on the same evidence everyone else uses.  The private
            // dormant set is for the wake path, not for decisions.
            let Ok(Some(view)) = host.pane_status(*sid) else {
                continue; // no machine for this pane yet
            };
            let Some(claude) = result.new_meta.get(sid).map(|m| m.claude) else { continue };
            let Some((cpu_prev, prev_at)) = self
                .cpu_samples
                .get(sid)
                .filter(|(id, ..)| *id == claude)
                .map(|&(_, c, at)| (c, at))
            else {
                // First sample for this process: record it and decide on
                // the next pass, when there is a delta to look at.
                self.cpu_samples.insert(*sid, (claude, *cpu_now, *sampled_at));
                continue;
            };
            let (work_in_flight, own_timer) =
                result.new_vetoes.get(sid).copied().unwrap_or((true, true));
            let evidence = IdleEvidence {
                work_in_flight,
                own_timer,
                user_here: self.focused_sid == Some(*sid),
                helper_bound: result.helpers.sits_out(*sid),
                quiescent: view.quiescent,
                awaiting_user: matches!(
                    view.status,
                    marspot::pane_state::PaneStatus::AwaitingUser
                ),
                held: view.held,
                idle_for: result
                    .new_meta
                    .get(sid)
                    .map(|m| {
                        SystemTime::now()
                            .duration_since(m.transcript_at)
                            .unwrap_or_default()
                    })
                    // No transcript for this pane means no evidence of
                    // idleness, not permission to act on none.
                    .unwrap_or_default(),
                cpu_delta_ns: cpu_now.saturating_sub(cpu_prev),
                since_sample: sampled_at.duration_since(prev_at).unwrap_or_default(),
            };
            if !should_hibernate(evidence, threshold) {
                // Say why, once per change of reason.  Without this the
                // log shows nothing at all until the moment a session
                // is reclaimed, and "nothing happened" reads the same
                // whether the policy is waiting or broken.
                if let Some((category, line)) = blocking_reason(evidence, threshold) {
                    // De-duplicate on the CATEGORY: the line carries a
                    // live number, so comparing lines would log every
                    // scan (it did — see `blocking_reason`).
                    if self.blocked_reason.get(sid).map(String::as_str) != Some(category) {
                        host.log(
                            LogLevel::Info,
                            "hibernate.waiting",
                            &format!("shelld_session={} — {}", sid, line),
                        );
                        self.blocked_reason.insert(*sid, category.to_string());
                    }
                } else {
                    self.blocked_reason.remove(sid);
                }
                // Replace the baseline only once it is older than the
                // window: rolling it forward every scan is what made
                // the delta span 2 s and the gate unreachable.
                if evidence.since_sample >= CPU_BASELINE_WINDOW {
                    self.cpu_samples.insert(*sid, (claude, *cpu_now, *sampled_at));
                }
                continue;
            }
            self.blocked_reason.remove(sid);
            // This scan's binding, not the previous tick's: the pid
            // is about to be signalled, so it should be the freshest
            // one we have.  (`last_meta` is only updated after this
            // runs, which would hand the policy a pid one scan old.)
            let Some(meta) = result.new_meta.get(sid).cloned() else {
                continue;
            };
            // A session we cannot name the profile of cannot be
            // brought back correctly: the resume line decides which
            // config dir — which account — claude comes back under.
            // Not reclaiming costs memory; reclaiming costs the user
            // their session in the wrong place.
            if meta.profile_num == PROFILE_UNKNOWN {
                if self.blocked_reason.get(sid).map(String::as_str)
                    != Some("unknown_profile")
                {
                    host.log(
                        LogLevel::Warn,
                        "hibernate.unknown_profile",
                        &format!(
                            "shelld_session={} uuid={} — profile unreadable; not reclaiming",
                            sid, meta.uuid
                        ),
                    );
                    self.blocked_reason.insert(*sid, "unknown_profile".to_string());
                }
                continue;
            }
            // Badged from its profile alone — claude is running but has
            // not written a session file yet, so there is no uuid to
            // resume.  Reclaiming would take the pane down with no way
            // back; leave it running.
            if meta.uuid.is_empty() {
                if self.blocked_reason.get(sid).map(String::as_str)
                    != Some("unknown_session")
                {
                    host.log(
                        LogLevel::Warn,
                        "hibernate.unknown_session",
                        &format!(
                            "shelld_session={sid} — no session file yet; not reclaiming"
                        ),
                    );
                    self.blocked_reason.insert(*sid, "unknown_session".to_string());
                }
                continue;
            }
            // Reclaiming restarts claude, and it comes back on what it
            // is told: without its model and effort it would wake on
            // the profile's default. Same rule as a switch.
            let choice = result.choices.get(sid).cloned().unwrap_or_default();
            if let Err(why) = confirm_choice(result.new_models.get(sid).map(String::as_str), &choice) {
                if self.blocked_reason.get(sid).map(String::as_str) != Some("model_unconfirmed") {
                    host.log(
                        LogLevel::Info,
                        "hibernate.model_unconfirmed",
                        &format!("shelld_session={sid} — {why}; not reclaiming"),
                    );
                    self.blocked_reason.insert(*sid, "model_unconfirmed".to_string());
                }
                continue;
            }
            // Re-verify immediately before signalling: the same process
            // the evidence above was gathered on, and still claude.
            // `new_meta` is up to one observation old and pids are
            // recycled; acting on a stale one SIGTERMs whatever has it.
            if !still_claude(meta.claude) {
                host.log(
                    LogLevel::Warn,
                    "hibernate.pid_no_longer_claude",
                    &format!(
                        "shelld_session={} pid={} is not claude any more; not signalling",
                        sid, meta.claude.pid
                    ),
                );
                self.cpu_samples.remove(sid);
                continue;
            }
            host.log(
                LogLevel::Info,
                "hibernate.start",
                &format!(
                    // `idle` is the clock that DECIDES (the session's
                    // transcript age).  It used to print `held` — the
                    // pane's own state clock — which reads 0s or 1s on
                    // a session that has been idle for hours, so the
                    // log said the policy had fired on nothing.
                    "shelld_session={} uuid={} idle={}s (pane state held {}s) \
                     cpu_delta={}ms over {}s — reclaiming pid {}",
                    sid,
                    meta.uuid,
                    evidence.idle_for.as_secs(),
                    evidence.held.as_secs(),
                    evidence.cpu_delta_ns / 1_000_000,
                    evidence.since_sample.as_secs(),
                    meta.claude.pid,
                ),
            );
            // Hold the pane's picture BEFORE anything else happens on
            // it.  Everything from here to the far side of the wake —
            // claude's exit, the shell prompt, the resume line,
            // claude's startup — has to happen behind the frame the
            // user last saw.
            //
            // The hold lives in L3, not in the core: a silent update
            // restarts the core, and a core-side freeze evaporates
            // with it.  That is what put a bare zsh prompt in a parked
            // pane after an update.
            //
            // The signal itself is sent from the `Holding` stage, once
            // this request has had time to land — see that variant.
            let Some(op) = reclaim_op(
                &meta.uuid,
                meta.config_dir.as_deref(),
                Some(meta.claude),
                shell_pid_for(*sid),
                meta.project_dir.as_deref(),
                &choice,
            ) else {
                host.log(
                    LogLevel::Warn,
                    "hibernate.no_resume_line",
                    &format!(
                        "refusing to reclaim {}: its profile cannot be quoted",
                        meta.uuid
                    ),
                );
                continue;
            };
            let op = op.on_end(self.wake_reporter(*sid));
            // Freeze the chrome at the same instant as the picture:
            // this scan still sees the claude we are about to take
            // down, so it is the last one that knows what the corner
            // of this pane says.
            self.held.insert(
                *sid,
                FrozenLook {
                    badge: result
                        .new_mapping
                        .get(sid)
                        .cloned()
                        .unwrap_or_else(|| format!("P{}", meta.profile_num)),
                    title: meta.project_basename.clone(),
                },
            );
            if let Err(e) = host.submit_pty_op(*sid, op) {
                host.log(
                    LogLevel::Warn,
                    "hibernate.submit_failed",
                    &format!("pane {sid}: {e}"),
                );
                continue;
            }
            // This run *is* the pane's wake path, so mark it armed.
            // Without this the re-arm sweep sees an un-armed dormant
            // record two seconds later and starts a second run on the
            // same pane — which replaces the first mid-park, and leaves
            // two paths racing to type the same line.  Observed live.
            self.armed.insert(*sid);
            self.dormant.push(DormantRecord {
                shelld_sid: *sid,
                uuid: meta.uuid,
                profile_num: meta.profile_num,
                config_dir: meta.config_dir,
                project_dir: meta.project_dir,
                choice,
                created_at: SystemTime::now(),
            });
            self.persist_dormant(host);
        }
        // Forget samples for panes that are gone.
        self.cpu_samples
            .retain(|sid, _| result.sessions_seen.contains(sid));
        self.blocked_reason
            .retain(|sid, _| result.sessions_seen.contains(sid));
    }
}
