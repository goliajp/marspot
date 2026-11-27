//! The autorun policy's shell side: switching it per pane from the CLI,
//! sweeping the panes it is on for, and typing its lines into a pane.

use super::*;

impl ShellApp {
    /// Switch the policy for a pane, or report where it is on.
    ///
    /// Stored by working directory: that is what "the pane for this
    /// project" means, and it is the one identity that survives a
    /// restart, a move between windows, and a twin appearing (which
    /// changes the pane's name).
    pub(super) fn run_cli_autorun(&mut self, target: &str, on: Option<bool>) -> Result<String, String> {
        if target.trim().is_empty() {
            if self.autorun_panes.is_empty() {
                return Ok("autorun is off everywhere".into());
            }
            let mut on: Vec<&String> = self.autorun_panes.iter().collect();
            on.sort();
            return Ok(on
                .iter()
                .map(|cwd| format!("{cwd}\n"))
                .collect::<String>()
                .trim_end()
                .to_string());
        }
        let sid = self.resolve_target(target)?;
        let cwd = Self::live_panes(&observe::shared().fresh(observe::NOW))
            .into_iter()
            .find(|(s, _, _)| *s == sid)
            .map(|(_, cwd, _)| cwd)
            .ok_or_else(|| format!("pane {sid} is gone"))?;
        match on {
            Some(true) => {
                self.autorun_panes.insert(cwd.clone());
                let _ = cli_socket::save_autorun(&self.autorun_panes);
                Ok(format!("autorun on for {cwd} (pane {sid})"))
            }
            Some(false) => {
                self.autorun_panes.remove(&cwd);
                self.pane_tables.autorun_mem.remove(&sid);
                let _ = cli_socket::save_autorun(&self.autorun_panes);
                Ok(format!("autorun off for {cwd}"))
            }
            None => Ok(if self.autorun_panes.contains(&cwd) {
                format!("autorun is on for {cwd} (pane {sid})")
            } else {
                format!("autorun is off for {cwd}")
            }),
        }
    }

    /// How often the autorun policy looks at its panes.
    ///
    /// Deliberately slow.  Every trigger it can fire on requires the
    /// pane to have been quiet for longer than this already, so looking
    /// more often could only make it act sooner on evidence it should
    /// be sure of.
    const AUTORUN_INTERVAL: Duration = Duration::from_secs(30);

    /// Look at every pane the policy is on for, and act if it says to.
    pub(super) fn sweep_autorun(&mut self) {
        if self.autorun_panes.is_empty() {
            return;
        }
        let now = Instant::now();
        if self
            .autorun_last_look
            .is_some_and(|t| now.duration_since(t) < Self::AUTORUN_INTERVAL)
        {
            return;
        }
        self.autorun_last_look = Some(now);

        // The state machine's own view — the same one the reclamation
        // policy acts on, so "quiet" means one thing in this process.
        let statuses = self.pane_status.snapshot(now);
        let wall = std::time::SystemTime::now();
        let panes: Vec<(u64, String)> = Self::live_panes(&observe::shared().latest())
            .into_iter()
            .filter(|(_, cwd, _)| self.autorun_panes.contains(cwd))
            .map(|(sid, cwd, _)| (sid, cwd))
            .collect();
        self.pane_tables.autorun_mem.retain(|sid, _| panes.iter().any(|(s, _)| s == sid));

        for (sid, cwd) in panes {
            let Some((status, _held, _quiescent)) = statuses.get(&sid) else { continue };
            // The screen, as a person would read it.  Cheap enough at
            // this cadence (one file tail + a replay) and the only way
            // to see what the session actually said.
            let screen = Self::read_pane_screen(sid).unwrap_or_default();
            let look = plugins::autorun::Look {
                // Exactly one state is a pane worth typing into: a
                // bound session that finished its turn.  Empty, parked,
                // contradictory and unknown are all quiet too, and
                // typing `/clear` at any of them is a shell command
                // that does not exist.
                awaiting_user: matches!(
                    status,
                    marspot::pane_state::PaneStatus::AwaitingUser
                ),
                // `Busy` covers both halves of "it is doing something":
                // a job the shell is holding, and a program that is
                // still drawing.  A rotation is not over while its
                // build is running, however quiet the terminal looks.
                work_in_flight: matches!(status, marspot::pane_state::PaneStatus::Busy(_)),
                screen: &screen,
            };
            let mem = self.pane_tables.autorun_mem.entry(sid).or_default();
            let was_exhausted = mem.is_exhausted();
            let (action, why) = plugins::autorun::decide(&look, mem, wall);
            if mem.is_exhausted() && !was_exhausted {
                lx_warn!(
                    "shell.autorun.exhausted",
                    "no response after every retry; leaving this pane alone",
                    shelld_session_id = sid,
                    cwd = cwd.as_str()
                );
            }
            if let Err(waiting) = why {
                // Say why, once per change of reason.  Without this the
                // log is silent until something fires, and "nothing
                // happened" reads the same whether the policy is
                // waiting or wedged.  That question came back on day
                // one — *why has torajs stopped?* — and there was
                // nothing to answer it with.
                let reason = format!("{waiting:?}");
                if self.pane_tables.autorun_why.get(&sid) != Some(&reason) {
                    lx_info!(
                        "shell.autorun.waiting",
                        &reason,
                        shelld_session_id = sid,
                        cwd = cwd.as_str()
                    );
                    self.pane_tables.autorun_why.insert(sid, reason);
                }
                continue;
            }
            self.pane_tables.autorun_why.remove(&sid);
            let lines: Vec<&str> = match action {
                plugins::autorun::Action::ClearAndContinue => {
                    vec!["/clear", plugins::autorun::CONTINUE_AUTORUN]
                }
                plugins::autorun::Action::Continue => vec![plugins::autorun::CONTINUE],
                // The line is already in the box; all it needs is the
                // Enter that went missing.
                plugins::autorun::Action::SubmitPending => vec![""],
                plugins::autorun::Action::Nothing => continue,
            };
            lx_info!(
                "shell.autorun.act",
                &format!("{why:?} → {lines:?}"),
                shelld_session_id = sid,
                cwd = cwd.as_str(),
                attempt = mem.attempts()
            );
            if let Err(e) = self.autorun_type(sid, &lines) {
                lx_warn!("shell.autorun.send_failed", &e, shelld_session_id = sid);
            }
        }
    }

    /// Type each line into the pane, in order, as one operation.
    ///
    /// One operation, not one per line: the queue serialises whole
    /// operations, so splitting them would let something else in
    /// between `/clear` and what follows it.  Each line waits for the
    /// pane to draw before the next goes in — `/clear` restarts the
    /// session's UI, and a line typed into that gap lands nowhere.
    fn autorun_type(&mut self, sid: u64, lines: &[&str]) -> Result<(), String> {
        if self.pty_ops.is_busy(sid) {
            return Err(format!(
                "pane {sid} is busy ({})",
                self.pty_ops.running(sid).unwrap_or("?")
            ));
        }
        let mut op = plugins::pty_op::PtyOp::new("autorun")
            .typed_for_program()
            // A delivery, not a takeover: the pane is not frozen and
            // the keyboard is not locked, so a person who walks up
            // mid-sequence keeps control of their own session.
            .lock_keys(false);
        for (i, line) in lines.iter().enumerate() {
            if line.is_empty() {
                // Just the Enter: the text is already in the box.
                op = op.step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
                continue;
            }
            if i > 0 {
                // Let the previous line land and the UI settle before
                // the next one.  Typing `继续 autorun` into a session
                // that is still processing `/clear` loses it.
                op = op
                    .step(plugins::pty_op::Step::await_quiet(Duration::from_millis(700))
                        .timeout(Duration::from_secs(20)));
            }
            op = op
                .step(plugins::pty_op::Step::paste(*line))
                .step(plugins::pty_op::Step::send(b"\r".to_vec()).named("enter"));
        }
        self.pty_ops
            .submit(sid, op)
            .map(|_| ())
            .ok_or_else(|| format!("pane {sid} has too much queued"))
    }
}
