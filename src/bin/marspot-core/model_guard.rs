//! Claude Code's `/model` writes the default model of every account
//! that shares the settings file.  While `claude.model` is managed:
//!
//! * Enter or Tab on a `/model` in a Claude Code input box (or on `/mo`,
//!   `/mod`, `/mode`, which its completion list runs as `/model`) is
//!   held, and a notice over the pane says what it does.  `y` sends the
//!   held key and tells the shell, which then keeps the settings file's
//!   next change of model; Esc drops it and leaves the box as it was.
//!   Any other key is swallowed while the question is up.  An input
//!   method's y counts too: a composition of exactly y (half or full
//!   width) answers as the key would, and what that composition later
//!   commits is dropped; any other composition is swallowed.  Pastes and
//!   file drops into the pane wait until it is answered.
//! * Input from programs (typing, pastes, injected bytes) that would
//!   send `/model` from such a box is refused: only the keyboard can
//!   confirm.  What one program call leaves unsent in the box is
//!   remembered for a few seconds, so `/model` in one call and the
//!   Enter in the next is refused before the screen shows the first.
//!
//! The check runs only on Enter and Tab, and reads cells until it finds
//! Claude Code's prompt; any other key costs one comparison.  It
//! reads the live screen, whatever the pane is scrolled to, and only
//! while the pane itself has the keys: an Enter for a helper strip is
//! the strip's.
//!
//! What it cannot see: an Enter that reaches the pane before the screen
//! shows the `/model` typed just before it (keys typed faster than
//! Claude Code redraws its box) is sent as it would be without the
//! guard.  The shared-settings watch in the shell is what still stands
//! between such a `/model` and the other accounts.

use super::*;
use marspot::input::{KeyState, LogicalKey, NamedKey};
use marspot::pane_read::slash_model::{claude_box_text, claude_prompt_row, is_model_command, typed_into};
use marspot::settings::CcModel;
use marspot::ui::strings::{t, Msg};

/// How long what a program's input left unsent in a box stands for what
/// the box holds, while the screen has yet to show it, and how many
/// sessions' are kept.
pub(super) const UNSENT_STANDS: Duration = Duration::from_secs(3);
pub(super) const UNSENT_CAP: usize = 16;

/// The refusal a program is given.
pub(super) const REFUSED: &str = "refused: this would send /model to Claude Code, which changes the default \
     model of every account sharing its settings; only the keyboard can confirm it (claude.model = managed)";

/// A held Enter: the session it was for and the key itself.
pub(super) struct Held {
    pub(super) window_id: u32,
    pub(super) sid: u64,
    enter: MarspotKeyEvent,
    mods: Modifiers,
}

fn managed() -> bool {
    marspot::settings::get().cc_model == CcModel::Managed
}

fn plain(m: Modifiers) -> bool {
    !(m.shift || m.control || m.alt || m.super_)
}

/// What an input method composes or commits for y, half or full width.
fn is_yes(text: &str) -> bool {
    matches!(text, "y" | "Y" | "\u{FF59}" | "\u{FF39}")
}

/// What Claude Code's box in `pane` holds, when `pane` shows one live.
fn box_in(pane: &Pane) -> Option<String> {
    let grid = pane.session().grid();
    (pane.is_l3() && claude_prompt_row(grid).is_some()).then(|| claude_box_text(grid))?
}

impl CoreApp {
    /// A key for the focused slot of window `wi`: whether the guard took it.
    pub(super) fn model_guard_key(&mut self, wi: usize, event: &MarspotKeyEvent, mods: Modifiers) -> bool {
        let window_id = self.windows[wi].window_id;
        if event.state == KeyState::Pressed && self.notices.composed_yes.is_some_and(|c| c.0 == window_id) {
            self.notices.composed_yes = None;
            // the commit of the composition that answered: it is not for the box
            if event.logical == LogicalKey::Other && event.text.is_some() {
                self.pane_control.shadows.peeks.held.swallowed(&event.logical);
                lx_event!("MODEL_GUARD_COMMIT_DROPPED", "what the composition that answered the guard committed was dropped");
                return true;
            }
        }
        let shown = self.focused_shown(wi).and_then(Pane::shelld_session_id);
        if let Some(held) = self.notices.guard.as_ref().filter(|h| h.window_id == window_id) {
            if Some(held.sid) == shown && !self.strip_has_keys(window_id) {
                // the window's own shortcuts still work while it asks
                return !mods.super_ && self.answer_guard(event, mods);
            }
            // focus moved away from the question: it is dropped, as Esc would
            self.drop_guard("focus_moved");
        }
        // Tab runs the entry Claude Code's completion list has highlighted
        let enter = event.state == KeyState::Pressed
            && !event.repeat
            && matches!(event.logical, LogicalKey::Named(NamedKey::Enter | NamedKey::Tab))
            && plain(mods);
        if !enter || !managed() || self.strip_has_keys(window_id) {
            return false;
        }
        let Some((sid, text)) = self.focused_shown(wi).and_then(|p| Some((p.shelld_session_id()?, box_in(p)?))) else {
            return false;
        };
        if !is_model_command(&text) {
            return false;
        }
        self.notices.guard = Some(Held { window_id, sid, enter: event.clone(), mods });
        // its release reaches nobody, as the release of every key the guard takes
        self.pane_control.shadows.peeks.held.swallowed(&event.logical);
        lx_event!("MODEL_GUARD_HELD", "Enter on /model in a Claude Code box is held for confirmation", session = sid, text = text.as_str());
        self.windows[wi].needs_render = true;
        true
    }

    /// A key while the question is up over the focused pane.
    fn answer_guard(&mut self, event: &MarspotKeyEvent, mods: Modifiers) -> bool {
        if event.state != KeyState::Pressed {
            return true;
        }
        self.pane_control.shadows.peeks.held.swallowed(&event.logical);
        // committed input-method text has no key of its own: its text is the answer
        let yes = (event.logical == LogicalKey::Char('y') && plain(mods))
            || (!(mods.control || mods.alt) && event.text.as_deref().is_some_and(is_yes));
        match event.logical {
            _ if yes => self.confirm_guard(),
            LogicalKey::Named(NamedKey::Escape) => self.drop_guard("escape"),
            _ => {}
        }
        true
    }

    /// Whether the guard holds an Enter for session `sid`.
    pub(super) fn model_guard_holds(&self, sid: u64) -> bool {
        self.notices.guard.as_ref().is_some_and(|h| h.sid == sid)
    }

    /// An input method's composition `text` for window `wi`: whether the
    /// guard took it.  A composition never reaches the guard as a key, so
    /// without this a y typed through an input method left it waiting.
    pub(super) fn model_guard_preedit(&mut self, wi: usize, text: &str) -> bool {
        let window_id = self.windows[wi].window_id;
        if let Some((_, composing)) = self.notices.composed_yes.filter(|c| c.0 == window_id) {
            if text.is_empty() {
                self.notices.composed_yes = Some((window_id, false));
                return true;
            }
            if composing {
                return true;
            }
            // a composition started after the answered one ended is the user's
            self.notices.composed_yes = None;
        }
        let shown = self.focused_shown(wi).and_then(Pane::shelld_session_id);
        let asking = self.notices.guard.as_ref().is_some_and(|h| h.window_id == window_id && Some(h.sid) == shown);
        if !asking || self.strip_has_keys(window_id) {
            return false;
        }
        if !self.windows[wi].ime_preedit.is_empty() {
            self.windows[wi].ime_preedit.clear();
            self.windows[wi].needs_render = true;
        }
        if is_yes(text) {
            self.confirm_guard();
            self.notices.composed_yes = Some((window_id, true));
        }
        true
    }

    fn confirm_guard(&mut self) {
        let Some(held) = self.notices.guard.take() else { return };
        let released = MarspotKeyEvent { state: KeyState::Released, ..held.enter.clone() };
        if let Some(pane) = self.pane_by_sid_mut(held.sid) {
            pane.session_mut().forward_key(&held.enter, held.mods);
            pane.session_mut().forward_key(&released, held.mods);
        }
        self.pending_to_shell.push((MsgType::ModelConfirmed, marspot::shell_proto::encode_model_confirmed(held.sid)));
        lx_event!("MODEL_GUARD_CONFIRMED", "the keyboard confirmed /model; the held Enter was sent", session = held.sid);
        self.windows.iter_mut().for_each(|w| w.needs_render = true);
    }

    fn drop_guard(&mut self, why: &str) {
        let Some(held) = self.notices.guard.take() else { return };
        lx_event!("MODEL_GUARD_DROPPED", "a held /model was not sent", session = held.sid, why = why);
        self.windows.iter_mut().for_each(|w| w.needs_render = true);
    }

    /// Whether a program's input to session `sid` would send `/model`
    /// from Claude Code's box: `typed` goes in as a paste or as typed
    /// text, then an Enter when `enter`.  A refusal is logged and said
    /// over the focused pane.
    pub(super) fn model_guard_refuses(&mut self, sid: u64, typed: &str, paste: bool, enter: bool, from: &str) -> bool {
        if !managed() {
            return false;
        }
        if self.model_guard_holds(sid) {
            lx_event!("MODEL_GUARD_REFUSED", "a program's input reached a box whose /model waits on the keyboard", session = sid, from = from);
            self.show_notice(t(Msg::ModelSendRefused).to_string());
            return true;
        }
        let Some(held) = self.pane_by_sid_mut(sid).and_then(|p| box_in(p)) else { return false };
        let now = Instant::now();
        let unsent = &mut self.notices.unsent;
        unsent.retain(|u| now < u.2);
        let earlier = unsent.iter().position(|u| u.0 == sid).map(|i| unsent.remove(i).1);
        let on_screen = typed_into(&held, false, typed, paste, enter);
        let on_earlier = earlier.map(|e| typed_into(&e.left, e.lost, typed, paste, enter));
        let refuse = on_screen.sends || on_earlier.as_ref().is_some_and(|t| t.sends);
        // the screen may not show the earlier input yet: what it left goes first
        let after = on_earlier.unwrap_or(on_screen);
        if !refuse && (!after.left.is_empty() || after.lost) {
            if unsent.len() == UNSENT_CAP {
                unsent.remove(0);
            }
            unsent.push((sid, after, now + UNSENT_STANDS));
        }
        if refuse {
            lx_event!("MODEL_GUARD_REFUSED", "a program's input would have sent /model to Claude Code", session = sid, from = from);
            self.show_notice(t(Msg::ModelSendRefused).to_string());
        }
        refuse
    }
}

#[cfg(test)]
#[path = "model_guard_tests.rs"]
mod tests;
