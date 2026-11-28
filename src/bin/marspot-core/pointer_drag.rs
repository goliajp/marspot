//! A pointer held down: what it drags, where it is let go, and files dropped from outside.

use super::*;

/// A cell a program was told the pointer is at: window, pane, and
/// column and row from one.
pub(super) type ReportedAt = (usize, usize, u32, u32);

impl CoreApp {
    /// Finder file drop: insert the shell-quoted path(s) into the
    /// pane under the drop point — the "type the path for me" gesture
    /// every macOS terminal supports.  Routing goes through the
    /// existing Paste path (L3 wraps in bracketed-paste when the app
    /// enabled the mode), so shells insert at the prompt and TUI apps
    /// like claudecode see a normal paste in their input box.
    pub(super) fn file_drop(&mut self, wi: usize, x_phys: f64, y_phys: f64, paths: &[String]) {
        // Nothing is typed into a pane from under an open panel.
        if paths.is_empty() || win!(self, wi).panels.owns_pointer() {
            return;
        }
        // Target the pane under the drop point (body cells or its
        // sidebar row); a drop on chrome/padding goes to the focused
        // pane — dropping "at the terminal" should never be a no-op.
        let row = self.chrome_at(wi, x_phys, y_phys).and_then(ChromeTarget::row);
        let sidebar_hit = row.and_then(|r| self.row_slot(wi, r));
        let idx = sidebar_hit
            .or(win!(self, wi).layout.hit_test(x_phys, y_phys))
            .filter(|i| *i < win!(self, wi).panes.len())
            .unwrap_or(win!(self, wi).focused_idx);
        if idx >= win!(self, wi).panes.len() {
            return;
        }
        // Same focus motion as a click — the pane receiving the text
        // becomes the pane the user is typing into next.
        self.set_focus(wi, idx);
        if self.shown_mut(wi, idx).snap_to_live() {
            win!(self, wi).needs_render = true;
        }
        // Trailing space after each path so the user can keep typing
        // (and multiple files arrive space-separated) — matches the
        // Finder → Terminal.app / iTerm2 convention.
        let mut text = String::new();
        for p in paths {
            text.push_str(&marspot_term::input_core::shell_quote_path(p));
            text.push(' ');
        }
        if self.shown(wi, idx).shelld_session_id().is_some_and(|sid| self.model_guard_holds(sid)) {
            return;
        }
        self.shown_mut(wi, idx).session_mut().forward_paste(&text);
        if let Some(sid) = win!(self, wi).panes[idx].shelld_session_id() {
            self.strips.typed(sid);
        }
        win!(self, wi).needs_render = true;
    }

    pub(super) fn mouse_drag(
        &mut self,
        wi: usize,
        x_phys: f64,
        y_phys: f64,
        hover_window_id: u32,
        hover_x: f64,
        hover_y: f64,
    ) {
        if self.scrollbar_drag_to(y_phys) {
            return;
        }
        // RFC-005 step 5 — an armed title press becomes a pane drag
        // once the pointer clears the slop radius.  The activation
        // repaints the title (a ⇢ marker) so the mode is visible.
        if let Some(d) = self.pane_drag.as_mut() {
            if !d.active {
                let (sx, sy) = d.start;
                if (x_phys - sx).hypot(y_phys - sy) > PANE_DRAG_SLOP * marspot::ui::chrome_scale() {
                    d.active = true;
                    let from = d.from_wi;
                    win!(self, from).needs_render = true;
                }
            }
            if self.pane_drag.map(|d| d.active).unwrap_or(false) {
                self.update_drop_target(hover_window_id, hover_x, hover_y);
                return;
            }
        }
        // A card held in the layout panel.
        if let Some(d) = win!(self, wi).layout_drag.as_mut() {
            d.mouse_phys = (x_phys, y_phys);
            win!(self, wi).needs_render = true;
            return;
        }
        let w = &mut win!(self, wi);
        if w.panels.on_drag(x_phys, y_phys, w.w_phys, w.h_phys, w.layout.top_inset) {
            w.needs_render = true;
            return;
        }
        if !win!(self, wi).selection_dragging {
            return;
        }
        let (cw, ch) = self.renderer.cell_dims();
        let target_idx = match win!(self, wi).selection.as_ref() {
            Some(s) => s.session_idx,
            None => return,
        };
        let cell = match win!(self, wi).layout.cells.get(target_idx) {
            Some(c) => *c,
            None => return,
        };
        let layout = &win!(self, wi).layout;
        let at = marspot::layout::cell_grid(&cell, layout.cell_title_h, layout.padding, cw, ch);
        let (raw_col, raw_row) = at.at(x_phys, y_phys);

        // Past-edge auto-scroll, NSTextView-style (see src/main.rs
        // for the rate-limit rationale).
        let max_row = cell.rows.saturating_sub(1) as i64;
        if raw_row < 0 || raw_row > max_row {
            // Same one-step-at-a-time gate as the wheel: a drag past
            // the edge delivers a mouse-moved event per frame of the
            // user's hand, which on a taped pane is far faster than
            // the tape folds — and steps that pile up between two
            // folds are exactly what leaves nothing to align on.
            let may_step = match win!(self, wi).selection_tape.as_ref() {
                Some(t) if t.pane_idx() == target_idx && !t.broken => {
                    let now = std::time::Instant::now();
                    let open = win!(self, wi)
                        .tape_scroll_gate
                        .is_none_or(|until| now >= until);
                    if open {
                        win!(self, wi).tape_scroll_gate =
                            Some(now + std::time::Duration::from_millis(120));
                    }
                    open
                }
                _ => true,
            };
            if may_step {
                let up = raw_row < 0;
                self.shown_mut(wi, target_idx).apply_scroll_lines(if up { 1 } else { -1 });
            }
        }

        let col = raw_col.clamp(0, cell.cols.saturating_sub(1) as i64);
        let floor = self.selection_floor(wi, target_idx, cell.rows.saturating_sub(1));
        let row = raw_row.clamp(0, floor as i64);

        // Re-read view_offset AFTER any auto-scroll above.
        let pane = self.shown(wi, target_idx);
        let rows = pane.session().grid().rows() as u32;
        let vo = pane.view_offset() as u32;
        let abs = vo + rows.saturating_sub(1).saturating_sub(row as u32);

        // On a taped pane the far end is recorded as a virtual line, so
        // the next repaint moves it with the text rather than leaving
        // it on the same row of glass.  `abs` is then whatever that
        // virtual line currently resolves to on screen.
        let abs = match win!(self, wi).selection_tape.as_mut() {
            Some(tape) if tape.pane_idx() == target_idx => {
                tape.set_focus(col as u16, row as u16);
                tape.abs(tape.focus.1, rows as u16)
            }
            _ => abs,
        };
        let Some(sel) = win!(self, wi).selection.as_mut() else { return };
        sel.focus = (col as u16, abs);
        win!(self, wi).needs_render = true;
    }

    pub(super) fn mouse_up(&mut self, wi: usize, drop_window_id: u32, drop_x: f64, drop_y: f64) {
        if self.scrollbar_release() {
            return;
        }
        // a button of the chrome pressed and let go on
        if self.release_chrome(wi, drop_window_id, drop_x, drop_y) {
            return;
        }
        // Release the button the program was told about, at whatever
        // cell the pointer is over now — or, if that is none of the
        // pane's, at the last one it heard of.
        let last = self.mouse_report_last_cell.take();
        self.strips.pointer_at(drop_window_id, drop_x, drop_y);
        if let Some(press @ (at, idx, _, _)) = self.mouse_report_press.take() {
            let here = self
                .cell_pos_at(wi, drop_x, drop_y)
                .filter(|(i, _, _)| (wi, *i) == (at, idx))
                .map(|(_, c, r)| (c as u32 + 1, r as u32 + 1));
            let (_, _, c1, r1) = here.map_or_else(|| last_heard(press, last), |(c, r)| (at, idx, c, r));
            if let Some(pane) = self.shown_pane_mut(at, idx) {
                pane.session_mut().l3_inject_mouse_button(0, false, c1, r1);
            }
            return;
        }
        // RFC-005 step 5 / RFC-006 — resolve an armed title press.
        if let Some(d) = self.pane_drag.take() {
            let target = self.drop_target.take();
            self.clear_drop_preview();
            if d.active {
                let from = d.from_wi;
                win!(self, from).needs_render = true;
                // The SAME resolution the ghost was drawn from — the
                // release delivers exactly what was previewed.
                match self.resolve_drop_outcome(&d, target, drop_window_id) {
                    DropOutcome::Split { to_wi, at_idx, zone } => {
                        self.split_insert(d.from_wi, d.idx, to_wi, at_idx, zone);
                    }
                    DropOutcome::Swap { to_wi, idx } => {
                        self.swap_panes(d.from_wi, d.idx, to_wi, idx);
                    }
                    DropOutcome::Fill { to_wi, idx } => {
                        self.fill_placeholder(d.from_wi, d.idx, to_wi, idx);
                    }
                    DropOutcome::Append { to_wi } => {
                        self.move_pane_to_window(d.from_wi, d.idx, to_wi);
                    }
                    DropOutcome::NewWindow => {
                        self.move_pane_to_new_window_at(
                            d.from_wi,
                            d.idx,
                            (drop_x, drop_y),
                        );
                    }
                    DropOutcome::Nothing => {
                        lx_debug!(
                            "core.pane_drag.cancelled",
                            "release resolves to nothing — as previewed",
                            drop_window_id = drop_window_id
                        );
                    }
                }
                return;
            }
            // Never activated: this was a click, not a drag.  It has
            // already focused the pane on mouse-down, and there is
            // nothing else for a title click to do — a pane's name is
            // its directory, not something to type.
            return;
        }
        // F3+3.3 — finalize LayoutModal card drag: pick the
        // destination slot under the cursor, swap, redraw.  No
        // animation (V2.0); settle = single-frame jump.
        if let Some(d) = win!(self, wi).layout_drag.take() {
            // The slot under the dragged card's centre, which is what
            // the panel highlighted while it was held.
            let modal = self.layout_modal_geometry(wi);
            if let Some(to_slot) = modal.drop_target(d.from_slot, d.mouse_phys, d.grab_offset_phys)
                    && to_slot < win!(self, wi).card_slots.len()
                    && d.from_slot < win!(self, wi).card_slots.len()
                {
                    win!(self, wi).card_slots.swap(d.from_slot, to_slot);
                }
            win!(self, wi).needs_render = true;
            return;
        }
        if let Some(moved) = win!(self, wi).panels.on_release() {
            moved.log();
        }
        // A click without movement leaves anchor == focus → treat as
        // "no selection" so a stray single-click doesn't ghost a
        // single-cell highlight.
        if win!(self, wi).selection_dragging {
            win!(self, wi).selection_dragging = false;
            if let Some(sel) = win!(self, wi).selection
                && sel.anchor == sel.focus {
                    win!(self, wi).selection = None;
                }
        }
    }
}

/// The cell the program in a pane last heard the pointer was at: of the
/// travel reported since the press if any was in its pane, else of the
/// press.
pub(super) fn last_heard(press: ReportedAt, last: Option<ReportedAt>) -> ReportedAt {
    last.filter(|l| (l.0, l.1) == (press.0, press.1)).unwrap_or(press)
}
