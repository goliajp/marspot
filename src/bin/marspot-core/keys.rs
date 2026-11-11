//! Keys: which are commands, which are the open panel's, and which
//! go to the program in the focused pane.

use marspot::commands::{self, Command};

use super::*;

impl CoreApp {
    pub(super) fn key(&mut self, wi: usize, event: MarspotKeyEvent, modifiers: Modifiers) {
        use marspot::input::{KeyState, LogicalKey, NamedKey};
        let pressed = event.state == KeyState::Pressed;

        // RFC-006 §7 — Esc cancels an in-flight pane drag outright:
        // state cleared, ghost cleared, the release that follows is a
        // plain mouse-up.  Swallowed; a drag is modal.
        if pressed
            && self.pane_drag.is_some()
            && matches!(event.logical, LogicalKey::Named(NamedKey::Escape))
        {
            if let Some(d) = self.pane_drag.take() {
                let from = d.from_wi;
                if from < self.windows.len() {
                    win!(self, from).needs_render = true;
                }
            }
            self.drop_target = None;
            self.clear_drop_preview();
            return;
        }

        // F3+9 — Esc closes the context menu if open.  Swallowed so the
        // \e doesn't reach the focused pane.
        if pressed && win!(self, wi).context_menu.is_some()
            && let LogicalKey::Named(NamedKey::Escape) = event.logical {
                win!(self, wi).context_menu = None;
                win!(self, wi).needs_render = true;
                return;
            }

        // A command's chord.  One that answers now runs, and both its
        // press and its release end here.  One that does not answer —
        // a pane command under a panel, Cmd-W with no panel — goes
        // nowhere if it has Cmd in it: a Cmd chord is never the
        // program's.  Esc with no panel open is the program's.
        //
        // Ahead of the routing to a plugin that holds the keyboard:
        // panels and windows are not pane content.  What reads from or
        // types into the held pane stays the plugin's.
        let panel_open = win!(self, wi).panels.owns_pointer();
        if let Some(row) = commands::for_key(&event, modifiers) {
            let held = row.command.acts_on_focused_pane()
                && self
                    .focused_pane_active_session(wi)
                    .is_some_and(|sid| self.pane_session_holds_keys(sid));
            if row.when.holds(panel_open) && !held {
                let by_escape = matches!(event.logical, LogicalKey::Named(NamedKey::Escape));
                if !pressed || self.run_command(wi, row.command, by_escape) {
                    return;
                }
            } else if modifiers.super_ && !held {
                return;
            }
        }
        // An open panel has the keyboard: nothing else gets through to
        // the pane under it.
        if panel_open {
            return;
        }

        // RFC-003 LOCK_KEYS: if the focused pane is in a plugin-held
        // PaneSession that asked for the keyboard, route the event up
        // to L1 instead of forwarding to the PTY.  Also count Esc
        // presses against the force-end window; 3 in 5 s force-ends
        // the session via PaneSessionUserEscape.
        if pressed
            && let Some(active_sid) = self.focused_pane_active_session(wi) {
                let has_lock = self.pane_session_holds_keys(active_sid);
                let is_esc = matches!(
                    event.logical,
                    LogicalKey::Named(NamedKey::Escape)
                );
                if is_esc {
                    let force_end = self
                        .note_escape_for_pane_session(active_sid, std::time::Instant::now());
                    if force_end {
                        let payload = marspot::shell_proto::encode_pane_session_user_escape(
                            active_sid,
                        );
                        self.pending_to_shell
                            .push((MsgType::PaneSessionUserEscape, payload));
                        // Also drop the L2 mirror immediately so the
                        // pane goes back to normal even if L1 lags.
                        self.pane_session_end(active_sid);
                        return;
                    }
                }
                if has_lock {
                    let wire = marspot::shell_proto::event_to_wire(&event, modifiers);
                    let payload = marspot::shell_proto::encode_pane_session_key(
                        active_sid, &wire,
                    );
                    self.pending_to_shell
                        .push((MsgType::PaneSessionKey, payload));
                    return;
                }
            }

        // An open search bar takes what is left before the pane does.
        if pressed && self.search_consume_key(wi, &event, modifiers) {
            return;
        }

        let Some(pane) = win!(self, wi).try_focused_pane() else { return };

        // RFC-003 Phase 4 (frozen reattach minimum): an exited L3 pane
        // shows whatever was last published; on key press we revive it
        // by spawning a fresh L3 at the same session id.  The bytelog
        // opens in append mode so the new shell's output continues the
        // same on-disk record.  Only revive on a key press the user
        // would actually mean as "wake up" (any printable / Enter /
        // arrow / Tab etc); modifier-only key transitions don't fire.
        //
        // RFC-004 B.2 — vacant slots (boot-time assembly failures)
        // revive through the exact same path: they hold their sid and
        // report is_exited() = true.  A vacant slot with sid 0 (id
        // allocation itself failed at boot) allocates one now.
        if (pane.is_l3() || pane.is_vacant())
            && !pane.is_dormant()
            && pane.is_exited()
            && pressed
        {
            let sid_opt = pane
                .session()
                .shelld_session_id()
                .filter(|&s| s != 0)
                .or_else(|| allocate_next_session_id().ok());
            if let Some(sid) = sid_opt {
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
                *win!(self, wi).focused_pane_mut() =
                    spawn_l3_pane_async(cols, rows, sid, String::new(), &self.event_tx);
                win!(self, wi).needs_render = true;
                lx_event!(
                    "L3_REVIVING",
                    "user keystroke started an off-loop respawn at same id",
                    session_id = sid
                );
                return;
            }
        }

        // L3-backed pane: forward the key *event* to the session process,
        // which encodes with its own modes + local-echoes, then republishes
        // the grid (we re-read it on the GridReady wake).  No local write /
        // predict here.
        if pane.is_l3() {
            // F1+12 — when the search overlay owns the keyboard, any key
            // that fell through to here must not (a) snap_to_live (the
            // F1+9 bouncer) or (b) forward_key into the PTY (would type
            // search-bar shortcuts into the shell).  Cmd-C / Cmd-B
            // already ran above; bar-bound printable / arrows / Enter
            // were already consumed by search_consume_key on the press
            // event.  Anything left (e.g. KeyState::Released, or a
            // Cmd-? we don't recognise) is dropped silently.
            if win!(self, wi).focused_pane_mut()
                .search
                .as_ref()
                .is_some_and(|s| s.bar.focused)
            {
                return;
            }
            if win!(self, wi).focused_pane_mut().snap_to_live() {
                win!(self, wi).needs_render = true;
            }
            if win!(self, wi).selection.is_some() {
                win!(self, wi).selection = None;
                win!(self, wi).selection_dragging = false;
                win!(self, wi).needs_render = true;
            }
            win!(self, wi).focused_pane_mut()
                .session_mut()
                .forward_key(&event, modifiers);
            return;
        }

        let modes = pane.session().terminal().input_modes();
        if let Some(bytes) = key_event_to_bytes(
            &event,
            modifiers,
            modes,
            marspot::input::read_clipboard_text,
        ) {
            // Typing snaps the focused pane's view back to live.
            if win!(self, wi).focused_pane_mut().snap_to_live() {
                win!(self, wi).needs_render = true;
            }
            // Typing into the PTY clears any text selection.
            if win!(self, wi).selection.is_some() {
                win!(self, wi).selection = None;
                win!(self, wi).selection_dragging = false;
                win!(self, wi).needs_render = true;
            }
            let session = win!(self, wi).focused_pane_mut().session_mut();
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
                    if session.terminal_mut().predict_byte(b) {
                        predicted = true;
                    }
                }
            }
            if predicted {
                win!(self, wi).needs_render = true;
            }
            // No cwd refresh here on purpose: an Enter fires *before*
            // the shell has run the line it submits, so reading the cwd
            // on Enter can only ever report the directory the pane was
            // in before the `cd`.  `sweep_pane_cwds` reads it after the
            // fact instead.
        }
    }

    fn pane_session_holds_keys(&self, sid: u64) -> bool {
        self.pane_session_for(sid)
            .is_some_and(|s| s.has(marspot::shell_proto::PANE_SESSION_CAP_LOCK_KEYS))
    }

    /// Runs `command`.  False when the key that asked for it should
    /// carry on to the pane as a key.
    fn run_command(&mut self, wi: usize, command: Command, by_escape: bool) -> bool {
        match command {
            Command::Settings => self.toggle_panel(wi, SurfaceKind::Settings),
            Command::Layout => self.toggle_panel(wi, SurfaceKind::Layout),
            Command::Processes => self.toggle_panel(wi, SurfaceKind::Processes),
            Command::Accounts => self.toggle_panel(wi, SurfaceKind::Accounts),
            Command::Shortcuts => self.toggle_panel(wi, SurfaceKind::Shortcuts),
            Command::ClosePanel => {
                let key = if by_escape { SurfaceKey::Esc } else { SurfaceKey::CmdW };
                if let KeyAction::Close(change) = win!(self, wi).panels.on_key(key) {
                    self.apply_surface_change(wi, change);
                }
            }
            Command::NewPane => self.add_pane(wi),
            Command::Copy => {
                self.copy_selection_to_clipboard(wi);
            }
            Command::Paste => return self.paste_into_focused_pane(wi),
            Command::Find => {
                // The search bar takes an open panel's place, as
                // another panel would.
                let change = win!(self, wi).panels.close(CloseBy::Replaced);
                if change.closed.is_some() {
                    self.apply_surface_change(wi, change);
                }
                self.handle_cmd_f(wi);
            }
            Command::ToggleSidebar => self.toggle_sidebar(wi),
            Command::PrevPane => self.focus_neighbour(wi, false),
            Command::NextPane => self.focus_neighbour(wi, true),
            Command::FocusPane(n) => self.focus_pane(wi, n as usize),
            // AppKit's and the shell's: the menu bar or the shell has
            // taken these before a key gets this far.
            Command::About
            | Command::Hide
            | Command::HideOthers
            | Command::ShowAll
            | Command::Quit
            | Command::NewWindow
            | Command::FullScreen
            | Command::Minimize => {}
        }
        true
    }

    /// The session process has no pasteboard, so the text is read here
    /// and sent to it; the session wraps it for a program that asked
    /// for bracketed paste and writes the PTY.
    fn paste_into_focused_pane(&mut self, wi: usize) -> bool {
        let Some(pane) = win!(self, wi).try_focused_pane() else { return true };
        // A key into an exited pane revives it, and an in-process pane
        // pastes in its own encoder: both are the key path's.
        if !pane.is_l3() || pane.is_exited() {
            return false;
        }
        if pane.search.as_ref().is_some_and(|s| s.bar.focused) {
            return true;
        }
        if let Some(text) = marspot::input::read_clipboard_text() {
            if win!(self, wi).focused_pane_mut().snap_to_live() {
                win!(self, wi).needs_render = true;
            }
            win!(self, wi).focused_pane_mut().session_mut().forward_paste(&text);
        }
        true
    }

    /// The sidebar's [+] and Cmd-T.
    pub(super) fn add_pane(&mut self, wi: usize) {
        if win!(self, wi).panes.len() >= SESSION_COUNT_HARD_CAP {
            return;
        }
        self.spawn_session(wi);
        self.rebuild_layout(wi);
        let w = &win!(self, wi);
        lx_event!("PANE_ADDED", "a pane was added", window = w.window_id, panes = w.panes.len() as u32);
    }

    pub(super) fn toggle_sidebar(&mut self, wi: usize) {
        win!(self, wi).sidebar_collapsed = !win!(self, wi).sidebar_collapsed;
        self.rebuild_layout(wi);
    }

    /// Makes pane `idx` the focused one.  Every way focus moves by
    /// hand ends here, so the log has each move once.
    pub(super) fn set_focus(&mut self, wi: usize, idx: usize) {
        let w = &mut win!(self, wi);
        if w.focused_idx == idx {
            return;
        }
        w.focused_idx = idx;
        w.needs_render = true;
        lx_event!("PANE_FOCUS", "the focused pane changed", window = w.window_id, pane = idx as u32);
    }

    /// Focuses pane `idx` as a click on its sidebar row does, if there
    /// is such a pane.
    pub(super) fn focus_pane(&mut self, wi: usize, idx: usize) {
        if idx >= win!(self, wi).panes.len() || idx == win!(self, wi).focused_idx {
            return;
        }
        self.set_focus(wi, idx);
        let _ = win!(self, wi).focused_pane_mut().snap_to_live();
    }

    /// The pane after or before the focused one, in sidebar order,
    /// round the end.
    fn focus_neighbour(&mut self, wi: usize, forward: bool) {
        let w = &win!(self, wi);
        let n = w.panes.len();
        if n < 2 {
            return;
        }
        let step = if forward { 1 } else { n - 1 };
        let idx = (w.focused_idx + step) % n;
        self.focus_pane(wi, idx);
    }

    /// The shortcuts panel's frame, where it has been dragged to.
    pub(super) fn shortcuts_panel_rect(&self, wi: usize) -> marspot_term::layout::Rect {
        let w = &win!(self, wi);
        let top = w.layout.top_inset;
        let home = marspot::ui::components::shortcuts_panel::panel_rect(
            w.w_phys, w.h_phys, self.renderer.cell_dims(), top,
        );
        surface_stack::place(home, w.panels.offset(), w.w_phys, w.h_phys, top)
    }
}

#[cfg(test)]
mod tests {
    use marspot::commands::{Chord, Key, CMD, SHIFT};
    use marspot_term::test_env::sandbox_state_dir;

    use super::super::window_state_tests::{app_with, win};
    use super::*;

    fn press(app: &mut CoreApp, c: char, mods: u8) {
        let chord = Chord { key: Key::Char(c), mods };
        app.key(0, chord.key_event(), chord.modifiers());
    }

    fn three_panes() -> Vec<Pane> {
        vec![Pane::new_vacant(10, 80, 24), Pane::new_vacant(11, 80, 24), Pane::new_vacant(12, 80, 24)]
    }

    const PANEL_CHORDS: [(char, u8, SurfaceKind); 5] = [
        (',', CMD, SurfaceKind::Settings),
        ('l', SHIFT | CMD, SurfaceKind::Layout),
        ('p', SHIFT | CMD, SurfaceKind::Processes),
        ('c', SHIFT | CMD, SurfaceKind::Accounts),
        ('/', CMD, SurfaceKind::Shortcuts),
    ];

    #[test]
    fn a_panel_chord_opens_its_panel_replaces_another_and_closes_its_own() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, three_panes())]);
        for (c, mods, kind) in PANEL_CHORDS {
            press(&mut app, c, mods);
            assert_eq!(app.windows[0].panels.open(), Some(kind), "{kind:?} opens");
            for (c2, mods2, other) in PANEL_CHORDS.into_iter().filter(|(_, _, o)| *o != kind) {
                press(&mut app, c2, mods2);
                assert_eq!(app.windows[0].panels.open(), Some(other), "{other:?} over {kind:?}");
                press(&mut app, c, mods);
                assert_eq!(app.windows[0].panels.open(), Some(kind), "{kind:?} over {other:?}");
            }
            press(&mut app, c, mods);
            assert_eq!(app.windows[0].panels.open(), None, "{kind:?} closes on its own chord");
        }
    }

    #[test]
    fn pane_commands_do_nothing_under_a_panel() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, three_panes())]);
        let sidebar = app.windows[0].sidebar_collapsed;
        press(&mut app, ',', CMD);
        for (c, mods) in [('2', CMD), (']', CMD), ('[', CMD), ('b', CMD), ('v', CMD), ('c', CMD)] {
            press(&mut app, c, mods);
            let w = &app.windows[0];
            assert_eq!(w.focused_idx, 0, "{c}");
            assert_eq!(w.sidebar_collapsed, sidebar, "{c}");
            assert_eq!(w.panels.open(), Some(SurfaceKind::Settings), "{c}");
        }
    }

    #[test]
    fn pane_chords_move_focus_by_sidebar_order() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, three_panes())]);
        let at = |app: &mut CoreApp, c: char| {
            press(app, c, CMD);
            app.windows[0].focused_idx
        };
        assert_eq!(at(&mut app, '2'), 1);
        assert_eq!(at(&mut app, '3'), 2);
        assert_eq!(at(&mut app, '9'), 2, "there is no ninth pane");
        assert_eq!(at(&mut app, ']'), 0, "next, round the end");
        assert_eq!(at(&mut app, ']'), 1);
        assert_eq!(at(&mut app, '['), 0);
        assert_eq!(at(&mut app, '['), 2, "previous, round the start");
        assert_eq!(at(&mut app, '1'), 0);
        let sidebar = app.windows[0].sidebar_collapsed;
        press(&mut app, 'b', CMD);
        assert_eq!(app.windows[0].sidebar_collapsed, !sidebar);
    }

    #[test]
    fn find_takes_an_open_panels_place() {
        let sandbox = sandbox_state_dir();
        let mut app = app_with(&sandbox, vec![win(1, three_panes())]);
        press(&mut app, 'p', SHIFT | CMD);
        assert!(app.windows[0].process_panel.is_some());
        press(&mut app, 'f', CMD);
        let w = &app.windows[0];
        assert_eq!(w.panels.open(), None);
        assert!(w.process_panel.is_none(), "the panel's data went with it");
        assert!(w.panes[0].search.is_some(), "the search bar is open on the focused pane");
    }
}
