//! Keeping the core's per-pane settings in step, waking parked panes
//! and sweeping each pane's status.

use super::*;

impl ShellApp {
    /// Tear down the active core (clean EOF then SIGKILL + reap) and
    /// clear the slot.  No-op when there's no active core.
    ///
    /// `reason` is the forensic anchor — every caller passes a short
    /// static string saying WHY we're closing the connection (crash
    /// detected, manual update, window close, etc.).  Without this
    /// the supervisor sees `CORE_EXIT control socket closed by shell`
    /// in `marspot.log` and the only thing the user can say is "okay,
    /// shell closed it — but why?".  The 2026-06-15 incident debugging
    /// loop made this hole obvious: six core boots in three minutes
    /// and no log entry telling us which path was firing them.
    pub(super) fn send_pane_agent_tui(&self, sid: u64, on: bool) {
        let Some(conn) = self.active.as_ref() else {
            return;
        };
        conn.send(
            MsgType::PaneAgentTui,
            marspot::shell_proto::encode_pane_agent_tui(sid, on),
        );
    }

    pub(super) fn send_pane_render_markup(&self, sid: u64, on: bool) {
        let Some(conn) = self.active.as_ref() else {
            return;
        };
        conn.send(
            MsgType::PaneRenderMarkup,
            marspot::shell_proto::encode_pane_render_markup(sid, on),
        );
    }

    pub(super) fn send_pane_wheel_keys(&self, upd: &plugins::host::PaneWheelKeysUpdate) {
        let Some(conn) = self.active.as_ref() else {
            return;
        };
        conn.send(
            MsgType::PaneWheelKeys,
            marspot::shell_proto::encode_pane_wheel_keys(
                upd.shelld_session_id,
                &upd.enter,
                &upd.up,
                &upd.down,
                &upd.marker,
            ),
        );
    }

    /// Replay every wheel declaration into a freshly-attached core.
    ///
    /// Without this a core swap silently disarms the wheel for every
    /// pane that had declared before it — see `PaneTables::wheel_keys`.
    pub(super) fn replay_pane_wheel_keys(&self) {
        if self.pane_tables.wheel_keys.is_empty() {
            return;
        }
        lx_info!(
            "shell.pane_wheel_keys.replayed",
            &format!("panes={}", self.pane_tables.wheel_keys.len())
        );
        for upd in self.pane_tables.wheel_keys.values() {
            self.send_pane_wheel_keys(upd);
        }
        for (sid, on) in &self.pane_tables.render_markup {
            self.send_pane_render_markup(*sid, *on);
        }
    }

    /// The user is back: line up every parked pane to be woken.
    ///
    /// `MARSPOT_NO_WAKE_PREFETCH=1` turns it off — the wakes then
    /// happen on the click, as they did before.
    pub(super) fn queue_parked_wakes(&mut self) {
        if std::env::var_os("MARSPOT_NO_WAKE_PREFETCH").is_some()
            || !marspot::settings::get().reclaim_prefetch
        {
            return;
        }
        let parked: Vec<u64> = self
            .active_pane_sessions
            .iter()
            .filter(|(_, a)| a.session.parked())
            .map(|(sid, _)| *sid)
            .collect();
        if parked.is_empty() {
            return;
        }
        for sid in &parked {
            if !self.wake_queue.contains(sid) {
                self.wake_queue.push_back(*sid);
            }
        }
        // First one now; the rest follow on the stagger.
        self.wake_next_at = Some(Instant::now());
        lx_event!(
            "WAKE_PREFETCH",
            "user is back; waking the parked panes ahead of the click",
            panes = parked.len()
        );
    }

    /// Release one queued wake per [`Self::WAKE_STAGGER`].
    pub(super) fn drive_wake_queue(&mut self) {
        if self.wake_queue.is_empty() {
            return;
        }
        match self.wake_next_at {
            Some(t) if Instant::now() < t => return,
            _ => {}
        }
        let Some(sid) = self.wake_queue.pop_front() else { return };
        self.wake_next_at = Some(Instant::now() + Self::WAKE_STAGGER);
        // Still parked?  The user may have clicked it themselves in the
        // meantime, and waking a run that has moved on is not harmless
        // — it would be typing at whatever is there now.
        if !self
            .active_pane_sessions
            .get(&sid)
            .is_some_and(|a| a.session.parked())
        {
            return;
        }
        self.dispatch_pane_session_focus(sid);
    }

    /// Is this pane's picture currently frozen by a PaneSession?
    ///
    /// The op that asked for the freeze is the one wearing
    /// `FREEZE_GRID`, so the capability is the answer — no second
    /// bookkeeping to keep in step with the first.
    fn pane_picture_held(&self, sid: u64) -> bool {
        self.active_pane_sessions.get(&sid).is_some_and(|s| {
            s.session.caps() & marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID != 0
        })
    }

    pub(super) fn sweep_pane_status(&mut self) {
        // None = interval gate; the machines weren't stepped.
        let now = Instant::now();
        let Some(changes) = self.pane_status.sweep(now) else {
            return;
        };
        for c in &changes {
            // INFO, not DEBUG: the runtime default level is Info, so a
            // DEBUG line does not exist on a real machine — and the
            // transition history is what any future threshold gets
            // calibrated against.  Rate is capped by construction: one
            // line per pane per committed transition, and a pane that
            // sits quiet for an hour prints once.
            lx_info!(
                "shell.pane_state.changed",
                "pane state machine committed a transition",
                sid = c.sid,
                from = c.change.from.label().as_str(),
                held_s = c.change.held.as_secs(),
                to = c.change.to.label().as_str()
            );
        }
        let snapshot = self.pane_status.snapshot(now);
        // Tell L2 how each pane should look.  On change only: a pane
        // that has been resting for an hour costs one frame, not one
        // per second.
        for (sid, (status, held, _)) in &snapshot {
            // A pane whose picture is held keeps the brightness it was
            // held at.  The hold stops the *cells* moving, not the byte
            // traffic underneath — reclaiming a pane kills the program
            // in it, which produces output, which reads as "busy" and
            // pulled the pane to full brightness, then to dormant a few
            // seconds later.  Observed 2026-08-03: content perfectly
            // still, pane visibly flashing bright and then dimming two
            // steps.  Everything drawn around a frozen picture has to
            // be frozen with it or the pane announces what is being
            // done to it.
            if self.pane_picture_held(*sid) {
                continue;
            }
            let level = pane_status::recede_level_for(status, *held);
            if self.pane_look.recede.declare(*sid, level) {
                // On change only — a pane that stays rested for an
                // hour is one line, not one per second.  Worth having:
                // "the pane looks wrong" is otherwise unanswerable
                // without a screenshot.
                lx_info!(
                    "shell.pane_recede.changed",
                    "pane recede level changed",
                    sid = *sid,
                    level = level as u64,
                    state = status.label().as_str(),
                    held_s = held.as_secs()
                );
            }
        }
        // a quiet pane's next output is a change of state
        for sid in snapshot.keys() {
            control_reader::pane_wakes::arm(*sid, self.pane_status.pty_quiet_now(*sid, now));
        }
        // Closed panes leave every per-pane table here, against the
        // same map the plugin host drops its memo by: a repeat the host
        // swallows must be one these tables still hold.
        self.forget_closed_panes(|sid| snapshot.contains_key(&sid));
        self.plugin_host.publish_pane_status(snapshot);
        self.flush_pane_decls();
    }

    /// A session the registry no longer lists is a closed pane: end what
    /// L1 still runs on it and drop it from every per-pane table.
    fn forget_closed_panes(&mut self, open: impl Fn(u64) -> bool) {
        let closed: Vec<u64> =
            self.active_pane_sessions.keys().copied().filter(|sid| !open(*sid)).collect();
        for sid in closed {
            self.end_pane_session(sid, plugins::EndReason::PaneClosed);
        }
        self.pty_ops.retain(&open);
        self.pane_look.retain(&open);
        self.pane_tables.retain(&open);
        self.wake_queue.retain(|sid| open(*sid));
        control_reader::pane_wakes::retain(&open);
    }

    /// Send the current core every badge, title and recede level it
    /// has not been sent.  Nothing goes to a core before its HelloAck:
    /// that is where `core_replaced` marks everything pending, and a
    /// frame written before it would only be written again.
    pub(super) fn flush_pane_decls(&mut self) {
        let Some(conn) = self.active.as_ref().filter(|c| c.hello_acked) else {
            return;
        };
        self.pane_look.flush(|t, payload| conn.send(t, payload));
    }
}
