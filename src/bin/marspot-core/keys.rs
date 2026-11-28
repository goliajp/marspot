//! Keys: which are commands, which are the open panel's, and which
//! go to the program in the focused pane.

use marspot::commands::{self, Command};
use marspot::shell_proto::PromptDir;

use super::*;

#[path = "helpers_command.rs"]
mod helpers_command;

impl CoreApp {
    pub(super) fn preedit(&mut self, wi: usize, text: String) {
        if self.model_guard_preedit(wi, &text) {
            return;
        }
        if !text.is_empty() {
            self.typed_before_view(wi);
            self.strip_keys_interrupted();
        }
        if win!(self, wi).ime_preedit != text {
            self.note_typing(wi);
            win!(self, wi).ime_preedit = text;
            win!(self, wi).needs_render = true;
        }
    }

    pub(super) fn key(&mut self, wi: usize, event: MarspotKeyEvent, modifiers: Modifiers) {
        use marspot::input::{KeyState, LogicalKey, NamedKey};
        if self.model_guard_drops_commit(wi, &event) {
            return;
        }
        let pressed = event.state == KeyState::Pressed;
        if !pressed && self.release_follows_press(&event, modifiers) {
            return;
        }

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

        // an open context menu takes every key: the arrows, Enter and
        // Esc are its, and nothing typed reaches the pane under it
        if self.menu_key(wi, &event) || self.close_ask_key(wi, &event) {
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
        if pressed && !panel_open && self.view_leaves_on(wi, &event, modifiers) {
            return;
        }
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
            if pressed {
                self.tree_panel_key(wi, &event, modifiers);
            }
            return;
        }
        if pressed {
            self.typed_before_view(wi);
        }
        if self.ended_view_takes_key(wi, &event) || (pressed && self.repeat_is_anothers(wi, &event)) {
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
        if self.model_guard_key(wi, &event, modifiers) || self.install_ask_key(wi, &event, modifiers) {
            return;
        }

        if !self.strip_key(wi, &event, modifiers) {
            self.key_to_pane(wi, &event, modifiers);
        }
    }

    fn pane_session_holds_keys(&self, sid: u64) -> bool {
        self.pane_session_for(sid)
            .is_some_and(|s| s.has(marspot::shell_proto::PANE_SESSION_CAP_LOCK_KEYS))
    }

    /// Runs `command`.  False when the key that asked for it should
    /// carry on to the pane as a key.
    fn run_command(&mut self, wi: usize, command: Command, by_escape: bool) -> bool {
        // a command is not a key the strip waits on or has
        self.strip_keys_interrupted();
        match command {
            Command::Settings => self.toggle_panel(wi, SurfaceKind::Settings),
            Command::Layout => self.toggle_panel(wi, SurfaceKind::Layout),
            Command::Processes => self.toggle_panel(wi, SurfaceKind::Processes),
            Command::Accounts => self.toggle_panel(wi, SurfaceKind::Accounts),
            Command::Helpers => self.helpers_command(wi),
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
            Command::PrevPrompt => self.jump_to_prompt(wi, PromptDir::Back),
            Command::NextPrompt => self.jump_to_prompt(wi, PromptDir::Forward),
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
        if self.focused_shown(wi).and_then(Pane::shelld_session_id).is_some_and(|sid| self.model_guard_holds(sid)) {
            return true;
        }
        let Some(pane) = self.focused_shown_mut(wi) else { return true };
        // A key into an exited pane revives it, and an in-process pane
        // pastes in its own encoder: both are the key path's.
        if !pane.is_l3() || pane.is_exited() {
            return false;
        }
        if pane.search.as_ref().is_some_and(|s| s.bar.focused) {
            return true;
        }
        if let Some(text) = marspot::input::read_clipboard_text() {
            let moved = pane.snap_to_live();
            pane.session_mut().forward_paste(&text);
            win!(self, wi).needs_render |= moved;
            self.note_typing(wi);
        }
        true
    }

    /// Cmd-Up / Cmd-Down: the focused pane's session is asked for the
    /// prompt that way, and the view moves when it answers.
    fn jump_to_prompt(&mut self, wi: usize, dir: PromptDir) {
        if let Some(pane) = self.focused_shown_mut(wi) {
            pane.jump_to_prompt(dir);
        }
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
        self.snap_focused_to_live(wi);
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
