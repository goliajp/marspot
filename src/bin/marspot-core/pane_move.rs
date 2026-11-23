//! Moving panes: between windows, into a new window, and the drag-and-drop targets that decide where.

use super::*;

impl CoreApp {
    /// Move one pane between windows — RFC-005 step 5's whole
    /// mechanism: a `Vec` move.  Everything per-pane travels inside
    /// the `Pane` value (pinned by
    /// `moving_a_pane_between_windows_carries_its_state`); the L3
    /// never notices.  Source window state that referenced the pane
    /// by index (selection, title edit, focus) is repaired; the
    /// target window appends, focuses it, and becomes key.
    pub(super) fn move_pane_to_window(&mut self, from_wi: usize, idx: usize, to_wi: usize) {
        if from_wi == to_wi
            || from_wi >= self.windows.len()
            || to_wi >= self.windows.len()
            || idx >= win!(self, from_wi).panes.len()
        {
            return;
        }
        let pane = self.take_out_of_window(from_wi, idx);
        // Landing: appended (sidebar overflows if the grid is full),
        // focused, and the target becomes the key window.
        let t = &mut win!(self, to_wi);
        t.panes.push(pane);
        t.focused_idx = t.panes.len() - 1;
        self.key_window = to_wi;
        self.rebuild_layout(from_wi);
        self.rebuild_layout(to_wi);
        lx_event!(
            "PANE_MOVED",
            "pane moved between windows",
            from_window = win!(self, from_wi).window_id,
            to_window = win!(self, to_wi).window_id
        );
        self.save_session_state();
        // An empty window closes itself — L1 owns windows, so ask.
        self.request_close_if_empty(from_wi);
    }

    /// Move a pane into a window that does not exist yet.  The pane is
    /// parked by sid; L1 opens a fresh window (user action — bypasses
    /// the restore gates) and the `SurfaceAttachWindow` that follows
    /// lands in `adopt_window`, which sees the parked sid and builds
    /// the window around the MOVED pane instead of spawning one.
    pub(super) fn move_pane_to_new_window(&mut self, from_wi: usize, idx: usize) {
        let Some(sid) = win!(self, from_wi)
            .panes
            .get(idx)
            .and_then(|p| p.shelld_session_id())
        else {
            return;
        };
        self.pending_move_sid = Some(sid);
        self.pending_to_shell.push((
            MsgType::WindowOpenRequest,
            marspot::shell_proto::encode_window_open_request(
                marspot::shell_proto::WINDOW_OPEN_USER,
            ),
        ));
        lx_event!(
            "PANE_MOVE_NEW_WINDOW_REQUESTED",
            "asked L1 for a fresh window to move a pane into",
            session = sid
        );
    }

    /// RFC-006 — resolve what a release at `target` would do for the
    /// pane being dragged.  Pure decision logic; both the ghost and
    /// the release go through it (single source of truth — a `Nothing`
    /// previews nothing and does nothing).
    pub(super) fn resolve_drop_outcome(
        &self,
        drag: &PaneDrag,
        target: Option<DropTarget>,
        drop_window_id: u32,
    ) -> DropOutcome {
        let Some(t) = target else {
            // No pane under the pointer.  In a foreign window that is
            // the append landing; in the own window it is a no-op; in
            // no window at all it births one.
            return match self.window_index(drop_window_id) {
                Some(to_wi) if to_wi != drag.from_wi => DropOutcome::Append { to_wi },
                Some(_) => DropOutcome::Nothing,
                None if drop_window_id == 0 => DropOutcome::NewWindow,
                None => DropOutcome::Nothing,
            };
        };
        let same_window = t.wi == drag.from_wi;
        let hovered_dormant = win!(self, t.wi)
            .panes
            .get(t.pane_idx)
            .is_some_and(|p| p.is_dormant());
        // A placeholder is an empty slot asking to be filled — every
        // zone of it means "put the pane HERE" (splitting beside
        // emptiness would be pedantry).
        if hovered_dormant {
            if same_window && t.pane_idx == drag.idx {
                return DropOutcome::Nothing; // cannot happen (dormant isn't draggable), but stay total
            }
            return DropOutcome::Fill { to_wi: t.wi, idx: t.pane_idx };
        }
        match t.zone {
            DropZone::Center => {
                if same_window && t.pane_idx == drag.idx {
                    DropOutcome::Nothing // swapping with yourself
                } else {
                    DropOutcome::Swap { to_wi: t.wi, idx: t.pane_idx }
                }
            }
            zone => {
                if same_window {
                    // Simulate the remove+insert: if the pane would
                    // come back to its own slot, the release changes
                    // nothing — own edges, AND the near edges of the
                    // neighbours (pane i dropped on i+1's Left band
                    // re-inserts at i).
                    let at = if drag.idx < t.pane_idx {
                        t.pane_idx - 1
                    } else {
                        t.pane_idx
                    };
                    let cols = win!(self, t.wi).grid_cols;
                    let n_after_remove = win!(self, t.wi).panes.len() - 1;
                    let insert_at = match zone {
                        DropZone::Left | DropZone::Top => at,
                        DropZone::Right => at + 1,
                        DropZone::Bottom => {
                            let r = at / cols;
                            let c = at % cols;
                            ((r + 1) * cols + c).min(n_after_remove)
                        }
                        DropZone::Center => unreachable!(),
                    };
                    if insert_at == drag.idx {
                        return DropOutcome::Nothing;
                    }
                }
                // At both grid caps a split cannot reshape — the drop
                // is an append, and previewing a half-pane split there
                // would promise a shape the release can't deliver.
                let t_win = &win!(self, t.wi);
                let full = t_win.panes.len() + 1 > t_win.grid_cols * t_win.grid_rows;
                let can_reshape = match zone {
                    DropZone::Left | DropZone::Right => t_win.grid_cols < 6,
                    DropZone::Top | DropZone::Bottom => t_win.grid_rows < 6,
                    DropZone::Center => false,
                };
                if !same_window && full && !can_reshape {
                    return DropOutcome::Append { to_wi: t.wi };
                }
                DropOutcome::Split { to_wi: t.wi, at_idx: t.pane_idx, zone }
            }
        }
    }

    /// RFC-006 — recompute the live drop target from this drag tick's
    /// hover.  The ghost is rendered FROM the resolved outcome, so
    /// what lights up is exactly what release will do — a `Nothing`
    /// (own slot, no-op reinsert) lights nothing.
    pub(super) fn update_drop_target(&mut self, hover_window_id: u32, hx: f64, hy: f64) {
        let new_target = self.window_index(hover_window_id).and_then(|wi| {
            let idx = win!(self, wi).layout.hit_test(hx, hy)?;
            if idx >= win!(self, wi).panes.len() {
                return None;
            }
            let rect = win!(self, wi).layout.cells.get(idx)?;
            let zone = drop_zone_at(rect, hx, hy);
            Some(DropTarget { wi, pane_idx: idx, zone })
        });
        if new_target == self.drop_target {
            return;
        }
        self.clear_drop_preview();
        self.drop_target = new_target;
        let Some(drag) = self.pane_drag else { return };
        let ghost = match self.resolve_drop_outcome(&drag, new_target, hover_window_id) {
            DropOutcome::Split { to_wi, at_idx, zone } => {
                let rect = win!(self, to_wi).layout.cells[at_idx];
                Some((to_wi, drop_preview_rect(&rect, zone), false))
            }
            DropOutcome::Swap { to_wi, idx } | DropOutcome::Fill { to_wi, idx } => {
                let rect = win!(self, to_wi).layout.cells[idx];
                Some((to_wi, drop_preview_rect(&rect, DropZone::Center), false))
            }
            // Append: no particular slot to promise — frame the whole
            // content area ("into this window"), outline only, so the
            // downgrade is visible BEFORE release (RFC-006 §2).
            DropOutcome::Append { to_wi } => {
                let l = &win!(self, to_wi).layout;
                let x = l.sidebar_w;
                let y = l.top_inset;
                let rect = (x, y, (l.window_w - x).max(0.0), (l.window_h - y).max(0.0));
                Some((to_wi, rect, true))
            }
            // NewWindow / Nothing: no rectangle to promise.
            _ => None,
        };
        if let Some((wi, rect, outline)) = ghost {
            win!(self, wi).drop_preview = Some((rect, outline));
            win!(self, wi).needs_render = true;
        }
    }

    pub(super) fn clear_drop_preview(&mut self) {
        for w in self.windows.iter_mut() {
            if w.drop_preview.take().is_some() {
                w.needs_render = true;
            }
        }
    }

    /// RFC-006 §2 — split placement: insert the dragged pane beside
    /// the hovered pane, reshaping the grid when it is full.  Within
    /// one window this is a REARRANGE (remove + insert, no
    /// placeholder); across windows the source keeps its layout via
    /// the dormant placeholder, same as every move-out.
    pub(super) fn split_insert(
        &mut self,
        from_wi: usize,
        from_idx: usize,
        to_wi: usize,
        at_idx: usize,
        zone: DropZone,
    ) {
        if from_wi >= self.windows.len()
            || to_wi >= self.windows.len()
            || from_idx >= win!(self, from_wi).panes.len()
            || at_idx >= win!(self, to_wi).panes.len()
        {
            return;
        }
        let same_window = from_wi == to_wi;
        if same_window && from_idx == at_idx {
            return; // splitting beside yourself is where you already are
        }
        // Take the pane out.
        let (pane, at_idx) = if same_window {
            let p = win!(self, from_wi).panes.remove(from_idx);
            let at = if from_idx < at_idx { at_idx - 1 } else { at_idx };
            (p, at)
        } else {
            let (pc, pr) = {
                let g = win!(self, from_wi).panes[from_idx].session().grid();
                (g.cols(), g.rows())
            };
            let p = std::mem::replace(
                &mut win!(self, from_wi).panes[from_idx],
                Pane::new_dormant(pc, pr),
            );
            let w = &mut win!(self, from_wi);
            if w.focused_idx == from_idx {
                w.focused_idx = (0..w.panes.len())
                    .filter(|&i| !w.panes[i].is_dormant())
                    .min_by_key(|&i| i.abs_diff(from_idx))
                    .unwrap_or(from_idx);
            }
            if let Some(sel) = w.selection
                && sel.session_idx == from_idx {
                    w.selection = None;
                    w.selection_dragging = false;
                }
            (p, at_idx)
        };
        // Reshape the target grid if it cannot absorb one more pane.
        // Horizontal zones grow a column, vertical ones a row; both at
        // the 6-cap → the drop downgrades to append (previewed as the
        // whole-pane rect, and the sidebar overflow catches it).
        let t = &mut win!(self, to_wi);
        let full = t.panes.len() + 1 > t.grid_cols * t.grid_rows;
        if full {
            match zone {
                DropZone::Left | DropZone::Right if t.grid_cols < 6 => t.grid_cols += 1,
                DropZone::Top | DropZone::Bottom if t.grid_rows < 6 => t.grid_rows += 1,
                _ => {}
            }
        }
        // Landing slot, row-major (RFC-006: exact for the flagship
        // 1×1 cases, predictable in general — the preview showed it).
        let cols = t.grid_cols;
        let insert_at = match zone {
            DropZone::Left | DropZone::Top | DropZone::Center => at_idx,
            DropZone::Right => at_idx + 1,
            DropZone::Bottom => {
                let r = at_idx / cols;
                let c = at_idx % cols;
                ((r + 1) * cols + c).min(t.panes.len())
            }
        };
        let insert_at = insert_at.min(t.panes.len());
        t.panes.insert(insert_at, pane);
        t.focused_idx = insert_at;
        self.key_window = to_wi;
        if !same_window {
            self.rebuild_layout(from_wi);
        }
        self.rebuild_layout(to_wi);
        lx_event!(
            "PANE_SPLIT_IN",
            "pane dropped into a split slot",
            to_window = win!(self, to_wi).window_id,
            slot = insert_at,
            zone = format!("{zone:?}")
        );
        self.save_session_state();
        if !same_window {
            self.request_close_if_empty(from_wi);
        }
    }

    /// RFC-006 §1 — center-zone drop: the dragged pane and the
    /// hovered pane trade slots.  Nothing reshapes, nothing spawns,
    /// no placeholder — a swap is symmetric.
    pub(super) fn swap_panes(&mut self, wa: usize, ia: usize, wb: usize, ib: usize) {
        if wa >= self.windows.len()
            || wb >= self.windows.len()
            || ia >= win!(self, wa).panes.len()
            || ib >= win!(self, wb).panes.len()
        {
            return;
        }
        if wa == wb {
            if ia == ib {
                return;
            }
            win!(self, wa).panes.swap(ia, ib);
            win!(self, wa).focused_idx = ib;
        } else {
            // Two disjoint &mut windows via split_at_mut.
            let (lo, hi, li, hj) = if wa < wb {
                (wa, wb, ia, ib)
            } else {
                (wb, wa, ib, ia)
            };
            let (left, right) = self.windows.split_at_mut(hi);
            std::mem::swap(&mut left[lo].panes[li], &mut right[0].panes[hj]);
            win!(self, wb).focused_idx = ib;
        }
        // Selections referenced content that just teleported.
        win!(self, wa).selection = None;
        win!(self, wa).selection_dragging = false;
        win!(self, wb).selection = None;
        win!(self, wb).selection_dragging = false;
        self.key_window = wb;
        self.rebuild_layout(wa);
        if wa != wb {
            self.rebuild_layout(wb);
        }
        lx_event!(
            "PANE_SWAPPED",
            "panes traded slots",
            a_window = win!(self, wa).window_id,
            b_window = win!(self, wb).window_id
        );
        self.save_session_state();
    }

    /// RFC-006 — drop onto a dormant placeholder: the pane takes the
    /// empty slot (any zone of a placeholder means "put it HERE").
    /// Source side follows the move-out rules; the placeholder is
    /// consumed, so no reshape and no new slot.
    pub(super) fn fill_placeholder(&mut self, from_wi: usize, from_idx: usize, to_wi: usize, at_idx: usize) {
        if from_wi >= self.windows.len()
            || to_wi >= self.windows.len()
            || from_idx >= win!(self, from_wi).panes.len()
            || at_idx >= win!(self, to_wi).panes.len()
            || !win!(self, to_wi).panes[at_idx].is_dormant()
        {
            return;
        }
        let same_window = from_wi == to_wi;
        // Either way the vacated slot goes dormant — a fill is a
        // slot-to-slot move, and the shape of the source layout is
        // preserved exactly like every other move-out.
        let (pc, pr) = {
            let g = win!(self, from_wi).panes[from_idx].session().grid();
            (g.cols(), g.rows())
        };
        let pane = std::mem::replace(
            &mut win!(self, from_wi).panes[from_idx],
            Pane::new_dormant(pc, pr),
        );
        {
            let w = &mut win!(self, from_wi);
            if w.focused_idx == from_idx {
                w.focused_idx = (0..w.panes.len())
                    .filter(|&i| !w.panes[i].is_dormant())
                    .min_by_key(|&i| i.abs_diff(from_idx))
                    .unwrap_or(from_idx);
            }
            if let Some(sel) = w.selection
                && sel.session_idx == from_idx {
                    w.selection = None;
                    w.selection_dragging = false;
                }
        }
        win!(self, to_wi).panes[at_idx] = pane;
        win!(self, to_wi).focused_idx = at_idx;
        self.key_window = to_wi;
        if !same_window {
            self.rebuild_layout(from_wi);
        }
        self.rebuild_layout(to_wi);
        lx_event!(
            "PANE_FILLED_PLACEHOLDER",
            "pane dropped into a dormant slot",
            to_window = win!(self, to_wi).window_id,
            slot = at_idx
        );
        self.save_session_state();
        if !same_window {
            self.request_close_if_empty(from_wi);
        }
    }

    /// RFC-006 §4 — drag-to-desktop: park the pane and ask L1 for a
    /// window centred on the release point (screen pts).
    pub(super) fn move_pane_to_new_window_at(&mut self, from_wi: usize, idx: usize, at: (f64, f64)) {
        let Some(sid) = win!(self, from_wi)
            .panes
            .get(idx)
            .and_then(|p| p.shelld_session_id())
        else {
            return;
        };
        self.pending_move_sid = Some(sid);
        self.pending_to_shell.push((
            MsgType::WindowOpenRequest,
            marspot::shell_proto::encode_window_open_request_at(
                marspot::shell_proto::WINDOW_OPEN_USER,
                at.0,
                at.1,
            ),
        ));
        lx_event!(
            "PANE_MOVE_NEW_WINDOW_REQUESTED",
            "drag-out: asked L1 for a window at the release point",
            session = sid
        );
    }

    /// Post-move bookkeeping — RFC-006 liveness rule: a window's
    /// population is its NON-DORMANT panes, and a window whose last
    /// live pane left asks L1 to close it.  Placeholders alone keep
    /// nothing alive (they are layout, and their layout dies with the
    /// window); this is also what makes "drag the sole pane of a 1×1
    /// window away" read as the window following its pane.
    pub(super) fn request_close_if_empty(&mut self, wi: usize) {
        // Same rule, same path as a window emptied by closing its
        // panes: nothing live left ⇒ the window goes.  (A move-out
        // cannot empty the *last* window — the destination window
        // exists by the time this runs — so the quit branch inside is
        // unreachable from here.)
        self.close_window_if_emptied(wi);
    }
}
