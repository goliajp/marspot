//! The supervisor's periodic check, run from `user_event`: core
//! liveness, update probation, plugin ticks and pane status.

use super::*;

impl ShellApp {
    /// Periodic check.  Fired from `user_event`, which runs on every
    /// wake: the 1 s redraw pump, a core frame, a plugin's knock.
    /// Responsibilities:
    ///
    ///   1. If the *active* (visible) core died → restart it (and
    ///      abandon any in-flight update).
    ///   2. Drive any in-flight silent update via `poll_pending_update`
    ///      (the probationary core's crash/timeout/ready handling).
    ///
    ///   3-5. Active core healthcheck: HELLO timeout, PING, PONG
    ///      deadline — unchanged, but scoped to `active` only.
    ///   6. SIGUSR1 manual trigger; 7. banner refresh.
    pub(super) fn poll_supervisor(&mut self, ctx: &MarspotAppCtx) {
        // first: a control socket connection is asleep on each of these
        self.drain_pane_control();
        inherited::reap_pending();
        // Plugin tick — at the supervisor cadence (1 s pump plus knocks, see
        // `user_event`), each plugin further gated by its own tick_interval_ms.
        // last_plugin_tick is reserved for a supervisor-level throttle; the registry gates.
        // Order matters twice over: drain the plugins' activity
        // reports first so this round's observation is built from
        // them, then step the machines, and only then tick the plugins
        // — which read the fresh state back through the host.
        // Phase timing, off unless MARSPOT_SUPERVISOR_PROFILE=1.  See
        // `supervisor_profile`: this runs at least once a second
        // forever, so what it costs per wake is what the app costs at
        // idle.
        let prof = supervisor_profile::on();
        let mut mark = std::time::Instant::now();
        macro_rules! lap {
            ($phase:expr) => {
                if prof {
                    let now = std::time::Instant::now();
                    supervisor_profile::add($phase, now - mark);
                    // The last lap has nothing after it; the write is
                    // what keeps every other lap measuring its own
                    // phase rather than everything since the top.
                    let _ = std::mem::replace(&mut mark, now);
                }
            };
        }

        while let Ok((sid, activity)) = self.pane_activity_rx.try_recv() {
            self.pane_status.report_activity(sid, activity);
        }
        lap!(0);
        super::theme_follow::follow_settings();
        for text in plugins::claudecode::model_guard::take_notices() {
            self.send(MsgType::Notice, marspot::shell_proto::encode_notice(&text));
        }
        self.follow_theme();
        lap!(1);
        self.sweep_pane_status();
        lap!(2);
        self.sweep_autorun();
        lap!(3);
        self.dev_drive_badge_menu();
        self.dev_drive_close_sequence();
        self.dev_drive_quit();
        lap!(4);
        self.drive_wake_queue();
        lap!(5);
        self.last_plugin_tick = Instant::now();
        self.plugin_registry.tick_all_with(&self.plugin_host);
        lap!(6);

        instance::answer_front();
        // Answer the command socket.  Before the submit drain, so a
        // request that arrives this pass runs this pass.
        while let Ok(req) = self.cli_rx.try_recv() {
            match req {
                cli_socket::CliRequest::SendText { target, text, reply } => {
                    let (ok, msg) = self.run_cli_send(&target, &text);
                    lx_info!(
                        "shell.cli.send",
                        &msg,
                        target = target.as_str(),
                        bytes = text.len() as u32,
                        ok = ok as u32
                    );
                    let _ = reply.send((ok, msg));
                }
                cli_socket::CliRequest::ListPanes { reply } => {
                    let _ = reply.send(Self::addressed_panes());
                }
                cli_socket::CliRequest::Autorun { target, on, reply } => {
                    let out = self.run_cli_autorun(&target, on);
                    lx_info!(
                        "shell.autorun.switch",
                        match &out {
                            Ok(t) => t.clone(),
                            Err(e) => e.clone(),
                        }
                        .as_str(),
                        target = target.as_str()
                    );
                    let _ = reply.send(out);
                }
                cli_socket::CliRequest::ReadPane { target, extra_lines, reply } => {
                    let out = self.run_cli_read(&target, extra_lines);
                    lx_info!(
                        "shell.cli.read",
                        match &out {
                            Ok(t) => format!("{} bytes", t.len()),
                            Err(e) => e.clone(),
                        }
                        .as_str(),
                        target = target.as_str(),
                        ok = out.is_ok() as u32
                    );
                    let _ = reply.send(out);
                }
            }
        }

        // Take in whatever was submitted, then start what can start and
        // collect what finished.  Before `process_pane_sessions` so a
        // run that starts this round gets its first tick immediately.
        while let Ok(req) = self.pty_op_rx.try_recv() {
            if self
                .pty_ops
                .submit_at(req.shelld_session_id, req.op, req.start_at)
                .is_none()
            {
                lx_warn!(
                    "shell.pty_op.queue_full",
                    "dropping a submitted operation",
                    shelld_session_id = req.shelld_session_id
                );
            }
        }
        for r in self.pty_ops.pump(&self.op_host) {
            if !r.outcome.is_done() {
                lx_warn!(
                    "shell.pty_op.not_done",
                    &format!("{:?}", r.outcome),
                    op = r.name,
                    shelld_session_id = r.sid
                );
            }
        }

        // Drain PaneSession begin requests + tick every active
        // session.  Order matters: drain first so a session begun
        // mid-tick still gets its first on_tick this round.
        self.process_pane_sessions();

        // Drain any badge updates plugins queued during the tick and
        // forward to L2 as PaneBadge frames.  No L2 (pre-boot or
        // during a crash gap) → just drop the update; the next tick
        // will push the current mapping again (plugins re-issue every
        // transition, not just once).
        // Re-issued every tick by design, so a core swap costs one
        // tick instead of leaving the pane's TUI-shaped link scanning
        // off until something else happens to change.  Logged only on
        // CHANGE — the heartbeat is not news.
        while let Ok(upd) = self.pane_agent_tui_rx.try_recv() {
            let sid = upd.shelld_session_id;
            if self.pane_tables.agent_tui.get(&sid) != Some(&upd.on) {
                lx_info!(
                    "shell.pane_agent_tui.declared",
                    &format!("sid={sid} on={}", upd.on)
                );
                self.pane_tables.agent_tui.insert(sid, upd.on);
            }
            self.send_pane_agent_tui(sid, upd.on);
        }
        while let Ok(upd) = self.pane_render_markup_rx.try_recv() {
            let sid = upd.shelld_session_id;
            lx_info!(
                "shell.pane_render_markup.declared",
                &format!("sid={sid} on={}", upd.on)
            );
            if upd.on {
                self.pane_tables.render_markup.insert(sid, true);
            } else {
                self.pane_tables.render_markup.remove(&sid);
            }
            self.send_pane_render_markup(sid, upd.on);
        }
        while let Ok(upd) = self.pane_wheel_keys_rx.try_recv() {
            let sid = upd.shelld_session_id;
            let cleared = upd.up.is_empty() && upd.down.is_empty();
            // Logged only on CHANGE — the heartbeat is not news, the
            // same rule the agent-TUI declaration above follows.  The
            // codex plugin re-asserts its clear every tick (L2's state
            // outlives L1's, so a one-shot clear is not enough), which
            // at INFO was 36 lines a minute per codex pane.
            let changed = if cleared {
                self.pane_tables.wheel_keys.remove(&sid).is_some()
            } else {
                self.pane_tables.wheel_keys.insert(sid, upd.clone()).as_ref() != Some(&upd)
            };
            if changed {
                lx_info!(
                    "shell.pane_wheel_keys.declared",
                    &format!(
                        "sid={sid} enter={:?} up={:?} down={:?} marker={:?} cleared={cleared}",
                        upd.enter, upd.up, upd.down, upd.marker
                    )
                );
            }
            self.send_pane_wheel_keys(&upd);
        }
        while let Ok(upd) = self.pane_badge_rx.try_recv() {
            self.pane_look.badges.declare(upd.shelld_session_id, upd.text);
        }
        let said = std::iter::from_fn(|| self.pane_title_rx.try_recv().ok()).map(|u| (u.shelld_session_id, u.text));
        plugins::helpers::titles::declare(&mut self.pane_look.titles, said);
        self.pane_look.strips_known |= plugins::helpers::strip::declare(&mut self.pane_look.strips);
        for (s, p, side, mask) in self.core_icons.drawn() {
            self.send(MsgType::IconMask, marspot::shell_proto::encode_icon_mask(s, p, side, &mask));
            self.draw_core_icons();
        }
        self.flush_pane_decls();

        // Drain cc inject-input requests onto the active core's
        // control socket as InjectInput frames; L2 routes by
        // session_id to the L3 owning that pane.
        while let Ok(req) = self.inject_input_rx.try_recv() {
            self.pane_status.poke();
            if let Some(conn) = self.active.as_ref() {
                match req.what {
                    plugins::host::InjectWhat::Input(bytes) => conn.send(
                        MsgType::InjectInput,
                        &marspot::shell_proto::encode_inject_input(
                            req.session_id,
                            &bytes,
                        ),
                    ),
                    plugins::host::InjectWhat::HoldGrid(on) => conn.send(
                        MsgType::PaneHoldGrid,
                        &marspot::shell_proto::encode_pane_hold_grid(
                            req.session_id,
                            on,
                        ),
                    ),
                    plugins::host::InjectWhat::Paste(text) => conn.send(
                        MsgType::PaneInjectPaste,
                        &marspot::shell_proto::encode_pane_inject_paste(
                            req.session_id,
                            &text,
                        ),
                    ),
                    plugins::host::InjectWhat::ResetMouseReporting => conn.send(
                        MsgType::PaneResetMouseReporting,
                        &marspot::shell_proto::encode_pane_reset_mouse_reporting(
                            req.session_id,
                        ),
                    ),
                };
            }
        }

        // 0a. Were *we* running?  See `witness_stall`.
        self.witness_stall(Instant::now());

        // 0b. Cool-down after a blown crash budget.  Not "until manual
        // intervention" any more: the four restarts that trip the
        // budget are usually a machine under load, and the load
        // passes.  Waiting one `CRASH_WINDOW` before the next attempt
        // keeps the restart *rate* bounded — the whole point of the
        // budget — without leaving the window frozen behind a banner
        // for the rest of the day.
        let cooldown = restart_cooldown(self.budget_trips);
        if self.auto_restart_disabled
            && self.disabled_at.is_some_and(|t| t.elapsed() >= cooldown)
        {
            lx_event!(
                "BUDGET_RECOVERED",
                "cool-down elapsed; trying the core once more",
                trips = self.budget_trips,
                cooldown_s = cooldown.as_secs()
            );
            sup_log::log(
                "BUDGET_RECOVERED",
                &format!("trips={} cooldown_s={}", self.budget_trips, cooldown.as_secs()),
            );
            self.auto_restart_disabled = false;
            self.disabled_at = None;
            self.crashes.clear();
            self.restart_core(ctx);
            self.refresh_banner(ctx);
            return;
        }

        // 1. Active core liveness — the core the user is looking at.
        let active_exited = match self.active.as_mut().and_then(|c| c.child.as_mut()) {
            Some(c) => matches!(c.try_wait(), Ok(Some(_))),
            None => false,
        };
        if active_exited {
            // Reap; restart_core below drops the rest of the conn.
            if let Some(c) = self.active.as_mut() {
                c.child.take();
            }
            self.record_crash();
            lx_event!(
                "CORE_GONE",
                "active core exited unexpectedly → restarting"
            );
            self.restart_core(ctx);
            return;
        }

        // 2. Active HELLO timeout.
        let hello_timed_out = self
            .active
            .as_ref()
            .map(|c| !c.hello_acked && c.spawned_at.elapsed() > HELLO_TIMEOUT)
            .unwrap_or(false);
        if hello_timed_out {
            lx_event!(
                "HELLO_TIMEOUT",
                "core failed to HelloAck → killing",
                timeout_s = HELLO_TIMEOUT.as_secs()
            );
            self.record_crash();
            self.restart_core(ctx);
            return;
        }

        // 3. Time to send the next ping?  Mutate the conn first, then
        // send (which borrows `self` immutably) with the new nonce.
        let now = Instant::now();
        let ping_nonce = self.active.as_mut().and_then(|c| {
            if c.hello_acked && now >= c.next_ping_at {
                c.last_ping_nonce = c.last_ping_nonce.wrapping_add(1);
                c.next_ping_at = now + PING_INTERVAL;
                Some(c.last_ping_nonce)
            } else {
                None
            }
        });
        if let Some(nonce) = ping_nonce {
            self.send(MsgType::Ping, encode_ping(nonce));
        }

        // 4. Pong deadline → hung.
        let pong_timed_out = self
            .active
            .as_ref()
            .map(|c| pong_overdue(c.hello_acked, c.last_pong_at, now))
            .unwrap_or(false);
        if pong_timed_out {
            lx_event!(
                "PONG_TIMEOUT",
                "no PONG → core hung; SIGKILL + restart",
                deadline_s = PONG_DEADLINE.as_secs()
            );
            self.record_crash();
            self.restart_core(ctx);
        }

        // 4b. Collect an in-flight Gatekeeper probe.  Nothing about the
        // outgoing process is disturbed while this is pending — it keeps
        // rendering and keeps taking input — so a probe that takes
        // minutes costs a late update, not a frozen window.
        if matches!(self.sup_state, SupervisorState::Probing) {
            match self.probe_rx.as_ref().map(mpsc::Receiver::try_recv) {
                // Still exec'ing.  Leave the state alone; we'll ask again
                // next tick.
                Some(Err(mpsc::TryRecvError::Empty)) => {}
                got => {
                    // A real answer, or the thread died without sending
                    // (Disconnected) / no channel at all.  Both of the
                    // latter mean "we did not prove anything can start",
                    // which is the same conclusion as a failed probe —
                    // and `ProbeVerdict::default()` is all-`None`, so
                    // nothing gets promoted or quarantined off it.
                    let verdict = match got {
                        Some(Ok(v)) => v,
                        _ => supervisor::ProbeVerdict::default(),
                    };
                    self.probe_rx = None;
                    self.sup_state = SupervisorState::Idle;
                    self.finish_probe(ctx, verdict);
                }
            }
        }

        // 5. Manual update trigger via SIGUSR1.  Lets a CLI invoke
        // `kill -USR1 $(pgrep marspot-shell)` to apply a staged
        // update on demand instead of waiting for focus-loss.
        if SIGUSR1_FLAG.swap(false, Ordering::AcqRel) {
            sup_log::log("SIGUSR1", "manual update trigger");
            if matches!(self.sup_state, SupervisorState::Idle) {
                self.apply_pending_update();
            } else {
                lx_warn!(
                    "shell.sigusr1.ignored_not_idle",
                    "SIGUSR1 ignored — supervisor not idle"
                );
            }
        }

        // after an exec of this shell: whether it is registered for
        // Apple events (one load per tick otherwise); the exec returns only on failure
        if let Some(n) = apple_events::tick(&self.proxy) {
            self.replace_image_for_apple_events(n);
        }
        self.show_held_frames();

        // 6. Recompute the banner once per tick (no-op if nothing changed).
        self.refresh_banner(ctx);
        lap!(7);
        if prof {
            supervisor_profile::wake_done();
        }
    }
}
