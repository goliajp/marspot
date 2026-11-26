//! Claude Code's `/model` writes the default model of every account
//! that shares the settings file.  While `claude.model` is managed:
//!
//! * Enter on a `/model` in a Claude Code input box is held, and a
//!   notice over the pane says what it does.  `y` sends the held Enter
//!   and tells the shell, which then keeps the settings file's next
//!   change of model; Esc drops it and leaves the box as it was.  Any
//!   other key is swallowed while the question is up.
//! * Input from programs (typing, pastes, injected bytes) that would
//!   send `/model` from such a box is refused: only the keyboard can
//!   confirm.
//!
//! The check runs only on Enter, and reads cells until it finds Claude
//! Code's prompt; a key that is not Enter costs one comparison.  It
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
use marspot::pane_read::slash_model::{claude_box_text, claude_prompt_row, is_model_command, sends_model};
use marspot::settings::CcModel;
use marspot::ui::strings::{t, Msg};

/// How long a paste that leaves `/model` in a box stands for what the
/// box holds, while the screen has yet to show it.
const PASTE_STANDS: Duration = Duration::from_secs(3);

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

/// What Claude Code's box in `pane` holds, when `pane` shows one live.
fn box_in(pane: &Pane) -> Option<String> {
    let grid = pane.session().grid();
    (pane.is_l3() && claude_prompt_row(grid).is_some()).then(|| claude_box_text(grid))?
}

impl CoreApp {
    /// A key for the focused slot of window `wi`: whether the guard took it.
    pub(super) fn model_guard_key(&mut self, wi: usize, event: &MarspotKeyEvent, mods: Modifiers) -> bool {
        let window_id = self.windows[wi].window_id;
        let shown = self.focused_shown(wi).and_then(Pane::shelld_session_id);
        if let Some(held) = self.notices.guard.as_ref().filter(|h| h.window_id == window_id) {
            if Some(held.sid) == shown && !self.strip_has_keys(window_id) {
                // the window's own shortcuts still work while it asks
                return !mods.super_ && self.answer_guard(event, mods);
            }
            // focus moved away from the question: it is dropped, as Esc would
            self.drop_guard("focus_moved");
        }
        let enter = event.state == KeyState::Pressed
            && !event.repeat
            && event.logical == LogicalKey::Named(NamedKey::Enter)
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
        match event.logical {
            LogicalKey::Char('y') if plain(mods) => self.confirm_guard(),
            LogicalKey::Named(NamedKey::Escape) => self.drop_guard("escape"),
            _ => {}
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
        let Some(held) = self.pane_by_sid_mut(sid).and_then(|p| box_in(p)) else { return false };
        let now = Instant::now();
        let pasted = self.notices.pasted.is_some_and(|(s, until)| s == sid && now < until);
        let sends_first = typed.split(['\r', '\n']).next() == Some("") && (enter || (!paste && submits(typed.as_bytes())));
        let refuse = sends_model(&held, typed, paste, enter) || (pasted && sends_first);
        if paste && !enter && sends_model(&held, typed, true, true) {
            self.notices.pasted = Some((sid, now + PASTE_STANDS));
        }
        if refuse {
            self.notices.pasted = None;
            lx_event!("MODEL_GUARD_REFUSED", "a program's input would have sent /model to Claude Code", session = sid, from = from);
            self.show_notice(t(Msg::ModelSendRefused).to_string());
        }
        refuse
    }
}

/// Whether `bytes`, written to a pane's terminal as they are, end a
/// line.
fn submits(bytes: &[u8]) -> bool {
    bytes.iter().any(|b| matches!(b, b'\r' | b'\n'))
}

#[cfg(test)]
#[path = "model_guard_tests.rs"]
mod tests;
