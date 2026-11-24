//! The wheel over a window.

use super::*;

impl CoreApp {
    pub(super) fn scroll(&mut self, wi: usize, dy_phys: f64, precise: bool) {
        if self.panel_takes_wheel(wi, dy_phys) {
            return;
        }
        // A plugin-held PaneSession that took the keyboard owns the
        // wheel too.  Keys route to L1 in `key` above; the wheel used
        // to fall straight through to the PTY, and on a mouse-tracking
        // pane `apply_scroll_lines` encodes it as `CSI < 64;x;y M` —
        // so scrolling during a profile cycle typed mouse reports into
        // the shell prompt the cycle had just uncovered, and their echo
        // kept the PTY noisy enough that `await_quiet` could only ever
        // time out (2026-09-01, the `^[[<64;37;32M` screenful).
        //
        // Dropped rather than routed up: a held pane's picture is
        // frozen, so there is no scroll for the user to see either.
        if let Some(active_sid) = self.focused_pane_active_session(wi)
            && self
                .pane_session_for(active_sid)
                .is_some_and(|s| s.has(marspot::shell_proto::PANE_SESSION_CAP_LOCK_KEYS))
            {
                return;
            }
        let (_, cell_h) = self.renderer.cell_dims();
        let lines = scroll_lines(dy_phys, precise, cell_h);
        if lines == 0 {
            return;
        }
        let idx = win!(self, wi).focused_idx;
        if self.strip_takes_wheel(wi, idx) {
            return;
        }
        // A mouse-tracking TUI (claudecode etc.) owns its own scroll: the
        // wheel is injected to the app, which redraws the whole screen in
        // place — marspot only ever sees the new grid and cannot re-anchor
        // an existing selection to it (there is no marspot scrollback for
        // these panes; `scrollback_len` stays 0). So a selection left in
        // place after such a scroll highlights *different* bytes than the
        // user drew it over — the "选区一滚就变" report. Clear it.
        //
        // This is deliberately narrower than "clear on any grid change":
        // a claudecode pane redraws autonomously (spinner) several times a
        // second, and the selection MUST survive that so the user can
        // Cmd-C what they picked. Only a user-initiated scroll of a
        // mouse-tracking pane clears — a real scroll can't keep a valid
        // selection, autonomous output can.
        if self.plugin_takes_wheel(wi, idx, lines) {
            return;
        }
        let tui_scroll = self.shown(wi, idx).session().is_l3()
            && self.shown(wi, idx).session().l3_mouse_tracking_active();
        // A wheel tick during a drag moves OUR viewport — on a pane
        // that has one.  The anchor is an absolute line number, so it
        // stays on the line it was put on while the far end follows
        // the view: the scroll appends, which is what was asked for.
        //
        // A pane whose picture the program owns has no viewport of
        // ours to move (the alt screen files nothing into scrollback),
        // so the tick goes to the program and the `SelectionTape`
        // carries the selection across the repaint instead.
        self.note_wheel_over_selection(wi, idx, tui_scroll);
        if win!(self, wi).selection_dragging
            && win!(self, wi).selection.is_some_and(|s| s.session_idx == idx)
            && !self.shown(wi, idx).program_owns_scroll()
        {
            let moved = self.shown_mut(wi, idx).scroll_own_view(lines);
            if moved != 0 {
                // The far end travels with the viewport; the anchor does
                // not.  Both are absolute line numbers counted back from
                // the newest line, so scrolling into history raises them.
                if let Some(sel) = win!(self, wi).selection.as_mut() {
                    sel.focus.1 = sel.focus.1.saturating_add_signed(moved);
                }
                win!(self, wi).needs_render = true;
            }
            return;
        }
        // While a tape is following this pane, the picture may only
        // move as fast as the tape can read it.
        //
        // Two limits, and both are needed.  ONE STEP AT A TIME: the
        // next tick waits for the tape to have read the last one off a
        // frame (or for the gate's deadline, so a step that moves
        // nothing cannot wedge the pane).  A momentum scroll delivers
        // events far faster than frames are folded, and open-loop
        // forwarding let several land between two folds — their
        // combined distance left nothing to align on, which is what
        // "稍微快一丁点就整个选择区都没有了" was.
        //
        // AND A SHORT STEP: measured on a live pane, a step of 16 rows
        // leaves 39 of the 47 rows that could still match doing so and
        // reads exactly; a jump of 40 rows leaves 9 and reads as
        // nothing.  The momentum stream keeps coming, so a short step
        // costs no reachable distance — only the size of the jump
        // between two frames.
        let taped_here = win!(self, wi)
            .selection_tape
            .as_ref()
            .is_some_and(|t| t.pane_idx() == idx && !t.broken);
        let lines = if taped_here {
            let now = std::time::Instant::now();
            match win!(self, wi).tape_scroll_gate {
                Some(until) if now < until => return,
                _ => {}
            }
            win!(self, wi).tape_scroll_gate =
                Some(now + std::time::Duration::from_millis(120));
            let rows = self.shown(wi, idx).session().grid().rows() as i32;
            let cap = (rows / 8).max(1);
            lines.clamp(-cap, cap)
        } else {
            lines
        };
        if self.shown_mut(wi, idx).apply_scroll_lines(lines) {
            // The program just repainted at a new position.  A tape
            // following this pane reads the shift off the next frame
            // and carries the selection with it; without one there is
            // nothing to re-anchor to, and a box left standing would
            // then cover text the user never picked.
            let taped = win!(self, wi)
                .selection_tape
                .as_ref()
                .is_some_and(|t| t.pane_idx() == idx && !t.broken);
            if tui_scroll && !taped
                && let Some(sel) = win!(self, wi).selection
                    && sel.session_idx == idx {
                        win!(self, wi).selection = None;
                        win!(self, wi).selection_dragging = false;
                    }
            win!(self, wi).needs_render = true;
        }
    }

    /// Whether the open panel took the wheel.
    fn panel_takes_wheel(&mut self, wi: usize, dy_phys: f64) -> bool {
        // While a panel is open the wheel is its own, whether or not
        // it has anywhere to go: nothing scrolls under a panel.
        match win!(self, wi).panels.open() {
            Some(SurfaceKind::Processes) => {
                if let Some(panel) = win!(self, wi).process_panel.as_mut() {
                    // Held to what the frame on screen can scroll.
                    let max = panel.layout.as_ref().map_or(0.0, |l| l.scroll().1);
                    panel.scroll_y = marspot::ui::Units::from_px((panel.scroll_y.px() + dy_phys).clamp(0.0, max));
                    win!(self, wi).needs_render = true;
                }
                true
            }
            Some(SurfaceKind::Settings) => {
                self.settings_scroll(wi, dy_phys);
                true
            }
            Some(SurfaceKind::Helpers) => {
                self.helpers_scroll(wi, dy_phys);
                true
            }
            Some(SurfaceKind::Layout | SurfaceKind::Accounts | SurfaceKind::Shortcuts) => true,
            None => false,
        }
    }

    /// Whether the keys a plugin declared for pane `idx`'s wheel took the tick.
    fn plugin_takes_wheel(&mut self, wi: usize, idx: usize, lines: i32) -> bool {
        // RFC-008 — a plugin may own this pane's wheel.  Checked before
        // the pane's own routing: the program repaints in place, so
        // there is no scrollback for the terminal to move and no mouse
        // reporting to forward through; only these keys reach it.
        //
        // `enter` goes only when the program's own on-screen marker
        // says the view is NOT open.  Read, never remembered: the
        // program leaves that view on its own as well as by the user's
        // key, and `enter` is typically a toggle — a stale flag would
        // then close the transcript instead of opening it (which is
        // exactly what happened when this was a remembered bool).
        //
        // Nothing here sends a key to LEAVE the view: a stray tick at
        // the bottom would otherwise close what the user was reading.
        //
        // And only an UPWARD tick may OPEN it.  Reaching for history is
        // an upward gesture; a downward one at rest means "I am already
        // at the newest, show me what is below" — answering that by
        // opening a history view is a surprise.  A closed view plus a
        // downward tick is therefore not ours: it falls through to the
        // pane's own routing untouched.
        if let Some(sid) = self.shown(wi, idx).session().shelld_session_id()
            && self.pane_wheel_keys.contains_key(&sid) && lines != 0 {
                let mut buf: Vec<u8> = Vec::new();
                let ticks = lines.unsigned_abs().min(
                    self.shown(wi, idx).session().grid().rows().max(1) as u32,
                );
                // Read the state, do not remember it — see `WheelKeys`.
                let open = {
                    let k = &self.pane_wheel_keys[&sid];
                    // The program's own answer, when it gave one.
                    let alt_scroll =
                        self.shown(wi, idx).session().l3_alt_scroll_active();
                    let grid = self.shown(wi, idx).session().grid();
                    marspot::wheel_marker::view_is_open(
                        alt_scroll,
                        grid.cols(),
                        grid.rows(),
                        |col, row| grid.cell(col, row).ch,
                        &k.marker,
                    )
                };
                let up = lines > 0;
                let since_enter = self
                    .pane_wheel_enter_at
                    .get(&sid)
                    .map(|t| t.elapsed().as_millis() as u64);
                let ask = marspot::wheel_marker::should_send_enter(open, since_enter);
                let mut suppressed = false;
                if marspot::wheel_marker::wheel_is_ours(open, up)
                    && let Some(k) = self.pane_wheel_keys.get(&sid) {
                        let mut opening = false;
                        if ask && !k.enter.is_empty() {
                            buf.extend_from_slice(&k.enter);
                            opening = true;
                        } else if !open && !k.enter.is_empty() {
                            suppressed = true;
                        }
                        // The tick that OPENS the view does not also
                        // travel in it.  A flick is one wheel event
                        // carrying many lines, so sending the enter key
                        // and that whole distance together opened the
                        // history and immediately threw the user tens
                        // of lines into it — landing in the middle of
                        // whatever the program had printed there rather
                        // than at the edge they were reaching for.
                        // Reaching for history is one gesture; moving
                        // inside it is the next one.
                        if !opening {
                            let key = if up { &k.up } else { &k.down };
                            for _ in 0..ticks {
                                buf.extend_from_slice(key);
                            }
                        }
                    }
                if ask && !buf.is_empty() {
                    self.pane_wheel_enter_at.insert(sid, std::time::Instant::now());
                }
                if suppressed {
                    // The thing the state log could not see.  A run of
                    // these is the flick that used to toggle the view
                    // open and shut several times over.
                    lx_debug_sampled!(
                        "core.pane_wheel.enter_held",
                        8,
                        "tick arrived while the last enter was still in flight",
                        session = sid,
                        since_ms = since_enter.unwrap_or(0)
                    );
                }
                // One line per change of state, not per event: a
                // momentum scroll is many events, and the thing worth
                // seeing is whether the view opened and stayed open.
                // Silence here is what made three wrong fixes all look
                // right (2026-09-06).
                if !buf.is_empty() {
                    if self.pane_wheel_open.get(&sid) != Some(&open) {
                        lx_info!(
                            "core.pane_wheel.state",
                            &format!("sid={sid} open={open} up={up} sent_enter={}", !open)
                        );
                        self.pane_wheel_open.insert(sid, open);
                    }
                    self.shown_mut(wi, idx).session_mut().forward_program_keys(&buf);
                    win!(self, wi).needs_render = true;
                    return true;
                }
            }
        false
    }

    fn note_wheel_over_selection(&mut self, wi: usize, idx: usize, tui_scroll: bool) {
        // Dev-cycle only — one line per change of the situation, never
        // per tick (a momentum scroll is hundreds).  The report
        // "选择框不会变动，内容滚动就选了不同的文字" fits none of the
        // three paths below as written: on an alt-screen pane the drag
        // path moves nothing and swallows the tick, and the path after
        // the drag drops the selection outright.  So the branch that
        // really fires has to be read, not reasoned about.
        if let Some(sel) = win!(self, wi).selection {
            let here = format!(
                "sel_pane={} wheel_pane={} dragging={} tui={} alt={} sb_len={} vo={}",
                sel.session_idx,
                idx,
                win!(self, wi).selection_dragging,
                tui_scroll,
                self.shown(wi, idx).session().l3_alt_screen_active(),
                self.shown(wi, idx).session().l3_scrollback_len(),
                self.shown(wi, idx).view_offset(),
            );
            if self.wheel_sel_path.as_deref() != Some(here.as_str()) {
                lx_info!("core.wheel.selection", &here);
                self.wheel_sel_path = Some(here);
            }
        }
    }
}
