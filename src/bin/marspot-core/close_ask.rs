//! Asked before a pane with child panes closes: the children end with
//! it, so the question names them.  It stands in the window the close
//! was asked from, over the pane when that window shows it and over
//! the pane in use there when it does not.  Enter or its first button
//! closes the pane; Esc, its second button or a press anywhere else
//! leaves everything as it was.  While it is up it takes every key of
//! its window, and only of its window.  It comes down unanswered when
//! the pane closing or the pane it stands over ends or leaves that
//! window, or the window closes.

use marspot::input::{KeyState, LogicalKey, NamedKey};
use marspot::ui::components::pane_notice::{self, PaneNoticeParams};

use super::*;

/// Children named in the question at most; past it a line says more.
const NAMED_CAP: usize = 3;

/// The question up.
pub(crate) struct CloseAsk {
    /// The pane to close.
    pub(crate) sid: u64,
    /// The pane it is shown over.
    pub(crate) over: u64,
    /// The window it is shown in, by id: indices move as windows close.
    pub(crate) window: u32,
    pub(crate) text: String,
}

impl CoreApp {
    /// Pane `sid` is to close from window `wi`: true when it has child
    /// panes and the question went up instead.
    pub(crate) fn close_asked(&mut self, wi: usize, sid: u64) -> bool {
        if !self.pane_control.shadows.family.counted() {
            self.shadows_changed();
        }
        let mut children: Vec<u64> =
            self.pane_control.shadows.family.parents.iter().filter(|(_, p)| **p == sid).map(|(c, _)| *c).collect();
        if children.is_empty() {
            return false;
        }
        let Some(over) = self.ask_place(wi, sid) else {
            // a window that shows no pane has nowhere to ask; the close
            // does not happen unasked
            lx_warn!("CLOSE_ASK_NOWHERE", "a pane with child panes was to close from a window that shows no pane",
                session = sid);
            return true;
        };
        children.sort_unstable();
        let names = self.pane_names();
        let mut body = t(Msg::CloseChildrenAsk).to_string();
        for c in children.iter().take(NAMED_CAP) {
            body.push('\n');
            body.push_str(&self.coded_name(*c, &names));
        }
        if children.len() > NAMED_CAP {
            body.push('\n');
            body.push_str(&strings::text(Arg::MoreChildren(children.len() - NAMED_CAP)));
        }
        let text = pane_notice::ask_text(&body, t(Msg::CloseChildrenConfirm), t(Msg::CloseChildrenCancel));
        let window = win!(self, wi).window_id;
        self.notices.close = Some(CloseAsk { sid, over, window, text });
        win!(self, wi).needs_render = true;
        lx_event!("CLOSE_ASKED", "asked before closing a pane with child panes", session = sid,
            children = children.len() as u32, window = window);
        true
    }

    /// The pane in window `wi` the question about closing `sid` stands
    /// over: `sid` when the window shows it, else the pane in use, else
    /// any pane it shows.
    fn ask_place(&self, wi: usize, sid: u64) -> Option<u64> {
        if self.slot_in(wi, sid).is_some() {
            return Some(sid);
        }
        let w = self.windows.get(wi)?;
        let focused = self.focused_session(wi).filter(|s| self.slot_in(wi, *s).is_some());
        focused.or_else(|| (0..w.panes.len()).find_map(|i| self.shown(wi, i).shelld_session_id()))
    }

    /// The slot of window `wi` that shows `sid`.
    fn slot_in(&self, wi: usize, sid: u64) -> Option<usize> {
        let (w, idx) = self.slot_showing(sid).filter(|(w, _)| *w == wi)?;
        (self.shown(w, idx).shelld_session_id() == Some(sid)).then_some(idx)
    }

    /// The window the question stands in, while everything it is about
    /// is still there: the pane closing, and the pane it stands over,
    /// shown in that window.
    fn close_ask_window(&self) -> Option<usize> {
        let ask = self.notices.close.as_ref()?;
        let wi = self.windows.iter().position(|w| w.window_id == ask.window)?;
        self.pane_of(ask.sid)?;
        self.slot_in(wi, ask.over).map(|_| wi)
    }

    /// Takes down a question whose pane or window went away; true when
    /// it was up and stands.
    fn close_ask_stands(&mut self) -> bool {
        if self.notices.close.is_none() {
            return false;
        }
        if self.close_ask_window().is_some() {
            return true;
        }
        let Some(ask) = self.notices.close.take() else { return false };
        self.windows.iter_mut().for_each(|w| w.needs_render = true);
        lx_event!("CLOSE_ASK_GONE", "the question before closing a pane came down: its pane or window went away",
            session = ask.sid, over = ask.over, window = ask.window);
        false
    }

    /// Once per loop turn.
    pub(crate) fn close_ask_tick(&mut self) {
        self.close_ask_stands();
    }

    /// The question's answer: close the pane, or leave it.
    fn close_answered(&mut self, go: bool) {
        let Some(ask) = self.notices.close.take() else { return };
        self.windows.iter_mut().for_each(|w| w.needs_render = true);
        lx_event!("CLOSE_ANSWERED", "the question before closing a pane with child panes was answered",
            session = ask.sid, close = go as u32);
        if go {
            self.close_anywhere(ask.sid, false);
        }
    }

    /// A key in window `wi` while the question is up there: Enter
    /// closes, Esc does not, any other does nothing.  Whether it took
    /// the key.
    pub(crate) fn close_ask_key(&mut self, wi: usize, event: &MarspotKeyEvent) -> bool {
        if !self.close_ask_stands() || self.close_ask_window() != Some(wi) {
            return false;
        }
        if event.state != KeyState::Pressed {
            return true;
        }
        self.pane_control.shadows.peeks.held.swallowed(&event.logical);
        match event.logical {
            LogicalKey::Named(NamedKey::Enter) => self.close_answered(true),
            LogicalKey::Named(NamedKey::Escape) => self.close_answered(false),
            _ => {}
        }
        true
    }

    /// A press in window `wi` while the question is up: on its buttons
    /// it answers and takes the press; anywhere else it leaves the pane
    /// open and the press goes on.
    pub(crate) fn close_ask_press(&mut self, wi: usize, x: f64, y: f64) -> bool {
        if !self.close_ask_stands() {
            return false;
        }
        let Some(over) = self.notices.close.as_ref().map(|a| a.over) else { return false };
        let hit = self.close_ask_buttons(wi, over).and_then(|b| b.iter().position(|r| r.contains(x, y)));
        self.close_answered(hit == Some(0));
        hit.is_some()
    }

    /// Where the question's two buttons are in window `wi`, if it is
    /// shown there.
    fn close_ask_buttons(&self, wi: usize, over: u64) -> Option<[marspot_term::layout::Rect; 2]> {
        if self.close_ask_window() != Some(wi) {
            return None;
        }
        let idx = self.slot_in(wi, over)?;
        let l = &win!(self, wi).layout;
        let cell = l.cells.get(idx)?;
        let grid = self.shown(wi, idx).session().grid();
        let text = self.notices.close.as_ref()?.text.as_str();
        let params = PaneNoticeParams {
            text,
            inner_x: (cell.x + l.padding) as f32,
            inner_y: (cell.y_top + l.cell_title_h + l.padding) as f32,
            grid_cols: grid.cols(),
            grid_rows: grid.rows(),
        };
        let (cw, ch) = self.renderer.cell_dims();
        let laid = pane_notice::layout(&params, (cw as f32, ch as f32))?;
        laid.buttons.map(|[(go, _), (stay, _)]| [go, stay])
    }
}

#[cfg(test)]
#[path = "close_ask_tests.rs"]
mod tests;
