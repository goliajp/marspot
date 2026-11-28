//! Asked before a pane with child panes closes: the children end with
//! it, so the question names them, over the pane when it is on screen
//! and over the pane in use when it is not.  Enter or its first button
//! closes the pane; Esc, its second button or a press anywhere else
//! leaves everything as it was.  While it is up it takes every key.

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
        children.sort_unstable();
        let names = self.pane_names();
        let mut body = t(Msg::CloseChildrenAsk).to_string();
        for c in children.iter().take(NAMED_CAP) {
            body.push('\n');
            body.push_str(&self.coded_name(*c, &names));
        }
        if children.len() > NAMED_CAP {
            body.push_str("\n…");
        }
        let text = pane_notice::ask_text(&body, t(Msg::CloseChildrenConfirm), t(Msg::CloseChildrenCancel));
        let over = if self.slot_showing(sid).is_some() { sid } else { self.focused_session(wi).unwrap_or(sid) };
        self.notices.close = Some(CloseAsk { sid, over, text });
        self.windows.iter_mut().for_each(|w| w.needs_render = true);
        lx_event!("CLOSE_ASKED", "asked before closing a pane with child panes", session = sid,
            children = children.len() as u32);
        true
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

    /// A key while the question is up: Enter closes, Esc does not, any
    /// other does nothing.  Whether it took the key.
    pub(crate) fn close_ask_key(&mut self, event: &MarspotKeyEvent) -> bool {
        if self.notices.close.is_none() {
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
        let Some(over) = self.notices.close.as_ref().map(|a| a.over) else { return false };
        let hit = self.close_ask_buttons(wi, over).and_then(|b| b.iter().position(|r| r.contains(x, y)));
        self.close_answered(hit == Some(0));
        hit.is_some()
    }

    /// Where the question's two buttons are in window `wi`, if it is
    /// shown there.
    fn close_ask_buttons(&self, wi: usize, over: u64) -> Option<[marspot_term::layout::Rect; 2]> {
        let (w, idx) = self.slot_showing(over).filter(|(w, _)| *w == wi)?;
        let l = &win!(self, w).layout;
        let cell = l.cells.get(idx)?;
        let grid = self.shown(w, idx).session().grid();
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
