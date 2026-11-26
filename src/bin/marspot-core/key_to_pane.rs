//! A key on its way to the session a slot shows.

use marspot::input::KeyState;

use super::held_keys::Went;
use super::*;

impl CoreApp {
    /// A key L1 sent for window `win`, dropped when that window has gone.
    pub(super) fn key_from_shell(&mut self, win: u32, event: MarspotKeyEvent, modifiers: Modifiers) {
        if let Some(wi) = self.window_index(win) {
            self.key(wi, event, modifiers)
        }
        trace::mark(Stage::CoreKeyHandled, 0);
    }

    /// Whether the key being let go of was pressed for a program, which
    /// is told, or for nobody.
    pub(super) fn release_follows_press(&mut self, event: &MarspotKeyEvent, modifiers: Modifiers) -> bool {
        match self.pane_control.shadows.peeks.held.released(&event.logical) {
            None => false,
            Some(Went::Nowhere) => true,
            Some(Went::To(sid)) => {
                if let Some(pane) = self.pane_by_sid_mut(sid) {
                    pane.session_mut().forward_key(event, modifiers);
                }
                true
            }
        }
    }

    /// A key held since before what is shown changed repeats for nobody.
    pub(super) fn repeat_is_anothers(&self, wi: usize, event: &MarspotKeyEvent) -> bool {
        if !event.repeat {
            return false;
        }
        match self.pane_control.shadows.peeks.held.of(&event.logical) {
            None => false,
            Some(Went::Nowhere) => true,
            Some(Went::To(sid)) => self.focused_shown(wi).and_then(|p| p.shelld_session_id()) != Some(sid),
        }
    }

    /// Cmd-W with no panel open leaves the focused slot's view.
    pub(super) fn view_leaves_on(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) -> bool {
        use marspot::commands::{self, Command};
        let asked =
            modifiers.super_ && commands::for_key(event, modifiers).is_some_and(|r| r.command == Command::ClosePanel);
        if !asked || !self.leave_focused_view(wi) {
            return false;
        }
        self.pane_control.shadows.peeks.held.swallowed(&event.logical);
        true
    }

    /// Show the newest of what the focused slot shows.
    pub(super) fn snap_focused_to_live(&mut self, wi: usize) {
        if let Some(pane) = self.focused_shown_mut(wi) {
            let _ = pane.snap_to_live();
        }
    }

    pub(super) fn key_to_pane(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) {
        if event.state == KeyState::Pressed && self.key_revives(wi) {
            return;
        }
        match self.focused_shown(wi).map(Pane::is_l3) {
            Some(true) => self.key_to_session(wi, event, modifiers),
            Some(false) => self.key_to_terminal(wi, event, modifiers),
            None => {}
        }
    }

    /// Whether the key brought back the focused slot's lost session.
    ///
    /// RFC-003 Phase 4 (frozen reattach minimum): an exited L3 pane
    /// shows whatever was last published; on key press we revive it
    /// by spawning a fresh L3 at the same session id.  The bytelog
    /// opens in append mode so the new shell's output continues the
    /// same on-disk record.  Only revive on a key press the user
    /// would actually mean as "wake up" (any printable / Enter /
    /// arrow / Tab etc); modifier-only key transitions don't fire.
    ///
    /// RFC-004 B.2 — vacant slots (boot-time assembly failures)
    /// revive through the exact same path: they hold their sid and
    /// report is_exited() = true.  A vacant slot with sid 0 (id
    /// allocation itself failed at boot) allocates one now.
    ///
    /// What is viewed in the slot's place is never revived, and the
    /// slot itself only once it is what the user sees.
    fn key_revives(&mut self, wi: usize) -> bool {
        let Some(pane) = win!(self, wi).try_focused_pane() else { return false };
        let lost = (pane.is_l3() || pane.is_vacant()) && !pane.is_dormant() && pane.is_exited();
        if !lost || pane.peek.is_some() {
            return false;
        }
        let sid = pane
            .session()
            .shelld_session_id()
            .filter(|&s| s != 0)
            .or_else(|| allocate_next_session_id().ok());
        let Some(sid) = sid else { return false };
        // Layout already sized the pane — keep its current
        // cell dims so the new L3 boots at the same shape.
        let (cols, rows) = win!(self, wi)
            .layout
            .cells
            .get(win!(self, wi).focused_idx)
            .map(|c| (c.cols, c.rows))
            .unwrap_or((INITIAL_COLS, INITIAL_ROWS));
        // Off-loop, same as [+]: a keystroke into a dead slot
        // must not freeze the other fifteen panes while the
        // replacement boots.
        *win!(self, wi).focused_pane_mut() = spawn_l3_pane_async(cols, rows, sid, String::new(), &self.event_tx);
        win!(self, wi).needs_render = true;
        lx_event!(
            "L3_REVIVING",
            "user keystroke started an off-loop respawn at same id",
            session_id = sid
        );
        true
    }

    /// L3-backed pane: forward the key *event* to the session process,
    /// which encodes with its own modes + local-echoes, then republishes
    /// the grid (we re-read it on the GridReady wake).  No local write /
    /// predict here.
    fn key_to_session(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) {
        let Some(pane) = self.focused_shown_mut(wi) else { return };
        // F1+12 — when the search overlay owns the keyboard, any key
        // that fell through to here must not (a) snap_to_live (the
        // F1+9 bouncer) or (b) forward_key into the PTY (would type
        // search-bar shortcuts into the shell).  Cmd-C / Cmd-B
        // already ran above; bar-bound printable / arrows / Enter
        // were already consumed by search_consume_key on the press
        // event.  Anything left (e.g. KeyState::Released, or a
        // Cmd-? we don't recognise) is dropped silently.
        if pane.search.as_ref().is_some_and(|s| s.bar.focused) {
            return;
        }
        let moved = pane.snap_to_live();
        pane.session_mut().forward_key(event, modifiers);
        let sid = pane.session().l3_session_id();
        let w = &mut win!(self, wi);
        w.needs_render |= moved || w.selection.is_some();
        w.selection = None;
        w.selection_dragging = false;
        if event.state == KeyState::Pressed {
            self.note_typing(wi);
        }
        if let Some(sid) = sid.filter(|_| event.state == KeyState::Pressed && !event.repeat) {
            self.pane_control.shadows.peeks.held.went_to(&event.logical, sid);
        }
    }

    /// A pane whose terminal is in this process.
    fn key_to_terminal(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) {
        let Some(pane) = self.focused_shown_mut(wi) else { return };
        let modes = pane.session().terminal().input_modes();
        let Some(bytes) = key_event_to_bytes(event, modifiers, modes, marspot::input::read_clipboard_text) else {
            return;
        };
        // Typing snaps the focused pane's view back to live.
        let moved = pane.snap_to_live();
        let session = pane.session_mut();
        let _ = session.write(&bytes);
        // Local-echo: paint each printable-ASCII byte to the grid
        // immediately, ahead of the PTY round trip — but only
        // for plain text runs.  An escape sequence (ESC-leading)
        // is a control code: the leading 0x1b isn't printable so
        // predict_byte rejects it, but the sequence's tail
        // (`[C`, `[A`, …) is ASCII printable and would smear
        // literally into the grid.  Skip the whole run when it
        // starts with ESC.
        let mut predicted = false;
        if !bytes.starts_with(&[0x1b]) {
            for &b in bytes.as_ref() {
                predicted |= session.terminal_mut().predict_byte(b);
            }
        }
        // Typing into the PTY clears any text selection.
        let w = &mut win!(self, wi);
        w.needs_render |= moved || predicted || w.selection.is_some();
        w.selection = None;
        w.selection_dragging = false;
        // No cwd refresh here on purpose: an Enter fires *before*
        // the shell has run the line it submits, so reading the cwd
        // on Enter can only ever report the directory the pane was
        // in before the `cd`.  `sweep_pane_cwds` reads it after the
        // fact instead.
    }
}

#[cfg(test)]
#[path = "key_alloc_tests.rs"]
mod alloc_tests;
