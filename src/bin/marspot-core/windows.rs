//! Window lifecycle: adopting live and restored windows, surfaces, focus, close, scale and chrome.

use super::*;

impl CoreApp {
    /// Take on a window the shell has just opened (or re-announced
    /// after a core swap).
    ///
    /// RFC-005: a fresh window starts as a 1×1 grid with one pane.
    /// The pane is spawned off-loop like `[+]` does — a window opening
    /// must not freeze the panes of the windows already up.
    pub(super) fn adopt_window(
        &mut self,
        window_id: u32,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
        slot: Option<usize>,
    ) {
        // RFC-005 step 5 — a parked "Move to New Window" pane claims
        // the window before the restore queue gets a look: the user
        // just asked for this window, a boot leftover did not.
        if let Some(sid) = self.pending_move_sid.take()
            && let Some((from_wi, idx)) = self.find_window_pane_by_sid(sid) {
                // RFC-006 §3 — same dormant-placeholder rule as an
                // in-window move: the source layout holds still.
                let (pc, pr) = {
                    let g = win!(self, from_wi).panes[idx].session().grid();
                    (g.cols(), g.rows())
                };
                let pane = std::mem::replace(
                    &mut win!(self, from_wi).panes[idx],
                    Pane::new_dormant(pc, pr),
                );
                let w = &mut win!(self, from_wi);
                if w.focused_idx == idx {
                    w.focused_idx = (0..w.panes.len())
                        .filter(|&i| !w.panes[i].is_dormant())
                        .min_by_key(|&i| i.abs_diff(idx))
                        .unwrap_or(idx);
                }
                if let Some(sel) = w.selection
                    && sel.session_idx == idx {
                        w.selection = None;
                        w.selection_dragging = false;
                    }
                let mut nw = WindowState::new(
                    window_id, vec![pane], 0, 1, 1, w_phys, h_phys,
                );
                nw.frame_index = self.slot_for_new_window(slot);
                nw.render.mark_bg_clear_required();
                self.windows.push(nw);
                let wi = self.windows.len() - 1;
                self.key_window = wi;
                self.rebuild_layout(from_wi);
                self.rebuild_layout(wi);
                lx_event!(
                    "PANE_MOVED",
                    "pane moved into its own new window",
                    session = sid,
                    to_window = window_id
                );
                self.save_session_state();
                self.request_close_if_empty(from_wi);
                return;
            }
            // The pane closed while the window was opening — fall
            // through and let the fresh-pane path fill the window.
        // RFC-005 step 6b — a queued record means L1 is reopening a
        // window from the last session, not making a new one.  The
        // window comes up with its saved grid and one "starting…"
        // placeholder per saved slot; the assembly worker replaces
        // them with the real panes.
        // Prefer the record L1 named.  Arrival order is only a valid
        // pairing while every saved window is being reopened, which is
        // a cold boot; a core swap re-announces the windows that are
        // already up, so a window merely parked would otherwise hand
        // its record to the next live window that showed up.
        let queued = slot
            .and_then(|s| self.saved_windows.iter().position(|(q, _)| *q == s))
            .and_then(|i| self.saved_windows.remove(i))
            .or_else(|| {
                // No slot on the wire (an older shell), or a slot we
                // hold no record for — the window is new.
                slot.is_none().then(|| self.saved_windows.pop_front()).flatten()
            });
        if let Some((slot, record)) = queued {
            self.adopt_restored_window(window_id, slot, record, w_phys, h_phys, scale);
            return;
        }
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let cols = ((w_phys / cell_w) as u16).max(INITIAL_COLS);
        let rows = ((h_phys / cell_h) as u16).max(INITIAL_ROWS);
        let pane = match allocate_next_session_id() {
            // A new window has no pane to inherit a directory from.
            Ok(id) => spawn_l3_pane_async(cols, rows, id, String::new(), &self.event_tx),
            Err(e) => {
                lx_error!("core.window.session_id_failed", &format!("{e}"));
                return;
            }
        };
        let mut w = WindowState::new(
            window_id,
            vec![pane],
            0,
            1,
            1,
            w_phys,
            h_phys,
        );
        w.frame_index = self.slot_for_new_window(slot);
        // A brand-new window has never been painted.
        w.render.mark_bg_clear_required();
        self.windows.push(w);
        let wi = self.windows.len() - 1;
        // A freshly-opened window takes keyboard focus — that is a
        // statement about focus, not about rank: every other window
        // keeps pumping, painting and persisting exactly as before.
        self.key_window = wi;
        self.rebuild_layout(wi);
        lx_event!(
            "WINDOW_ADOPTED",
            "core took on a new window",
            window_id = window_id,
            windows = self.windows.len()
        );
        self.save_session_state();
    }

    /// RFC-005 step 6b — bring up a window from its saved record.
    ///
    /// Placeholders first, panes later: the window is on screen with
    /// its own grid before any reattach has been attempted, so the
    /// windows already up never stall on this one's I/O.
    pub(super) fn adopt_restored_window(
        &mut self,
        window_id: u32,
        slot: usize,
        record: marspot::state::SavedWindowLayout,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
    ) {
        // Clamp to the *shared* bound, not a copy of it.  A literal
        // `6` here outlived the day the picker went to 9: a restored
        // window came back squeezed into 6 columns, and because the
        // save side was fine, the file said 7 while the screen said 6.
        let grid_cols = (record.grid_cols as usize).clamp(GRID_MIN, GRID_MAX);
        let grid_rows = (record.grid_rows as usize).clamp(GRID_MIN, GRID_MAX);
        // Cell dims for this window's own grid — a restored 3×3 window
        // must hand its L3s the size they will actually be shown at,
        // exactly as the boot path does.
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let probe = Layout::build(
            w_phys,
            h_phys,
            0.0, // sidebar starts collapsed
            HEADER_PT * scale,
            CELL_TITLE_PT * scale,
            grid_cols,
            grid_rows,
            cell_w,
            cell_h,
        );
        let (cols, rows) = (probe.cells[0].cols, probe.cells[0].rows);
        let placeholders: Vec<Pane> = record
            .panes
            .iter()
            .map(|p| Pane::new_pending(p.sid, cols, rows))
            .collect();
        // A record with no panes would leave an empty window, which
        // several paths index into; one placeholder is the floor.
        let placeholders = if placeholders.is_empty() {
            vec![Pane::new_pending(0, cols, rows)]
        } else {
            placeholders
        };
        let focused_idx = (record.focused_idx as usize).min(placeholders.len() - 1);
        let mut w = WindowState::new(
            window_id,
            placeholders,
            focused_idx,
            grid_cols,
            grid_rows,
            w_phys,
            h_phys,
        );
        w.frame_index = slot;
        w.render.mark_bg_clear_required();
        self.windows.push(w);
        let wi = self.windows.len() - 1;
        self.key_window = wi;
        self.rebuild_layout(wi);
        assemble_restore_window_async(window_id, record, cols, rows, &self.event_tx);
        lx_event!(
            "WINDOW_RESTORING",
            "saved window reopened; panes assembling off-loop",
            window_id = window_id,
            slots = win!(self, wi).panes.len(),
            windows = self.windows.len()
        );
    }

    /// A restore worker landed.  Swap the placeholders for the real
    /// panes; a window closed in the meantime drops them.
    pub(super) fn adopt_restored_panes(&mut self, window_id: u32, panes: Vec<Pane>) {
        let Some(wi) = self.window_index(window_id) else {
            lx_warn!(
                "core.window.restore_window_gone",
                "window closed while its panes were assembling; dropping them",
                window_id = window_id,
                panes = panes.len()
            );
            return;
        };
        if panes.is_empty() {
            lx_warn!(
                "core.window.restore_empty",
                "assembly produced no panes; window keeps its placeholders",
                window_id = window_id
            );
            return;
        }
        let n = panes.len();
        win!(self, wi).panes = panes;
        let asked = marspot::session_signal::ask_all_to_update(win!(self, wi).panes.iter_mut());
        win!(self, wi).focused_idx = win!(self, wi).focused_idx.min(n - 1);
        // Placeholder-era selection referred to panes that no longer
        // exist.
        win!(self, wi).selection = None;
        win!(self, wi).selection_dragging = false;
        self.rebuild_layout(wi);
        self.save_session_state();
        lx_event!(
            "WINDOW_RESTORED",
            "assembled panes replaced the placeholders",
            window_id = window_id,
            panes = n,
            asked_to_update = asked
        );
    }

    /// `(window index, pane index)` of the pane holding this session.
    ///
    /// Sessions are window-agnostic: a session id says nothing about
    /// which window its pane currently lives in, and an off-loop
    /// result (spawn finished, control reconnected, search hits) has
    /// to find it wherever it is.  Searching only the key window meant
    /// those results were silently dropped whenever the user had
    /// focused a different window in the meantime.  Windows only; `pane_by_sid_mut` also finds shadows.
    pub(super) fn find_window_pane_by_sid(&self, sid: u64) -> Option<(usize, usize)> {
        self.windows.iter().enumerate().find_map(|(wi, w)| {
            w.panes
                .iter()
                .position(|p| p.shelld_session_id() == Some(sid))
                .map(|pi| (wi, pi))
        })
    }

    /// Total panes across every window — what the diagnostics mean by
    /// "how big is this session", now that panes live in more than one
    /// window.
    pub(super) fn total_panes(&self) -> usize {
        self.windows.iter().map(|w| w.panes.len()).sum()
    }

    /// Repaint the window whose pane carries this session id.
    ///
    /// Plugin-driven state (badges, titles, PaneSession caps) is keyed
    /// by session, and a session says nothing about which window shows
    /// it.  Marking the key window instead would paint the badge into
    /// whichever window the user happened to be looking at — and leave
    /// the one that actually changed stale.
    pub(super) fn mark_sid_window_dirty(&mut self, sid: u64) {
        if let Some((wi, _)) = self.find_window_pane_by_sid(sid) {
            win!(self, wi).needs_render = true;
        }
    }

    /// Index of the window carrying `window_id`, if the core has it.
    pub(super) fn window_index(&self, window_id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.window_id == window_id)
    }

    /// L1 handed one window a freshly-created IOSurface pair (resize,
    /// core restart, pending-update spawn, or the window's birth).
    /// Swap that window's paint target, adopt the new dims, and
    /// re-lay it out.  Every other window is untouched.
    ///
    /// Returns the window index when the pair took, `None` when the
    /// window is gone or the surfaces couldn't be looked up — in the
    /// latter case the window keeps the pair it had rather than going
    /// black.
    pub(super) fn attach_window_surfaces(
        &mut self,
        window_id: u32,
        front_id: u32,
        back_id: u32,
        w_phys: f64,
        h_phys: f64,
        scale: f64,
    ) -> Option<usize> {
        let wi = self.window_index(window_id)?;
        // the pair the window already has is no new paint target
        if !win!(self, wi).surfaces.as_ref().is_some_and(|s| s.ids() == (front_id, back_id)) {
            let Some(fresh) = WindowSurfaces::attach(front_id, back_id, self.renderer.device()) else {
                lx_warn!(
                    "core.attach.surface_lookup_nil",
                    "IOSurfaceLookup returned nil; keeping the previous pair",
                    window_id = window_id,
                    front_id = front_id,
                    back_id = back_id
                );
                return None;
            };
            win!(self, wi).surfaces = Some(fresh);
        }
        win!(self, wi).w_phys = w_phys;
        win!(self, wi).h_phys = h_phys;
        // A window that moved to a display of another density reports
        // it here.  Chrome follows from the next layout; the terminal
        // cell has to come out of a rebuilt font cache, because its
        // size is the font's metrics and those were taken at the old
        // density.
        marspot::ui::set_chrome_scale(scale);
        win!(self, wi).scale = marspot::ui::chrome_scale();
        self.adopt_display_scale();
        self.rebuild_layout(wi);
        Some(wi)
    }

    /// Make `window_id` the key window.  Returns false (and changes
    /// nothing) when the core has no such window — a frame for a
    /// window that already closed, which is a normal race, not an
    /// error.
    pub(super) fn focus_window(&mut self, window_id: u32) -> Option<usize> {
        let i = self.window_index(window_id)?;
        if self.key_window != i {
            let was = self.key_window;
            self.key_window = i;
            // The focus ring moves, so BOTH windows owe a frame — the
            // one that gained it and the one that lost it.
            win!(self, was).needs_render = true;
            win!(self, i).needs_render = true;
        }
        Some(i)
    }

    /// Which window a drag update or release belongs to: the one that
    /// took the press.  A drag that leaves its window still steers the
    /// selection it started, and a release delivered after focus moved
    /// still ends that drag rather than poking whatever is key now.
    /// Falls back to the window on the frame when no press is
    /// outstanding (a release with no drag, e.g. after a core swap).
    pub(super) fn drag_target(&self, frame_window: u32) -> Option<usize> {
        self.drag_window
            .and_then(|w| self.window_index(w))
            .or_else(|| self.window_index(frame_window))
    }

    /// A window closed: park its layout (if it still held live panes)
    /// and drop its `WindowState`.
    ///
    /// Closing a window does NOT retire its sessions.  The window is
    /// being put away, so its L3s keep running, its record keeps being
    /// saved, and the next launch reattaches them — which is what
    /// makes "close the windows, quit, come back" lossless regardless
    /// of the order the windows were closed in.  Discarding a window
    /// for good is what closing its *panes* does.
    ///
    /// A window with nothing but dormant placeholders left (its last
    /// live pane was moved out, or its last pane was closed) has
    /// nothing worth restoring and is not parked.
    ///
    /// The last window is left alone — L1 owns app teardown, and
    /// tearing the state down here first would race it.
    pub(super) fn close_window(&mut self, window_id: u32) {
        let Some(i) = self.window_index(window_id) else { return };
        // Window indices shift below; any in-flight drag is stale.
        self.pane_drag = None;
        self.scrollbar_drag = None;
        if self.windows.len() <= 1 {
            lx_event!(
                "WINDOW_CLOSE_LAST",
                "last window closed — L1 drives app teardown",
                window_id = window_id
            );
            return;
        }
        let gone = self.windows.remove(i);
        let parked = gone.panes.iter().any(|p| !p.is_dormant());
        if parked {
            let record = self.window_layout_record(&gone);
            self.parked_windows.push((gone.frame_index, record));
        } else {
            // Discarded for good, so its slot goes too — both saved
            // lists are positional, and a hole in one of them would
            // hand every window above it the geometry of its
            // neighbour.  L1 compacts its own list to match.
            self.forget_slot(gone.frame_index);
        }
        self.key_window = self.key_window.min(self.windows.len() - 1);
        lx_event!(
            "WINDOW_CLOSED",
            "window closed",
            window_id = window_id,
            remaining = self.windows.len(),
            parked = parked as u32
        );
        self.save_session_state();
    }

    /// Re-derive everything that was sized for the old display.
    ///
    /// A MacBook Pro and a 4K panel run without HiDPI are two
    /// densities, and a window gets dragged between them — so this is
    /// a normal event, not a corner case.  The font cache holds the
    /// terminal cell, so it is rebuilt first; then every window
    /// relayouts, which resizes its panes, which is what tells each
    /// PTY its new size.  No-op when the scale did not actually move,
    /// which is the common case (this runs on every attach).
    pub(super) fn adopt_display_scale(&mut self) {
        if !self.renderer.rebuild_fonts_if_scale_changed() {
            return;
        }
        let (cell_w, cell_h) = self.renderer.cell_dims();
        lx_event!(
            "DISPLAY_SCALE_ADOPTED",
            "fonts rebuilt for a new display density; grids reflow",
            chrome_scale = format!("{:.2}", marspot::ui::chrome_scale()),
            cell_w = format!("{cell_w:.2}"),
            cell_h = format!("{cell_h:.2}"),
            windows = self.windows.len() as u64
        );
        for wi in 0..self.windows.len() {
            self.rebuild_layout(wi);
            win!(self, wi).needs_render = true;
        }
    }

    /// Adopt a new window-button cluster edge and relay it out.
    ///
    /// Shared by the attach path and the chrome-only frame so the two
    /// cannot disagree about what a measurement means.
    pub(super) fn apply_window_chrome(&mut self, window_id: u32, right_phys: f64) {
        let Some(wi) = self.window_index(window_id) else { return };
        let changed = win!(self, wi)
            .lights_right_phys
            .map(|old| (old - right_phys).abs() > 0.5)
            .unwrap_or(true);
        lx_event!(
            "TRAFFIC_LIGHTS",
            "window-button cluster edge as measured by the shell",
            window_id = window_id as u64,
            right_phys = format!("{right_phys:.1}"),
            was = win!(self, wi)
                .lights_right_phys
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "unmeasured".into()),
            changed = changed as u32
        );
        if changed {
            win!(self, wi).lights_right_phys = Some(right_phys);
            self.rebuild_layout(wi);
            win!(self, wi).needs_render = true;
        }
    }
}
