//! Rebuilding a window's layout, tracking each pane's cwd, and saving session state.

use super::*;

impl CoreApp {
    pub(super) fn rebuild_layout(&mut self, wi: usize) {
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let sidebar_phys = if win!(self, wi).sidebar_collapsed {
            0.0
        } else {
            SIDEBAR_W_LOGICAL * win!(self, wi).scale
        };
        let (lc, lr) = (win!(self, wi).grid_cols, win!(self, wi).grid_rows);
        // One pane gets no title strip.
        //
        // The strip says which pane is which, and with one pane there
        // is no which.  The window header above it already carries
        // the traffic lights and the icon buttons, so the strip is a
        // second band of chrome that names the only thing on screen —
        // which is what makes a single window look busy next to a
        // plain terminal.
        //
        // It costs the pane-drag handle in that state.  A lone pane
        // has nowhere to be dragged to that dragging the window does
        // not already do, and the strip comes back the moment there
        // are two.
        let title_h = if win!(self, wi).panes.len() <= 1 {
            0.0
        } else {
            CELL_TITLE_PT * win!(self, wi).scale
        };
        // a row per pane of the window and per child listed with them
        self.refresh_sidebar_tree(wi);
        let rows = self.sidebar_rows(wi).len();
        let layout = Layout::build(
            win!(self, wi).w_phys,
            win!(self, wi).h_phys,
            sidebar_phys,
            HEADER_PT * win!(self, wi).scale,
            title_h,
            lc,
            lr,
            cell_w,
            cell_h,
        )
        .with_chrome(win!(self, wi).scale, rows, win!(self, wi).lights_right_phys);
        let layout = self.take_strip_rows(wi, layout);
        for (i, p) in win!(self, wi).panes.iter_mut().enumerate() {
            if let Some(rect) = layout.cells.get(i) {
                p.resize(rect.cols, rect.rows);
                // the one viewed in its place, with it
                if let Some(v) = p.peeked().and_then(|sid| self.shadows.iter_mut().find(|s| s.shelld_session_id() == Some(sid))) {
                    v.resize(rect.cols, rect.rows);
                }
            }
        }
        win!(self, wi).layout = layout;
        win!(self, wi).needs_render = true;
        // The shape of the BG region just changed (sidebar / layout
        // mode / pane count / window dims).  Force a hard Clear on
        // the next IOSurface render so any newly-uncovered area
        // shows SIDEBAR_BG, not the previous frame's stale pixels.
        // (Steady-state frames use Load to dodge the cross-process
        // race; see `MetalRenderer::clear_bg_required`.)
        win!(self, wi).render.mark_bg_clear_required();
    }

    /// F3+3.3 — reset `card_slots` to identity for the current
    /// pending grid shape.  Called when the modal opens, when the
    /// user changes cols/rows in the modal (since the cell count
    /// changes), and on apply (after permuting).
    /// Re-read one session's working directory into `pane_cwds`: the
    /// foreground program's, or the shell's when the shell is in front.  Returns
    /// `true` when the stored path actually changed — callers use that
    /// to decide whether a repaint is owed, so a sweep over N unchanged
    /// panes costs N syscalls and zero frames.
    ///
    /// The pid comes from `shell_child_pids`, refilled from entry.toml
    /// only when it is missing or has stopped resolving.  A pid that
    /// fails to resolve leaves the last known cwd in place: an L3
    /// mid-execv has no live child for a moment, and blanking the title
    /// for that moment would be a worse lie than a slightly old one.
    pub(super) fn resolve_pane_cwd(&mut self, sid: u64) -> bool {
        Self::note_pane_code(sid);
        let cached_pid = self.shell_child_pids.get(&sid).copied();
        let mut path = cached_pid.and_then(marspot::pidtree::pane_cwd);
        if path.is_none() {
            // Either no pid cached yet, or the cached one is gone
            // (shell replaced, L3 execv'd).  One entry.toml read, then
            // retry — and only cache the pid once it has resolved.
            self.shell_child_pids.remove(&sid);
            if let Some(pid) = read_shell_child_pid(sid) {
                path = marspot::pidtree::pane_cwd(pid);
                if path.is_some() {
                    self.shell_child_pids.insert(sid, pid);
                }
            }
        }
        let Some(path) = path else { return false };
        let next = path.to_string_lossy().into_owned();
        if self.pane_cwds.get(&sid).is_some_and(|prev| *prev == next) {
            return false;
        }
        // INFO, not DEBUG: the runtime default level is Info, so a
        // DEBUG line does not exist on a real machine — and this is the
        // first thing worth reading when a title looks wrong.  Rate is
        // capped by construction at one line per pane per sweep.
        lx_info!(
            "core.pane_cwd.changed",
            "pane cwd moved; title placeholder follows",
            sid = sid,
            cwd = next.as_str()
        );
        self.pane_cwds.insert(sid, next);
        true
    }

    /// Re-read every pane's cwd, at most once per `CWD_SWEEP_INTERVAL`.
    /// Driven from the main loop's periodic block, so it rides whatever
    /// wake the loop already had (PTY output while the user works, the
    /// 1 s idle timeout otherwise) rather than owning a timer.
    ///
    /// Only the windows whose titles actually changed are marked dirty.
    /// Emit `PaneFocused` for any window whose focused pane changed
    /// since the last check.  Cheap enough to call every loop
    /// iteration: one map lookup per window, and nothing on the wire
    /// unless focus actually moved.
    pub(super) fn notify_pane_focus_changes(&mut self) {
        for wi in 0..self.windows.len() {
            let sid = self.focused_session(wi);
            let Some(sid) = sid else { continue };
            if self.last_focus_notified.get(&wi) == Some(&sid) {
                continue;
            }
            self.last_focus_notified.insert(wi, sid);
            self.pending_to_shell.push((
                MsgType::PaneFocused,
                marspot::shell_proto::encode_pane_focused(sid),
            ));
        }
        // The keyboard is in one pane at most: the key window's focused
        // one, while the app is active.
        // A view that has ended takes no keys, and they are not the
        // hidden pane's either.
        let now = if self.renderer.window_focused() && !self.focused_view_ended(self.key_window) {
            self.focused_session(self.key_window)
        } else {
            None
        };
        if now != self.terminal_focus {
            let was = std::mem::replace(&mut self.terminal_focus, now);
            // the shell credits each key to this pane: PaneFocused does
            // not change when only the key window does
            self.pending_to_shell.push((MsgType::KeyboardPane, marspot::shell_proto::encode_keyboard_pane(now)));
            for (sid, focused) in [(was, false), (now, true)] {
                let Some(sid) = sid else { continue };
                if let Some(pane) = self.pane_by_sid_mut(sid) {
                    pane.session_mut().forward_pane_terminal_focus(focused);
                }
            }
        }
    }

    pub(super) fn sweep_pane_cwds(&mut self) {
        if self.last_cwd_sweep.elapsed() < CWD_SWEEP_INTERVAL {
            return;
        }
        self.last_cwd_sweep = Instant::now();
        for wi in 0..self.windows.len() {
            let sids: Vec<u64> = win!(self, wi)
                .panes
                .iter()
                .flat_map(|p| [p.shelld_session_id(), p.peeked()])
                .flatten()
                .collect();
            let mut window_changed = false;
            for sid in sids {
                if self.resolve_pane_cwd(sid) {
                    window_changed = true;
                }
            }
            if window_changed {
                win!(self, wi).needs_render = true;
                self.cwd_save_pending = true;
            }
        }
        // `last_cwd` in the saved state feeds the respawn cwd on cold
        // boot, so a move wants persisting — but rate-limited, since a
        // cd-ing script must not turn into one fsync per second.  The
        // pending flag makes the limit a delay rather than a drop.
        if self.cwd_save_pending && self.last_cwd_save.elapsed() >= CWD_SAVE_MIN_GAP {
            self.last_cwd_save = Instant::now();
            self.cwd_save_pending = false;
            self.save_session_state();
        }
    }

    /// F3+6 — snapshot every persistable bit of state to
    /// `shell-state.bin`.  Called from spawn / close / focus-change /
    /// layout-apply / title-commit so a hard kill leaves a recent
    /// state on disk.  ~50 us per call (memcpy + atomic rename); no
    /// debounce because we never call this on the render hot path.
    pub(super) fn save_session_state(&self) {
        use marspot::state::SavedState;
        // The user closed the last pane of the last window — the saved
        // layout is gone on purpose and this process is being torn
        // down.  Writing now would put it back.
        if self.layout_discarded {
            return;
        }
        // RFC-005 step 6 — every window, in slot order.  Saving only
        // the key window is what made opening a second window
        // destructive: the new window became key the instant it
        // appeared, and the next save replaced a 16-pane record with
        // its single pane.
        //
        // Parked windows (closed, but still holding live sessions) are
        // merged back in at the slot they had, so their geometry in
        // `window-state.bin` still lines up and the next launch brings
        // them back where they were.
        let mut slots: Vec<(usize, marspot::state::SavedWindowLayout)> = self
            .windows
            .iter()
            .map(|w| (w.frame_index, self.window_layout_record(w)))
            .chain(self.parked_windows.iter().cloned())
            // Records whose window L1 has not opened yet are part of
            // the layout too.  Without them the boot save — which runs
            // before any restore lands — rewrote the file with just
            // the boot window, and a crash in that gap lost every
            // other window.
            .chain(self.saved_windows.iter().cloned())
            .collect();
        slots.sort_by_key(|(slot, _)| *slot);
        // `key_window` indexes the live list; the saved list is the
        // merged one, so translate through the slot.
        let key_slot = self
            .windows
            .get(self.key_window)
            .map(|w| w.frame_index)
            .unwrap_or(0);
        let key_window = slots
            .iter()
            .position(|(slot, _)| *slot == key_slot)
            .unwrap_or(0) as u16;
        let saved = SavedState {
            windows: slots.into_iter().map(|(_, r)| r).collect(),
            key_window,
        };
        if let Err(e) = marspot::state::write(&saved) {
            lx_warn!(
                "core.state_file.write_failed",
                &format!("{e}; saved state not persisted this tick")
            );
        }
    }
}

/// F3+1 — read `sessions/<sid>/entry.toml`'s `shell_child_pid` field.
/// Returns None on missing file, parse error, or absent field; the
/// caller renders the pane's tree as "no root pid" in that case.
/// Cheap one-shot read (typical entry.toml ~200 bytes); we don't
/// cache because panel refresh is already throttled to ≥ 2 s.
pub(super) fn read_shell_child_pid(shelld_session_id: u64) -> Option<i32> {
    let path = marspot_term::session_registry::session_dir(shelld_session_id)
        .join("entry.toml");
    let body = std::fs::read_to_string(&path).ok()?;
    for line in body.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("shell_child_pid") {
            // tolerate "= 1234" or "=1234"
            let v = rest.trim_start_matches(|c: char| c == '=' || c.is_whitespace());
            return v.parse::<i32>().ok();
        }
    }
    None
}

#[cfg(test)]
#[path = "keyboard_pane_tests.rs"]
mod keyboard_pane_tests;
