//! Process-level chores outside the supervisor tick: starting the redraw
//! pump, drawing the icons the core waits for, and putting marspot away.

use super::*;

impl ShellApp {
    pub(super) fn start_redraw_pump(&mut self) {
        if self.pump.is_none() {
            self.pump = Some(pump::Pump::start(self.proxy.clone()));
        }
    }

    /// Put marspot away.
    ///
    /// **The running sessions are deliberately left alone.**  Quitting
    /// is putting marspot away, not ending the work in it: every L3
    /// keeps its PTY, its shell and whatever is running inside, and
    /// the next launch reattaches to them from the registry.  While
    /// any is kept this process does not exit either: it goes on as a
    /// resident with no window (`resident`), and opening the app
    /// brings this same process back.  That is
    /// what makes "close the windows, come back tomorrow" keep the
    /// sessions — and it has to hold whichever window was closed last,
    /// which is why closing a window parks it rather than retiring it.
    ///
    /// This used to SIGTERM every L3 on the way out (RFC-003 §6
    /// Amendment 15, "user-driven quit = clean account").  The panes
    /// came back — reincarnated from state.bin + bytelog — but as new
    /// shells in the old directory, so anything actually running in
    /// them was gone.
    ///
    /// L3s that outlive their usefulness are still collected: one that
    /// no window claims is retired by the next boot's assembly sweep,
    /// and one whose session dir goes away exits on its own.
    pub(super) fn quit(&mut self, ctx: &MarspotAppCtx) {
        // RFC-001: clean plugin shutdown FIRST, so plugins releasing
        // host resources (notifications, fs watchers) don't race
        // against the rest of the teardown.
        self.plugin_registry.stop_all_with(&self.plugin_host);

        // Spawn races can leave L3s whose entry a later sibling
        // overwrote: nothing will ever reattach to them.
        let (n_kept, orphans_killed) = orphans::stop(&orphans::Install::ours(&self.binaries));
        lx_event!(
            "SHELL_QUIT_CLEANUP",
            "quitting; registered sessions kept running, orphans swept",
            n_kept = n_kept,
            n_orphans_killed = orphans_killed
        );

        // Drop control socket first — gives the core a clean EOF on
        // its read side so it can shut down gracefully before SIGKILL.
        self.shutdown_active("window close_requested");
        // App quit: release every window's surfaces, not just the one
        // whose close button was pressed.
        for w in &mut self.windows {
            if let Some(pair) = w.surfaces.take() {
                pair.release();
            }
            if let Some(stale) = w.pending_surfaces.take() {
                stale.release();
            }
        }
        // The sessions stay in this process's coalition, and it is what
        // answers for them: it stays too, as a resident with no window.
        // The end of the login session is the one quit that is not that.
        if n_kept > 0 && !marspot::app_delegate::quit_ends_the_login_session() {
            resident::stay(n_kept);
        } else {
            // Last thing before the process goes: this is the only
            // moment the bundle's own copy of L1 can be replaced.
            resident::land_staged_bundle();
        }
        ctx.exit();
    }

    /// Have a drawer draw the icons the core is waiting for: the core's
    /// own binary, run once for the batch.
    pub(super) fn draw_core_icons(&mut self) {
        if let Ok(exe) = std::env::current_exe() {
            self.core_icons.draw_waiting(self.binaries.resolve_runnable(&exe), &self.proxy);
        }
    }
}
