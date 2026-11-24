//! The plugin as the host drives it: init, tick, badge clicks and menus, stop.

use super::*;

impl Default for ClaudecodePlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for ClaudecodePlugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "claudecode",
            api_version: PLUGIN_API_VERSION,
            // 2 s — long enough to be near-free,short enough to catch
            // "assistant message done" within a couple seconds for
            // future notification feature.
            tick_interval_ms: 2000,
        }
    }

    fn init(&mut self, host: &dyn PluginHost) -> Result<(), PluginError> {
        // Persistence dir set up now so PluginError::IoError surfaces
        // a bad config early instead of mid-tick.
        let _ = host.state_dir()?;
        // Anything an earlier process left dormant still has its
        // scrollback and its uuid; the next scan re-arms the wake path.
        self.load_dormant(host);
        self.load_heads_up(host);
        self.load_trends(host);
        self.load_refused(host);
        self.load_fable_need(host);
        // Resolve ~/.claude/projects.  Without HOME there's nothing
        // to do — disable cleanly via a soft Err.
        let home = std::env::var_os("HOME").ok_or_else(|| {
            PluginError::Other("HOME not set; claudecode plugin idle".into())
        })?;
        let projects_root = PathBuf::from(home).join(".claude").join("projects");
        // Sessions and their processes come from the shell's shared
        // observation; ShelldClient is the L1→L2→L3 InjectInput
        // wire-frame proxy (set later by the registry via
        // `attach_inject_proxy`).
        self.shelld = Some(Arc::new(ShelldClient::new(host.cc_inject_proxy())));
        host.log(
            LogLevel::Info,
            "init.registry_walker",
            "cc reading sessions from the shared observation + InjectInput proxy",
        );
        host.log(
            LogLevel::Info,
            "init",
            &format!(
                "claudecode plugin initialised (projects_root={})",
                projects_root.display()
            ),
        );

        // Spin up the background scan worker.  All disk IO + pidtree
        // walks happen there; `tick` only drains the result channel
        // and pushes badges through the host.  Without this, the
        // tick blocked on `~/.claude/projects` IO and spiked to
        // 700ms-2s under disk contention — 3 such ticks tripped the
        // 100ms HOOK_BUDGET strike rule and the host permanently
        // disabled the plugin.
        let (req_tx, req_rx) = mpsc::channel::<()>();
        let (res_tx, res_rx) = mpsc::channel::<ScanResult>();
        let ctx = WorkerCtx {
            projects_root,
            caches: ScanCaches::default(),
            statusline_state: None,
            seen: HashMap::new(),
            model_cutoff: HashMap::new(),
            banner_tried: HashMap::new(),
            last_refusal: HashMap::new(),
            last_model: HashMap::new(),
            last_model_by_pane: HashMap::new(),
        };
        let handle = std::thread::Builder::new()
            .name("claudecode-scan".into())
            .spawn(move || worker_main(ctx, req_rx, res_tx))
            .map_err(|e| {
                PluginError::Other(format!("spawn claudecode-scan worker: {e}"))
            })?;
        self.worker = Some(handle);
        self.scan_req_tx = Some(req_tx);
        self.scan_res_rx = Some(std::sync::Mutex::new(res_rx));
        self.scan_inflight = false;

        self.initialised = true;
        Ok(())
    }

    fn tick(&mut self, host: &dyn PluginHost) {
        if !self.initialised {
            return;
        }
        // Where a tick's time went, so an overshoot names its own
        // cause.  33 of them in one day, up to 1.6 s, with nothing
        // logged around them (2026-09-27) — this hook runs on the
        // thread that draws the window, and the budget is 100 ms.
        let t_start = std::time::Instant::now();
        let mut t_apply = 0u128;
        let mut t_looks = 0u128;
        let mut t_activity = 0u128;
        let mut t_idle = 0u128;
        let mut t_sweep = 0u128;
        let mut t_rearm = 0u128;
        // (1) Drain any results the worker delivered since last tick.
        // Apply only the newest — older results are stale (the worker
        // already overrode their mapping in its next pass).
        let mut latest: Option<ScanResult> = None;
        let mut disconnected = false;
        if let Some(rx_lock) = &self.scan_res_rx {
            let rx = rx_lock.lock().unwrap();
            loop {
                match rx.try_recv() {
                    Ok(r) => {
                        latest = Some(r);
                        self.scan_inflight = false;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            host.log(
                LogLevel::Warn,
                "worker.gone",
                "scan worker channel closed; plugin idle until restart",
            );
            self.scan_res_rx = None;
            self.scan_req_tx = None;
            self.initialised = false;
            return;
        }
        let t_drain = t_start.elapsed().as_micros();
        if let Some(result) = latest {
            let t0 = std::time::Instant::now();
            // Replay log lines the worker accumulated.  Stays cheap:
            // a typical scan emits 0 lines (no transitions); only
            // first-discovery / mapping change ticks have any.
            for (lvl, tag, msg) in &result.log_lines {
                host.log(*lvl, tag, msg);
            }
            // Log mapping transitions (new bindings / lost bindings)
            // here on the main side — worker can't diff vs the host-
            // owned `self.last_mapping` cheaply.  Steady-state ticks
            // emit zero lines, so the file stays quiet.
            for (sh_sid, cc_sid) in &result.new_mapping {
                let prev = self.last_mapping.get(sh_sid);
                if prev.map(|p| p != cc_sid).unwrap_or(true) {
                    host.log(
                        LogLevel::Info,
                        "session.bound",
                        &format!(
                            "shelld_session={} → claudecode sid={}",
                            sh_sid, cc_sid
                        ),
                    );
                }
            }
            t_apply = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            self.publish_looks(host, &result);
            t_looks = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            // Report this layer to the shell, which owns the state
            // machine.  The plugin does NOT compose its view with the
            // kernel's and does not decide what is actionable — it
            // only says what its own transcript shows.  Every live
            // session gets a report, including `Absent` for panes with
            // no claude in them: silence and "not here" are different
            // facts, and the machine distinguishes them.
            for (sid, activity) in &result.new_activity {
                let _ = host.report_pane_activity(*sid, *activity);
            }
            for sid in result.sessions_seen.iter() {
                if !result.new_activity.contains_key(sid) {
                    let _ = host.report_pane_activity(
                        *sid,
                        activity_for_unbound(*sid, &self.dormant),
                    );
                }
            }
            t_activity = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            // Idle reclamation runs off the same scan: the CPU samples
            // it compares were taken by the worker in the same pass
            // that produced these bindings.
            self.sweep_unusable_profiles(host, &result);
            t_sweep = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            self.run_idle_policy(host, &result);
            t_idle = t0.elapsed().as_micros();
            let t0 = std::time::Instant::now();
            // A pane that lost its claude while we had it marked
            // dormant, with no session of ours attached (an L1
            // self-execv drops every PaneSession), needs its wake path
            // put back — otherwise the next keystroke lands in the
            // shell and the session is only recoverable by hand.
            self.rearm_dormant(host, &result);
            t_rearm = t0.elapsed().as_micros();
            self.quiet.scanned(result.sessions_seen.iter().copied());
            self.last_mapping = result.new_mapping;
            self.last_meta = result.new_meta;
            self.last_model = result.new_models;
            self.last_vetoes = result.new_vetoes;
        }

        // (2) Kick off the next scan if the worker is idle.  At most
        // one in-flight request: if the worker is still chewing on
        // the previous (slow disk), we skip — preserves the worker
        // queue from growing unbounded.
        // With claude in no pane a scan is due only when `due` says so.
        if !self.scan_inflight
            && !matches!(self.due(), crate::plugins::TickDue::At(t) if Instant::now() < t)
            && let Some(tx) = &self.scan_req_tx {
                if tx.send(()).is_ok() {
                    self.scan_inflight = true;
                    self.quiet.sent();
                } else {
                    host.log(
                        LogLevel::Warn,
                        "worker.send_failed",
                        "scan worker dropped request channel",
                    );
                    self.scan_req_tx = None;
                    self.initialised = false;
                    return;
                }
            }

        // (3) RFC-003 profile-cycle state machine lives in
        // ProfileCyclePaneSession::on_tick, driven by the L1 plugin
        // dispatcher (not here).
        //
        // C7 auto-retry monitors: cheap host-side bookkeeping, no
        // disk IO.  `monitor_unsupported` latches true on first
        // tick (PTY raw broadcast not wired yet) so the body is a
        // no-op in steady state.
        let t0 = std::time::Instant::now();
        self.refresh_monitors(host);
        self.pump_monitors(host);
        let t_monitors = t0.elapsed().as_micros();

        let total = t_start.elapsed().as_micros();
        if total > 20_000 {
            host.log(
                LogLevel::Warn,
                "tick.slow",
                &format!(
                    "total={total} drain={t_drain} apply={t_apply} looks={t_looks} \
                     activity={t_activity} sweep={t_sweep} idle={t_idle} rearm={t_rearm} \
                     monitors={t_monitors} (µs)"
                ),
            );
        }
    }

    fn tick_due(&self) -> crate::plugins::TickDue {
        self.due()
    }

    /// Where the user is.
    ///
    /// The only input this plugin has about the *person* rather than
    /// the session.  Everything else it weighs — transcript age, CPU,
    /// child processes — describes what claude is doing, and none of it
    /// can tell "half an hour since the last turn" apart from "half an
    /// hour since the last turn, and they are sitting right here
    /// reading it".
    fn on_pane_focused(&mut self, _host: &dyn PluginHost, shelld_session_id: u64) {
        self.focused_sid = Some(shelld_session_id);
    }

    fn on_pane_badge_click(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) {
        self.start_profile_cycle(host, shelld_session_id);
    }

    fn pane_badge_menu(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
    ) -> Vec<marspot::shell_proto::PaneBadgeMenuItem> {
        // Both ways out of here used to be silent, and an empty menu
        // opens nothing — so a right-click on the badge that did
        // nothing left no trace anywhere to say which of the two it
        // was.  The badge is drawn from a look this plugin published
        // and the core holds until told otherwise; the menu is
        // computed live from `last_meta`.  The two can disagree, and
        // when they do the badge is on screen with nothing behind it.
        let Some(meta) = self.last_meta.get(&shelld_session_id) else {
            host.log(
                LogLevel::Warn,
                "badge_menu.no_bind",
                &format!(
                    "shelld_session={} has a badge but no binding; menu empty",
                    shelld_session_id
                ),
            );
            return Vec::new();
        };
        let profiles = discover_profiles();
        let mut items = badge_menu_for(meta.profile_num, &profiles);
        items.extend(crate::plugins::handoff::menu_rows(crate::plugins::handoff::Agent::Claude));
        if items.is_empty() {
            host.log(
                LogLevel::Warn,
                "badge_menu.empty",
                &format!(
                    "shelld_session={} current=P{} discovered={:?}; nothing to offer",
                    shelld_session_id, meta.profile_num, profiles
                ),
            );
        }
        items
    }

    fn on_pane_badge_menu_action(
        &mut self,
        host: &dyn PluginHost,
        shelld_session_id: u64,
        tag: u32,
    ) {
        use crate::plugins::handoff::{self, Pick};
        // every plugin hears every pick; only claude's own rows are read
        let target = match handoff::parse_tag(handoff::Agent::Claude, tag) {
            None => return,
            Some(Pick::HandOff(to, n)) => {
                if self.last_meta.contains_key(&shelld_session_id) {
                    self.hand_off(host, shelld_session_id, to, n);
                }
                return;
            }
            Some(Pick::Profile(n)) => n,
        };
        let Some(meta) = self.last_meta.get(&shelld_session_id).cloned() else {
            host.log(
                LogLevel::Warn,
                "cycle.menu_no_bind",
                &format!("menu pick on unbound shelld_session={}", shelld_session_id),
            );
            return;
        };
        if meta.profile_num == target {
            // Stale menu — the pane already cycled here.  Nothing to do.
            return;
        }
        host.log(
            LogLevel::Info,
            "cycle.menu_pick",
            &format!(
                "shelld_session={} P{} → P{} uuid={}",
                shelld_session_id, meta.profile_num, target, meta.uuid
            ),
        );
        // A menu pick is someone who is right here.
        self.switch_by_hand(host, shelld_session_id, meta, target);
    }

    fn stop(&mut self, host: &dyn PluginHost) {
        host.log(LogLevel::Info, "stop", "plugin stopped");
        self.initialised = false;
        // Drop the request channel — the worker's recv loop sees Err
        // and exits cleanly.  Then join so the thread is fully torn
        // down before the plugin slot is dropped (otherwise the
        // thread would outlive the plugin briefly and the next
        // start would race the previous one's last send).
        self.scan_req_tx = None;
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
        self.scan_res_rx = None;
        self.scan_inflight = false;
        // after the join: the worker's last scan published too
        with_helpers::turns::publish(Vec::new());
    }
}
