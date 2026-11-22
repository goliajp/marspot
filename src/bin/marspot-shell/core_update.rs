//! Taking a staged core update through probation, swapping it in or
//! rolling it back, and restarting a core that crashed or hung.

use super::*;

impl ShellApp {
    /// Start applying a pending update: pick the layer, then hand off
    /// to a background Gatekeeper probe.  Nothing is promoted, killed
    /// or spawned here — `finish_probe` does that once the staged
    /// binary has proven it starts.
    ///
    /// History, part 1 — why the swap is single-core.  It used to run a
    /// dual-core probation pattern: active + pending L2 in parallel for
    /// 30 s, presenter atom-swaps to pending if it survives.  That
    /// assumed pending + active could share L3 control connections (the
    /// L4-shelld model where shelld was a multi-consumer broker).
    /// RFC-003 made L3 single-client: as soon as the pending core's UDS
    /// hello reaches L3, L3 closes the previous client (= active core).
    /// Result for the full probation window: active core has dead L3
    /// control sockets, L1 still routes input to active, every
    /// keystroke drops on the floor.  The user sees panes (active still
    /// reads grid shm) but can't type.  So: kill active, spawn new from
    /// promoted current/, let the new core reattach via registry.  L3
    /// self-execv + state.bin reattach means each L3 survives that on
    /// its own; the IOSurface pair is reused, so no presenter handshake
    /// is required either.
    ///
    /// History, part 2 — why the probe came later.  That swap's cost
    /// was budgeted as "the ~200-500 ms gap from old-core-down to
    /// new-core's first SurfaceReady", which held right up until the
    /// gap stopped being bounded.  On 2026-07-29 a cargo build in
    /// another project flooded `syspolicyd`, macOS took 204 s to assess
    /// the freshly staged `marspot-core`, and the swap had already
    /// retired the old one — so the window sat frozen on a dead core's
    /// last frame for three and a half minutes, and relaunching only
    /// queued more cores behind the same stall.  The probe puts that
    /// wait back where it can be afforded: before anything is retired.
    pub(super) fn apply_pending_update(&mut self) {
        if !matches!(self.sup_state, SupervisorState::Idle) {
            return;
        }
        // Probe every layer that has a candidate in the SAME round.
        // `install-local.sh` stages shell and core together, and
        // handling them as one round is what keeps the core from being
        // started twice: knowing the core is good before we exec means
        // the successor shell spawns it directly, instead of booting
        // the outgoing core and swapping it out seconds later.
        let shell_candidate = supervisor::BinaryTree::for_shell()
            .ok()
            .filter(|t| t.has_pending())
            .map(|t| t.pending());
        let core_candidate = self
            .binaries
            .has_pending()
            .then(|| self.binaries.pending());
        if shell_candidate.is_some() || core_candidate.is_some() {
            self.start_probe(shell_candidate, core_candidate);
        }
    }

    /// Exec a staged binary's `--version` on a background thread so the
    /// kernel charges its Gatekeeper assessment now, while the process
    /// this update is about to replace is still doing its job.
    ///
    /// Nothing here touches `current/`: the candidate is still sitting
    /// in `pending/`.  `promote_pending` moves it with `rename`, which
    /// preserves the inode, so the verdict this warms is the very one
    /// the real spawn will hit.
    ///
    /// Deliberately un-timed.  A slow verdict has to degrade to "the
    /// update lands later", never to "retire the old one anyway and
    /// hope" — that second shape is what froze the window for 204 s on
    /// 2026-07-29.
    fn start_probe(
        &mut self,
        shell_candidate: Option<std::path::PathBuf>,
        core_candidate: Option<std::path::PathBuf>,
    ) {
        lx_event!(
            "UPDATE_PROBE_START",
            "warming staged binaries' Gatekeeper verdicts off the main thread",
            shell = shell_candidate
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into()),
            core = core_candidate
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "-".into())
        );
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // Sequential on purpose: two concurrent first-execs just
            // queue behind the same Gatekeeper anyway, and serialising
            // keeps the wall-clock attributable per layer in the log.
            let verdict = ProbeVerdict {
                shell: shell_candidate
                    .as_deref()
                    .map(marspot::binary_tree::can_start),
                core: core_candidate.as_deref().map(shader_warm::can_start_warmed),
            };
            let _ = tx.send(verdict);
        });
        self.probe_rx = Some(rx);
        self.sup_state = SupervisorState::Probing;
    }

    /// Route a finished probe.  A good verdict hands off to that layer's
    /// real swap; a bad one quarantines the candidate — so the next
    /// trigger doesn't retry the same dead binary forever — and leaves
    /// the running process untouched.
    pub(super) fn finish_probe(&mut self, ctx: &MarspotAppCtx, v: ProbeVerdict) {
        lx_event!("UPDATE_PROBE_DONE", "probe round finished", verdict = v.summary());
        let plan = v.plan();

        // Quarantine whatever failed, before acting on whatever passed.
        if plan.quarantine_shell {
            self.quarantine_rejected("shell", supervisor::BinaryTree::for_shell().and_then(|t| t.quarantine_pending()));
        }
        if plan.quarantine_core {
            self.quarantine_rejected("core", self.binaries.quarantine_pending());
        }

        // A good core promoted BEFORE the exec is the whole point of
        // probing both layers together: the successor shell finds the
        // new binary already in `current/` and spawns it once.  Without
        // this, it boots the outgoing core (still in `current/` at exec
        // time) and a second probe + swap retires it seconds later —
        // two core starts, two L3 control reconnects, for one install.
        if plan.promote_core_before_exec {
            match self.binaries.promote_pending() {
                Ok(()) => {
                    lx_event!(
                        "CORE_PROMOTED_FOR_EXECV",
                        "staged core promoted ahead of the shell exec; \
                         the successor spawns it directly"
                    );
                    sup_log::log(
                        "CORE_PROMOTED_FOR_EXECV",
                        "pending/marspot-core → current/ before shell execv",
                    );
                }
                Err(e) => {
                    // Not fatal: the successor boots the old core and
                    // its own trigger swaps it the usual way.
                    lx_warn!(
                        "shell.update.core_preprompte_failed",
                        &format!("{e} — successor will swap it separately")
                    );
                }
            }
        }

        if plan.then == supervisor::AfterProbe::ShellExec {
            // The successor opens ITS window 0 at the frame we hand
            // over, so hand over window 0's frame — not `ctx`'s.  This
            // path runs from the redraw pump, whose ctx is whichever
            // window happened to drive the tick, so with two windows
            // open the boot window could be told to open where the
            // second one was.  `last_saved_window` is window 0's own
            // frame, kept current by its move / resize callbacks.
            let frame0 = self.windows[0]
                .last_saved_window
                .map(|(x, y, w, h, _)| (x, y, w, h))
                .unwrap_or_else(|| ctx.window_frame_pt());
            // Returns only if the exec failed — on success this process
            // is already the new image.
            self.finish_shell_self_update(frame0);
            return;
        }
        if plan.then == supervisor::AfterProbe::CoreSwap {
            self.perform_core_swap(ctx);
        }
    }

    /// Log one rejected candidate's quarantine outcome.
    fn quarantine_rejected(&self, layer: &str, outcome: std::io::Result<()>) {
        lx_event!(
            "UPDATE_PROBE_REJECT",
            "staged binary could not start — quarantining it, leaving the running one alone",
            layer = layer
        );
        sup_log::log(
            "UPDATE_PROBE_REJECT",
            &format!("pending/{layer} failed its start probe; quarantined"),
        );
        if let Err(e) = outcome {
            lx_warn!(
                "shell.update.quarantine_failed",
                &format!("{e}"),
                layer = layer
            );
        }
    }

    /// The single-core swap itself, reached only once the probe proved
    /// the staged core can start.  From here everything is the fast
    /// path — the Gatekeeper verdict is cached, so the gap between the
    /// old core dying and the new one drawing is back inside the
    /// ~200-500 ms this design was built around.
    fn perform_core_swap(&mut self, ctx: &MarspotAppCtx) -> bool {
        if !self.binaries.has_pending() {
            return false;
        }
        // The core is spawned against a pair of its own for the boot
        // window (its ids ride in on env); the pair on the window keeps
        // showing the old core's last frame until the new core's first.
        // Any further window announces itself to the fresh core with a
        // `SurfaceAttachWindow` frame.
        let Some((w_px, h_px)) = self.windows[0].surfaces.as_ref().map(|s| (s.width(), s.height())) else {
            lx_warn!("shell.apply_update.no_surface", "no displayed surface to swap onto");
            return false;
        };
        let (front_id, back_id) = match self.fresh_pending_pair(0, w_px, h_px) {
            Ok(ids) => ids,
            Err(e) => {
                lx_warn!("shell.apply_update.no_surface", &e);
                return false;
            }
        };
        let scale = ctx.scale();

        lx_event!("UPDATE_APPLY", "starting single-core in-place swap");
        sup_log::log(
            "UPDATE_APPLY",
            "promoting pending → current (single-core swap)",
        );
        if let Err(e) = self.binaries.promote_pending() {
            lx_event!(
                "UPDATE_FAIL",
                "promote_pending failed; active core untouched",
                error = format!("{e}")
            );
            sup_log::log("UPDATE_FAIL", &format!("promote_pending: {e}"));
            return false;
        }
        // Tear down the active core BEFORE spawning the new one: its
        // L3 control connections (one per pane) must be closed before
        // the new core's hello reaches L3, otherwise L3 sees a brief
        // window where the new client kicks the old (same race that
        // motivated this rewrite — it's harmless here but cleaner to
        // make the ordering explicit).  shutdown_active blocks until
        // the active core process is reaped.
        self.shutdown_active("retiring active for single-core swap (UPDATE_SWAP)");
        // A fresh core knows nothing about the panes, so the memo of
        // what each pane has already been told has to go with the old
        // one -- see `ShellPluginHost::declared`.
        self.plugin_host.forget_declarations();
        self.active = match self.spawn_core(front_id, back_id, w_px, h_px, scale) {
            Some(c) => {
                self.record_core_boot();
                Some(c)
            }
            None => {
                lx_event!("UPDATE_FAIL", "spawn new active failed");
                sup_log::log("UPDATE_FAIL", "spawn new active");
                self.rollback_binary("spawn new active failed");
                return false;
            }
        };
        self.announce_windows_to_new_core(ctx);
        // Commit the update — drop the rollback target.
        match self.binaries.finalize_stable() {
            Ok(()) => sup_log::log("UPDATE_STABLE", "single-core swap; prev/ deleted"),
            Err(e) => {
                lx_event!(
                    "FINALIZE_FAIL",
                    "finalize_stable failed",
                    error = format!("{e}")
                );
                sup_log::log("FINALIZE_FAIL", &format!("{e}"));
            }
        }
        lx_event!("UPDATE_SWAP", "single-core swap complete");
        sup_log::log("UPDATE_SWAP", "active → new binary; old core retired");
        true
    }

    /// Roll `current/marspot-core` back to `prev/` after a failed
    /// update attempt.  The active core is still running the old binary,
    /// so this just realigns the binary tree with reality.
    fn rollback_binary(&self, reason: &str) {
        match self.binaries.rollback_to_prev() {
            Ok(true) => {
                lx_event!("ROLLBACK", "rolled back to prev/", reason = reason);
                sup_log::log("ROLLBACK", "prev/ → current/");
            }
            Ok(false) => {
                lx_event!("ROLLBACK_NOOP", "no prev/ to restore", reason = reason);
                sup_log::log("ROLLBACK_NOOP", "no prev/ to restore");
            }
            Err(e) => {
                lx_event!("ROLLBACK_FAIL", "rollback_to_prev failed", error = format!("{e}"));
                sup_log::log("ROLLBACK_FAIL", &format!("{e}"));
            }
        }
    }

    /// Resolve which banner (if any) the current shell state wants
    /// to show, and push it into the presenter if it changed.
    ///
    /// Note: an update being probed shows **no** banner — the whole
    /// point is invisibility, the active core keeps rendering normally
    /// while the staged binary's Gatekeeper verdict warms up.
    pub(super) fn refresh_banner(&mut self, ctx: &MarspotAppCtx) {
        // The condition is about the CORE, which every window shares —
        // so the banner goes into every window's presenter, not just
        // whichever one's callback happened to run first.  With one
        // `banner_kind` compared against and one presenter written, the
        // second window silently never showed (or never cleared) it.
        let any_attached = self.windows.iter().any(|w| w.surfaces.is_some());
        let want = if self.safe_mode {
            Some(BannerKind::CrashLoop)
        } else if self.auto_restart_disabled {
            Some(BannerKind::RestartsPaused)
        } else if !self.core_alive() && any_attached {
            // the active core is gone (killed, or died and not yet
            // respawned): shown while the gap lasts
            Some(BannerKind::Recovering)
        } else {
            None
        };
        let scale = ctx.scale();
        let want = want.map(|kind| banner::Shown { kind, scale });
        if want == self.banner_kind {
            return;
        }
        for w in self.windows.iter_mut() {
            let Some(l) = w.layers.as_mut() else { continue };
            if let Err(e) = l.set_banner(want.map(|shown| shown.kind), scale) {
                lx_warn!("shell.set_banner_failed", &e.to_string());
                return;
            }
        }
        self.banner_kind = want;
    }

    /// Record a crash event in the rolling window.  Trips
    /// `auto_restart_disabled` if too many have happened recently.
    pub(super) fn record_crash(&mut self) {
        let now = Instant::now();
        supervisor::note_crash(&mut self.crashes, now, CRASH_WINDOW);
        sup_log::log(
            "CRASH",
            &format!("count={}/{}", self.crashes.len(), MAX_CRASHES_IN_WINDOW),
        );
        if self.crashes.len() > MAX_CRASHES_IN_WINDOW {
            self.auto_restart_disabled = true;
            self.disabled_at = Some(now);
            self.budget_trips = self.budget_trips.saturating_add(1);
            lx_event!(
                "BUDGET_EXCEEDED",
                "crash budget exceeded; auto-restart disabled until manual intervention",
                count = self.crashes.len(),
                window_s = CRASH_WINDOW.as_secs()
            );
            sup_log::log(
                "BUDGET_EXCEEDED",
                &format!("count={} window_s={}", self.crashes.len(), CRASH_WINDOW.as_secs()),
            );
        }
    }

    /// Kill the running core (best-effort) and re-spawn from
    /// `current/`.  Used both after a clean detected crash and after
    /// a hang.  Honours `auto_restart_disabled`.
    pub(super) fn restart_core(&mut self, ctx: &MarspotAppCtx) {
        // Tear down whatever's left.
        self.shutdown_active("supervisor restart_core (crash/hang/respawn)");
        if self.auto_restart_disabled {
            return;
        }
        let Some(wi) = self.window_index(ctx.window_id()) else { return };
        if let Some(s) = self.windows[wi].surfaces.as_ref() {
            let w_px = s.width();
            let h_px = s.height();
            let scale = ctx.scale();
            // A fresh core knows nothing about the panes, so the memo of
            // what each pane has already been told has to go with the old
            // one -- see `ShellPluginHost::declared`.
            self.plugin_host.forget_declarations();
            // The new core is started on a pair of its own.  The one on
            // the window is being read by the window server and keeps
            // showing the old core's last frame until the new core's
            // first: no black flash on a live crash, and nothing ever
            // painted into a surface that is on the screen.
            let (f, b) = match self.fresh_pending_pair(wi, w_px, h_px) {
                Ok(ids) => ids,
                Err(e) => {
                    lx_error!("shell.restart_handshake.pair_create_failed", &e);
                    return;
                }
            };
            self.active = self.spawn_core(f, b, w_px, h_px, scale);
            if self.active.is_some() {
                self.record_core_boot();
                self.announce_windows_to_new_core(ctx);
                // A core from before frames were announced as they
                // finish says nothing of the pair it was started on
                // until it is attached; one that does announce them
                // takes the same pair again as no change.
                self.send_surface_attach(
                    ctx.window_id(),
                    f,
                    b,
                    w_px as f64,
                    h_px as f64,
                    scale,
                    ctx.traffic_lights_right_phys(),
                );
            }
        }
    }
}
