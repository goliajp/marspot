//! Dormant panes: putting their wake path back, and keeping the set on disk.

use super::*;

impl ClaudecodePlugin {
    /// Put the wake path back after it was lost.
    ///
    /// An L1 self-execv replaces the process and every PaneSession
    /// with it, while the pane itself survives with claude already
    /// reclaimed.  Without this, the next keystroke in a dormant pane
    /// runs as a shell command and the session is only recoverable by
    /// typing the resume line by hand.
    ///
    /// Also drops records for panes where claude is back (woken, or
    /// started by the user), which is what keeps the set bounded.
    pub(super) fn rearm_dormant(&mut self, host: &dyn PluginHost, result: &ScanResult) {
        let Some(client) = self.shelld.as_ref().cloned() else {
            return;
        };
        self.settle_wake_ends(host);
        self.held.retain(|sid, _| result.sessions_seen.contains(sid));
        let before = self.dormant.len();
        // A pane that reports a binding again has a live claude: it is
        // no longer dormant, whoever woke it.
        self.dormant.retain(|d| {
            // A scan may not judge a record until the kill it was made
            // for has had time to land.  The scan that triggers the
            // reclamation predates it, which is obvious; the ones for
            // the next few seconds are the subtle case — claude takes
            // longer than one 2 s tick to flush and exit, so those
            // scans still find it alive and still hold its binding.
            // Reading that as "claude is back" drops the record, and
            // with it the wake path.  Observed on the real machine at
            // 21:15: pane 384 was reclaimed and `dormant.tsv` was
            // rewritten empty in the same minute, leaving the pane at
            // a bare shell instead of a resumable session.
            if result.scanned_at <= d.created_at + KILL_GRACE {
                return true;
            }
            !result.new_mapping.contains_key(&d.shelld_sid)
                && result.sessions_seen.contains(&d.shelld_sid)
        });
        // Ask the kernel, not the scan, whether the pane is really
        // empty.  A binding can lag a restart by a scan or two, and
        // arming a wake on a pane that already has a live claude means
        // the next focus types `claude --resume …` **into that running
        // claude's prompt** — where it sits waiting for the user to
        // press Enter.  That is precisely the shape of the "I have to
        // press Enter" report, and it costs one proc-table walk to
        // make impossible.
        //
        // Only when there is a record waiting to be armed.  The list
        // is empty almost always — a pane is reclaimed once, armed
        // once, and stays armed — while the walk is one
        // `proc_pidinfo` per pid on the host, measured at 0.46-1.33 ms
        // for 1200 processes here.  Small, but it is the thread that
        // draws the window and it was paying it every two seconds for
        // nothing.
        let obs = self
            .dormant
            .iter()
            .any(|d| !self.armed.contains(&d.shelld_sid))
            .then(|| crate::observe::shared().fresh(crate::observe::NOW));
        let rearm: Vec<DormantRecord> = self
            .dormant
            .iter()
            .filter(|d| !self.armed.contains(&d.shelld_sid))
            .filter(|d| {
                let Some(s) = obs.as_ref().and_then(|o| o.session(d.shelld_sid)) else {
                    return false;
                };
                s.shell.is_some() && s.find("claude").is_none()
            })
            .cloned()
            .collect();
        for d in rearm {
            // The same script, started at the park: there is nothing
            // left to kill, and the pid it would have signalled died
            // with the process that armed it.
            let choice = dormant_choice(host, &d);
            let Some(op) = reclaim_op(
                &d.uuid,
                d.config_dir.as_deref(),
                None,
                shell_pid_for(d.shelld_sid),
                d.project_dir.as_deref(),
                &choice,
            ) else {
                host.log(
                    LogLevel::Warn,
                    "hibernate.no_resume_line",
                    &format!("cannot re-arm {}: its profile cannot be quoted", d.uuid),
                );
                continue;
            };
            let op = op.on_end(self.wake_reporter(d.shelld_sid));

            // Idempotent by design: if this pane is already held (the
            // usual case — L3 outlived the L1 that asked), L3 sees no
            // change.  If a *new* L3 came up meanwhile, this is what
            // puts the hold back.
            // The look this pane is frozen at, rebuilt from the
            // record: an L1 self-execv dropped whatever the badge said
            // in full, and the profile is the half that survives.  It
            // is what the badge would read anyway until the model is
            // read back off the resumed session.
            self.held.entry(d.shelld_sid).or_insert_with(|| FrozenLook {
                badge: if d.profile_num == PROFILE_UNKNOWN {
                    "cc".to_string()
                } else {
                    format!("P{}", d.profile_num)
                },
                title: String::new(),
            });
            let _ = client.hold_grid_of(d.shelld_sid, true);
            let park = op.index_of(RECLAIM_PARK_LABEL).unwrap_or(0);
            match host.submit_pty_op_at(d.shelld_sid, op, park) {
                Ok(()) => {
                    self.armed.insert(d.shelld_sid);
                    host.log(
                        LogLevel::Info,
                        "hibernate.rearmed",
                        &format!("dormant session {} can be woken again", d.uuid),
                    );
                }
                Err(e) => host.log(
                    LogLevel::Warn,
                    "hibernate.rearm_failed",
                    &format!("pane {}: {e}", d.shelld_sid),
                ),
            }
        }
        self.armed.retain(|sid| {
            self.dormant.iter().any(|d| d.shelld_sid == *sid)
        });
        // A held pane whose claude is back must be drawing again.
        //
        // The session releases its own hold when the wake finishes,
        // but that release travels L1 → L2 → L3 and a core that is
        // restarting at that instant drops it — leaving a live pane
        // showing a picture from before the reclamation.  This is the
        // sweep that notices.  Costs a frame only when it is the one
        // fixing something: `held` is empty in the ordinary case.
        let back: Vec<u64> = self
            .held
            .keys()
            .copied()
            // A pane with a wake armed is one we are deliberately
            // holding — leave it alone.  The scan's mapping is up to
            // two seconds old, so the pass right after a reclamation
            // still shows the claude we just killed; acting on that
            // released the hold six milliseconds into the park, and
            // the pane spent its whole parked life showing a shell
            // prompt instead of the frame the user left.
            .filter(|sid| !self.armed.contains(sid))
            .filter(|sid| result.new_mapping.contains_key(sid))
            .collect();
        for sid in back {
            self.held.remove(&sid);
            if client.hold_grid_of(sid, false).is_ok() {
                host.log(
                    LogLevel::Info,
                    "hibernate.hold_released",
                    &format!("pane {sid} has a live claude again"),
                );
            }
        }
        if self.dormant.len() != before {
            self.persist_dormant(host);
        }
    }

    /// Write the dormant set to the plugin's state dir; losing it costs a wake path.
    pub(super) fn persist_dormant(&self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        let path = dir.join("dormant.tsv");
        if let Err(e) = fs::write(&path, encode_dormant(&self.dormant)) {
            host.log(
                LogLevel::Warn,
                "hibernate.persist_failed",
                &format!("{e}"),
            );
        }
    }

    /// Read back what an earlier process left dormant.  Called once at
    /// init; `rearm_dormant` does the rest on the next scan.
    pub(super) fn load_dormant(&mut self, host: &dyn PluginHost) {
        let Ok(dir) = host.state_dir() else { return };
        let Ok(text) = fs::read_to_string(dir.join("dormant.tsv")) else {
            return;
        };
        self.dormant = decode_dormant(&text);
        if !self.dormant.is_empty() {
            host.log(
                LogLevel::Info,
                "hibernate.loaded",
                &format!("{} dormant session(s) carried over", self.dormant.len()),
            );
        }
    }
}

/// The model a re-armed wake starts claude on: the one its row carries,
/// else (a row written before the column) the session's own record,
/// the account's last observed model, the default.
pub(super) fn dormant_choice(host: &dyn PluginHost, d: &DormantRecord) -> CliChoice {
    if d.choice.model.is_some() {
        return d.choice.clone();
    }
    use launch_model::Source;
    let account = config_dir_or_default(d.config_dir.as_deref());
    let (model, from) = launch_model::first_known(&[
        (Source::Session, &|| launch_model::session_model(&d.uuid, None)),
        (Source::Account, &|| launch_model::account_model(&account)),
    ]);
    host.log(
        LogLevel::Info,
        "hibernate.model_from",
        &format!("{}: its row names no model; waking on {model} ({})", d.uuid, from.as_str()),
    );
    CliChoice { model: Some(model), effort: d.choice.effort.clone() }
}
