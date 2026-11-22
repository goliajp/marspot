//! Pane session lifecycle: spawning, the per-session silent update, retiring and closing.

use super::*;

impl CoreApp {
    pub(super) fn spawn_session(&mut self, wi: usize) {
        if win!(self, wi).panes.len() >= SESSION_COUNT_HARD_CAP {
            return;
        }
        let (cols, rows) = win!(self, wi)
            .layout
            .cells
            .get(win!(self, wi).panes.len())
            .or_else(|| win!(self, wi).layout.cells.first())
            .map(|c| (c.cols, c.rows))
            .unwrap_or((INITIAL_COLS, INITIAL_ROWS));
        // RFC-003 step 3a: L2 allocates the session id itself via
        // the on-disk registry (sessions/.next_id with flock), no
        // L4 round-trip.
        // The spawn itself runs off-loop; this only allocates the
        // id (a flock + a directory scan) and pushes a "starting…"
        // slot.  Clicking [+] used to freeze every pane for as long
        // as the new L3 took to register and handshake.
        let from = win!(self, wi)
            .try_focused_pane()
            .and_then(|p| p.shelld_session_id())
            .map(live_cwd_of)
            .unwrap_or_default();
        match allocate_next_session_id().map(|id| {
            spawn_l3_pane_async(cols, rows, id, from, &self.event_tx)
        }) {
            Ok(pane) => {
                let new_sid = pane.shelld_session_id();
                win!(self, wi).panes.push(pane);
                // One-shot resolve so the title strip lands
                // populated on the new pane's first paint instead
                // of showing its ordinal until the next sweep.
                // shell_child_pid may not be written yet on this
                // very tick — then this is a no-op and the sweep
                // picks the pane up within CWD_SWEEP_INTERVAL.
                if let Some(sid) = new_sid {
                    self.resolve_pane_cwd(sid);
                }
                self.save_session_state();
            }
            Err(e) => lx_error!("core.spawn.l3_failed", &format!("{e}")),
        }
    }

    /// Per-session silent update: promote the staged `marspot-session`
    /// into `current/`, then ask every live L3 pane to execv itself into
    /// it.  Skips panes that have exited.
    pub(super) fn swap_idle_l3(&mut self) {
        // Each L3 execs itself into the new image, keeping its pid, PTY
        // and listener; L2 only asks (see `session_signal::ask_to_update`).
        match marspot::updater::promote_pending_session() {
            Ok(true) => lx_event!(
                "SESSION_PROMOTE",
                "promoted staged marspot-session → current/ for swap"
            ),
            Ok(false) => {}
            Err(e) => lx_error!("core.promote.swap_failed", &format!("{e}")),
        }
        let asked = self.ask_every_l3_to_update();
        lx_event!(
            "L3_SWAP_FANOUT",
            "asked every L3 not on current/marspot-session to move to it",
            n_asked = asked
        );
    }

    /// Ask every live L3, drawn or not, to move to the staged build.
    /// Returns how many were asked.
    pub(super) fn ask_every_l3_to_update(&mut self) -> usize {
        marspot::session_signal::ask_all_to_update(self.panes_mut())
    }

    /// Terminate `panes[idx]` and keep all parallel state in sync.
    /// Caller refuses the call when it would leave zero sessions.
    /// Retire the session behind a pane: signal its L3, drop the
    /// registry dir + shm region, and forget the sid-keyed caches.
    ///
    /// Takes the id and kind by value rather than a `&Pane` so the
    /// caller does not hold a borrow of `windows` across the call —
    /// and so closing one pane and closing a whole window can share
    /// the teardown while keeping their own bookkeeping.
    ///
    /// RFC-003 step 3c: L3 panes own their PTY in-process, so the
    /// close is SIGHUP via entry.toml's pid + delete the registry dir
    /// (which also wipes the bytelog, so the slot cannot replay if a
    /// later L3 picks the same id).
    pub(super) fn retire_pane_session(&mut self, id: u64, is_l3: bool) {
        if is_l3 {
            // SIGHUP is a close and nothing else: SIGTERM also asks an
            // L3 to update, and with another build staged it would
            // re-exec instead of ending
            let _ = marspot::session_signal::signal_session(id, libc::SIGHUP);
            // the names it published under, so the kernel frees the
            // pages once every holder has closed them
            let regions = session_registry::read_session_entry(id)
                .ok()
                .and_then(|e| grid_shm::SessionRegions::of_entry(&e));
            let _ = session_registry::delete_session(id);
            if let Some(regions) = regions {
                regions.delete();
            }
        }
        // F3+5 — drop cached cwd state so it can't leak past the pane.
        // The same id may eventually be reused; a fresh pane gets a
        // fresh resolve.
        self.pane_cwds.remove(&id);
        self.shell_child_pids.remove(&id);
        self.pane_badges.remove(&id);
        self.pane_titles.remove(&id);
        self.esc_history.remove(&id);
    }

    pub(super) fn close_session(&mut self, wi: usize, idx: usize) {
        // A pane closing invalidates any armed drag's index math.
        self.pane_drag = None;
        self.scrollbar_drag = None;
        if idx >= win!(self, wi).panes.len() {
            return;
        }
        if let Some(id) = win!(self, wi).panes[idx].shelld_session_id() {
            let is_l3 = win!(self, wi).panes[idx].is_l3();
            self.retire_pane_session(id, is_l3);
        }
        let (pc, pr) = {
            let g = win!(self, wi).panes[idx].session().grid();
            (g.cols(), g.rows())
        };
        win!(self, wi).panes.remove(idx);
        if win!(self, wi).panes.is_empty() {
            // Closing the window is L1's call and arrives a few frames
            // from now (`close_window_if_emptied`).  Until then the
            // window is still on screen and still being rendered, and
            // every pane lookup here indexes rather than `get`s — so
            // leave the same dormant placeholder a moved-out pane
            // leaves.  It renders as an empty slot, it is not parked,
            // and it is not restored.
            win!(self, wi).panes.push(Pane::new_dormant(pc, pr));
        }
        if !win!(self, wi).panes.is_empty() {
            if win!(self, wi).focused_idx == idx {
                win!(self, wi).focused_idx = idx.min(win!(self, wi).panes.len() - 1);
            } else if win!(self, wi).focused_idx > idx {
                win!(self, wi).focused_idx -= 1;
            }
        } else {
            win!(self, wi).focused_idx = 0;
        }
        if let Some(sel) = win!(self, wi).selection {
            if sel.session_idx == idx {
                win!(self, wi).selection = None;
                win!(self, wi).selection_dragging = false;
            } else if sel.session_idx > idx {
                win!(self, wi).selection = Some(Selection {
                    session_idx: sel.session_idx - 1,
                    ..sel
                });
            }
        }
        self.save_session_state();
        self.close_window_if_emptied(wi);
    }

    /// A window whose last pane the user just closed goes away with it
    /// — and if it was the last window, so does marspot.
    ///
    /// Closing panes is the deliberate, destructive gesture (closing
    /// the *window* parks it instead), so nothing here is remembered:
    /// the window is not parked, and emptying the last window wipes
    /// the saved layout so the next launch opens a fresh default
    /// window rather than resurrecting what was just dismantled.
    ///
    /// L1 owns windows and app teardown, so both cases are a request.
    pub(super) fn close_window_if_emptied(&mut self, wi: usize) {
        if wi >= self.windows.len() {
            return;
        }
        if win!(self, wi).panes.iter().any(|p| !p.is_dormant()) {
            return;
        }
        let window_id = win!(self, wi).window_id;
        if self.only_window_left(wi) {
            // The app is going away.  Drop the layout first: L1 tears
            // the core down moments after the request lands, and a
            // save racing that would restore an empty window.
            self.layout_discarded = true;
            if let Err(e) = marspot::state::clear() {
                lx_warn!("core.state_file.clear_failed", &format!("{e}"));
            }
            lx_event!(
                "WINDOW_EMPTY_LAST",
                "last pane of the last window closed — asking L1 to quit",
                window_id = window_id
            );
        } else {
            lx_event!(
                "WINDOW_EMPTY",
                "last pane closed — asking L1 to close the window",
                window_id = window_id
            );
        }
        self.pending_to_shell.push((
            MsgType::WindowCloseRequest,
            marspot::shell_proto::encode_window_close_request(window_id),
        ));
    }
}
