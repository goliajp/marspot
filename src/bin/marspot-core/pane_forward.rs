//! PaneSession bookkeeping, input prediction, and what L2 forwards to a pane's L3 connection.

use super::*;

impl CoreApp {
    /// L1 plugin → control socket → here: enter a PaneSession for the
    /// given shelld_session_id with `caps` capability bits.  Idempotent
    /// on the same caps; bumps the entry on a caps change.
    pub(super) fn pane_session_begin(&mut self, shelld_session_id: u64, caps: u32) {
        let changed = self
            .pane_sessions
            .insert(shelld_session_id, PaneSessionState { caps })
            .map(|prev| prev.caps != caps)
            .unwrap_or(true);
        if changed {
            // FREEZE_GRID may want a redraw to (eventually) freeze
            // visibly; other caps don't change pixels right away.
            self.mark_sid_window_dirty(shelld_session_id);
        }
    }

    /// L1 plugin released the pane back to live mode.
    pub(super) fn pane_session_end(&mut self, shelld_session_id: u64) {
        self.esc_history.remove(&shelld_session_id);
        if self.pane_sessions.remove(&shelld_session_id).is_some() {
            self.mark_sid_window_dirty(shelld_session_id);
        }
    }

    /// Look up an active pane session by shelld_session_id; helper for
    /// the key/render branches.
    pub(super) fn pane_session_for(&self, shelld_session_id: u64) -> Option<&PaneSessionState> {
        self.pane_sessions.get(&shelld_session_id)
    }

    /// Returns the focused pane's shelld_session_id if it currently
    /// has any PaneSession (regardless of caps) — used for the Esc
    /// hatch + key routing.
    pub(super) fn focused_pane_active_session(&self, wi: usize) -> Option<u64> {
        let p = win!(self, wi).try_focused_pane()?;
        let sid = p.shelld_session_id()?;
        if self.pane_sessions.contains_key(&sid) {
            Some(sid)
        } else {
            None
        }
    }

    /// Record an Escape press at `now`.  Returns true if the rolling
    /// window contains ≥ 3 escapes within 5 s — the caller then sends
    /// PaneSessionUserEscape to force-end the session.
    pub(super) fn note_escape_for_pane_session(&mut self, sid: u64, now: std::time::Instant) -> bool {
        const WINDOW: std::time::Duration = std::time::Duration::from_secs(5);
        const THRESHOLD: usize = 3;
        let hist = self.esc_history.entry(sid).or_default();
        while let Some(&front) = hist.front() {
            if now.duration_since(front) > WINDOW {
                hist.pop_front();
            } else {
                break;
            }
        }
        hist.push_back(now);
        if hist.len() > THRESHOLD {
            hist.pop_front();
        }
        hist.len() >= THRESHOLD
    }

    /// L1 plugin → control socket → here: stash a per-shelld-session
    /// right-side decoration for the title strip.  Empty `text` clears
    /// any prior badge.  Forces a redraw on transition.
    /// cc plugin asked to push raw bytes into the PTY behind
    /// `shelld_session_id`.  Find the matching L3 pane and let it
    /// forward via the existing control channel.
    /// Plugin-injected bytes for one session.  Sid-keyed, so it looks
    /// in every window — the plugin knows nothing about windows and
    /// Take back any local-echo guess that has gone unanswered past
    /// its deadline.  Bytes are what normally settle a prediction, and
    /// a program can send none at all — a `sudo` password prompt
    /// echoes nothing until Enter, and the guesses would otherwise
    /// stay painted, showing what was typed.
    pub(super) fn expire_stale_predictions(&mut self) {
        for w in self.windows.iter_mut() {
            for pane in w.panes.iter_mut() {
                // L3 panes keep their terminal in the session
                // process — only a pane that has one in this address
                // space can be swept here.
                let Some(t) = pane.session_mut().terminal_mut_opt() else {
                    continue;
                };
                if t.expire_predictions() {
                    w.needs_render = true;
                }
            }
        }
    }

    /// True while any pane is still showing an unconfirmed guess —
    /// the loop's cue to wake on a timer rather than sleep for a
    /// second waiting for a byte that may never come.
    pub(super) fn any_prediction_pending(&self) -> bool {
        self.windows
            .iter()
            .flat_map(|w| w.panes.iter())
            .any(|p| {
                p.session()
                    .terminal_opt()
                    .is_some_and(|t| t.predictions_pending())
            })
    }

    /// the pane may well be in one that is not focused.
    pub(super) fn inject_input(&mut self, shelld_session_id: u64, bytes: &[u8]) {
        if let Some(pane) = self.pane_by_sid_mut(shelld_session_id) {
            pane.session_mut().forward_inject_input(bytes);
        }
    }

    /// Pass pasted text down to the pane's own L3.
    pub(super) fn forward_pane_paste(&mut self, shelld_session_id: u64, text: &str) {
        if let Some(pane) = self.pane_by_sid_mut(shelld_session_id) {
            pane.session_mut().forward_paste(text);
        }
    }

    /// Pass a grid hold down to the pane's own L3.
    ///
    /// L2 does not act on it: the point of the hold is that it belongs
    /// to the session, not to whichever core is currently drawing it.
    pub(super) fn forward_pane_hold_grid(&mut self, shelld_session_id: u64, on: bool) {
        if let Some(pane) = self.pane_by_sid_mut(shelld_session_id) {
            pane.session_mut().forward_pane_hold_grid(on);
        }
    }

    /// Pass a plugin's markup declaration down to the pane's L3.
    ///
    /// L2 does not act on it: the terminal that would draw the markup
    /// lives in L3, and L2's grid is a mirror of what L3 publishes.
    pub(super) fn forward_pane_render_markup(&mut self, shelld_session_id: u64, on: bool) {
        if let Some(pane) = self.pane_by_sid_mut(shelld_session_id) {
            pane.session_mut().forward_pane_render_markup(on);
            return;
        }
        // Named a pane this core does not have.  Worth a line: the
        // declaration is re-sent every tick, so a steady stream of
        // these means the sid the plugin uses and the one the pane
        // answers to have drifted apart.
        lx_debug!(
            "core.pane_render_markup.no_pane",
            "declaration named a session with no pane here",
            session_id = shelld_session_id
        );
    }

    /// Tell the pane's L3 that whatever had the foreground is gone.
    ///
    /// Same shape as the hold: L2 does not act on it, because the
    /// terminal that holds the mode lives in L3 and L2's copy is a
    /// mirror of L3's next publish.
    pub(super) fn forward_pane_reset_mouse_reporting(&mut self, shelld_session_id: u64) {
        if let Some(pane) = self.pane_by_sid_mut(shelld_session_id) {
            pane.session_mut().forward_pane_reset_mouse_reporting();
        }
    }

    /// How far this pane has receded from active use.  Repaints only
    /// when the value actually moves — L1 sends on change, but a
    /// repeated value after a core swap must not cost a frame.
    pub(super) fn set_pane_recede(&mut self, sid: u64, level: u32) {
        let Some((wi, idx)) = self.find_window_pane_by_sid(sid) else {
            return;
        };
        let pane = &mut win!(self, wi).panes[idx];
        if pane.recede == level {
            return;
        }
        pane.recede = level;
        win!(self, wi).needs_render = true;
    }

    /// Record (or clear) a plugin's wheel-key declaration.
    ///
    /// An empty `up` clears the declaration — the program has left
    /// that pane, so the wheel goes back to the terminal's routing.
    pub(super) fn set_pane_wheel_keys(
        &mut self,
        sid: u64,
        enter: Vec<u8>,
        up: Vec<u8>,
        down: Vec<u8>,
        marker: Vec<u8>,
    ) {
        if up.is_empty() {
            if self.pane_wheel_keys.remove(&sid).is_some() {
                lx_info!("core.pane_wheel_keys.cleared", &format!("sid={sid}"));
            }
            return;
        }
        lx_info!(
            "core.pane_wheel_keys.set",
            &format!(
                "sid={sid} enter={enter:?} up={up:?} down={down:?} marker={marker:?}"
            )
        );
        self.pane_wheel_keys
            .insert(sid, WheelKeys { enter, up, down, marker });
    }

    pub(super) fn set_pane_badge(&mut self, shelld_session_id: u64, text: String) {
        let prev = self.pane_badges.get(&shelld_session_id).cloned();
        let changed = if text.is_empty() {
            self.pane_badges.remove(&shelld_session_id).is_some()
        } else if prev.as_deref() != Some(text.as_str()) {
            self.pane_badges.insert(shelld_session_id, text);
            true
        } else {
            false
        };
        if changed {
            self.mark_sid_window_dirty(shelld_session_id);
        }
    }

    pub(super) fn set_pane_title(&mut self, shelld_session_id: u64, text: String) {
        let prev = self.pane_titles.get(&shelld_session_id).cloned();
        let changed = if text.is_empty() {
            self.pane_titles.remove(&shelld_session_id).is_some()
        } else if prev.as_deref() != Some(text.as_str()) {
            self.pane_titles.insert(shelld_session_id, text);
            true
        } else {
            false
        };
        if changed {
            self.mark_sid_window_dirty(shelld_session_id);
        }
    }
}
