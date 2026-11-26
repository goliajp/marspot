//! Pane sessions: a plugin or the supervisor holding one pane's input
//! for a while, and the host each callback is given.

use super::*;

/// Bundle: the plugin's session object + the metadata we need to log
/// + cleanly tear it down (plugin name for log namespace).
///
/// Starts the queue's runs.
///
/// One method's worth of type, but it must not be the plugin host:
/// that one gates `begin_pane_session` on the *current plugin's*
/// permissions, and a run the supervisor starts has no plugin behind
/// it.  Routing through it denied the first CLI delivery with
/// `missing permission: PermissionSet(16)`, from the permission model
/// since removed — the supervisor asking
/// itself for permission it has no identity to hold.
pub(super) struct SupervisorOpHost {
    pub(super) begin_tx: std::sync::mpsc::Sender<plugins::host::PaneSessionBeginRequest>,
}

impl plugins::pty_op::OpHost for SupervisorOpHost {
    fn begin(
        &self,
        shelld_session_id: u64,
        session: Box<dyn plugins::PaneSession>,
    ) -> Result<(), String> {
        self.begin_tx
            .send(plugins::host::PaneSessionBeginRequest {
                shelld_session_id,
                plugin_name: "pty_op",
                session,
            })
            .map_err(|_| "main loop dropped".to_string())
    }

    fn log(&self, level: plugins::LogLevel, tag: &str, msg: &str) {
        match level {
            plugins::LogLevel::Warn => {
                lx_warn!("shell.pty_op", msg, tag = tag)
            }
            _ => lx_info!("shell.pty_op", msg, tag = tag),
        }
    }
}

pub(super) struct ActivePaneSession {
    pub(super) session: Box<dyn plugins::PaneSession>,
    plugin_name: &'static str,
}

/// Concrete `PaneSessionHost` constructed per-dispatch; lives only
/// for the duration of one plugin callback.  set_badge → the plugin
/// host's badge path (the same change check a plugin's badge goes
/// through), end → flips a `Cell` the main-loop checks after the
/// callback returns.  log → logx via the plugin namespace.
struct ConcretePaneSessionHost<'a> {
    sid: u64,
    plugin_name: &'static str,
    plugin_host: &'a ShellPluginHost,
    end_requested: &'a std::cell::Cell<bool>,
}

impl<'a> plugins::PaneSessionHost for ConcretePaneSessionHost<'a> {
    fn shelld_session_id(&self) -> u64 {
        self.sid
    }
    fn end(&self) {
        self.end_requested.set(true);
    }
    fn set_badge(&self, text: &str) {
        self.plugin_host.send_pane_badge(self.sid, text);
    }
    fn log(&self, level: plugins::LogLevel, tag: &str, msg: &str) {
        let composed = format!("plugin.{}.{}", self.plugin_name, tag);
        match level {
            plugins::LogLevel::Debug => marspot::lx_debug!(&*composed, msg),
            plugins::LogLevel::Info => marspot::lx_info!(&*composed, msg),
            plugins::LogLevel::Warn => marspot::lx_warn!(&*composed, msg),
        }
    }
}

impl ShellApp {
    /// Drain queued PaneSession take-over requests + drive each
    /// active session's on_tick.  Called by the supervisor poll
    /// alongside the regular plugin tick.
    pub(super) fn process_pane_sessions(&mut self) {
        // Drain begin requests → register + emit PaneSessionBegin.
        while let Ok(req) = self.pane_session_begin_rx.try_recv() {
            if self.active_pane_sessions.contains_key(&req.shelld_session_id) {
                lx_warn!(
                    "shell.pane_session.duplicate_begin",
                    "rejecting duplicate PaneSession begin",
                    shelld_session_id = req.shelld_session_id,
                    plugin = req.plugin_name
                );
                continue;
            }
            let caps = req.session.caps();
            if let Some(conn) = self.active.as_ref() {
                conn.send(
                    MsgType::PaneSessionBegin,
                    &marspot::shell_proto::encode_pane_session_begin(
                        req.shelld_session_id,
                        caps,
                    ),
                );
            }
            self.active_pane_sessions.insert(
                req.shelld_session_id,
                ActivePaneSession {
                    session: req.session,
                    plugin_name: req.plugin_name,
                },
            );
            lx_event!(
                "PANE_SESSION_BEGIN",
                "plugin took over pane",
                shelld_session_id = req.shelld_session_id,
                plugin = req.plugin_name,
                caps = caps
            );
        }
        // Tick every active session.  end_requested flag flushed after.
        let sids: Vec<u64> = self.active_pane_sessions.keys().copied().collect();
        for sid in sids {
            let end_flag = std::cell::Cell::new(false);
            let host = ConcretePaneSessionHost {
                sid,
                plugin_name: self.active_pane_sessions[&sid].plugin_name,
                plugin_host: &self.plugin_host,
                end_requested: &end_flag,
            };
            if let Some(active) = self.active_pane_sessions.get_mut(&sid) {
                active.session.on_tick(&host);
            }
            if end_flag.get() {
                self.end_pane_session(sid, plugins::EndReason::PluginRequested);
            }
        }
    }

    /// Hand a focus change to the pane's own session, if it has one.
    ///
    /// Mirrors `dispatch_pane_session_key` — same host, same
    /// end-on-request handling — because a restore that starts when
    /// the user looks at the pane is the difference between "it came
    /// back" and "it took forever".
    pub(super) fn dispatch_pane_session_focus(&mut self, sid: u64) {
        let Some(active) = self.active_pane_sessions.get_mut(&sid) else {
            return;
        };
        let plugin_name = active.plugin_name;
        let end_flag = std::cell::Cell::new(false);
        let host = ConcretePaneSessionHost {
            sid,
            plugin_name,
            plugin_host: &self.plugin_host,
            end_requested: &end_flag,
        };
        active.session.on_focus(&host);
        if end_flag.get() {
            self.end_pane_session(sid, plugins::EndReason::PluginRequested);
        }
    }

    pub(super) fn dispatch_pane_session_key(
        &mut self,
        sid: u64,
        ev: marspot::shell_proto::WireKeyEvent,
    ) {
        let Some(active) = self.active_pane_sessions.get_mut(&sid) else {
            return;
        };
        let plugin_name = active.plugin_name;
        let end_flag = std::cell::Cell::new(false);
        let host = ConcretePaneSessionHost {
            sid,
            plugin_name,
            plugin_host: &self.plugin_host,
            end_requested: &end_flag,
        };
        let handling = active.session.on_user_key(&host, &ev);
        if end_flag.get() || handling == plugins::KeyHandling::EndSession {
            self.end_pane_session(sid, plugins::EndReason::PluginRequested);
        }
    }

    pub(super) fn end_pane_session(&mut self, sid: u64, reason: plugins::EndReason) {
        let Some(mut active) = self.active_pane_sessions.remove(&sid) else {
            return;
        };
        let plugin_name = active.plugin_name;
        // Read before `on_end` — that is where the hold is released,
        // and after it this session no longer speaks for the pane.
        let froze_picture = active.session.caps()
            & marspot::shell_proto::PANE_SESSION_CAP_FREEZE_GRID
            != 0;
        let end_flag = std::cell::Cell::new(false);
        let host = ConcretePaneSessionHost {
            sid,
            plugin_name,
            plugin_host: &self.plugin_host,
            end_requested: &end_flag,
        };
        active.session.on_end(&host, reason);
        // Tell L2 to leave the locked / frozen state.
        if let Some(conn) = self.active.as_ref() {
            conn.send(
                MsgType::PaneSessionEnd,
                &marspot::shell_proto::encode_pane_session_end(sid),
            );
        }
        lx_event!(
            "PANE_SESSION_END",
            "pane session torn down",
            shelld_session_id = sid,
            plugin = plugin_name,
            reason = format!("{:?}", reason)
        );
        // The freeze just lifted.  `sweep_pane_status` held this pane's
        // brightness for as long as its picture was held, so the pane
        // is still wearing the level it was parked at — and the sweep
        // that would notice is up to a second away.  A second is long
        // enough to watch the content come back and the pane light up
        // afterwards as two separate events.
        //
        // Sweeping *here* rather than sending a level from the last
        // snapshot: that snapshot describes the parked pane (the
        // machines kept running while only the presentation was
        // frozen), so it would dim the pane at the exact moment its
        // program came back.  Stepping the machines first is what makes
        // the level true.
        if froze_picture {
            self.pane_status.force_next_sweep();
            self.sweep_pane_status();
        }
    }
}
