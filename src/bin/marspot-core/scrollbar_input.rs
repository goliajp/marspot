//! Pressing and dragging a pane's scrollbar: a press on it moves the
//! view to where the thumb lands, and a drag keeps moving it until the
//! button comes up.

use marspot::ui::components::scrollbar::{Scrollbar, ScrollbarGrab};

use super::*;

/// A press holding pane `idx`'s scrollbar.
#[derive(Clone, Copy)]
pub(super) struct ScrollbarDrag {
    wi: usize,
    idx: usize,
    grab: ScrollbarGrab,
}

impl ScrollbarDrag {
    pub(super) fn is_of(&self, wi: usize, idx: usize) -> bool {
        (self.wi, self.idx) == (wi, idx)
    }
}

impl CoreApp {
    fn scrollbar_of(&self, wi: usize, idx: usize, held: bool) -> Option<Scrollbar> {
        let w = &win!(self, wi);
        let pane = self.shown_pane(wi, idx)?;
        let cell = w.layout.cells.get(idx)?;
        let rows = pane.session().grid().rows();
        let history = pane.history_view(idx == w.focused_idx);
        if held {
            Scrollbar::held(cell, w.layout.cell_title_h, rows, history)
        } else {
            Scrollbar::of(cell, w.layout.cell_title_h, rows, history)
        }
    }

    /// Takes a press that lands on a pane's scrollbar, and says whether
    /// it did.
    pub(super) fn scrollbar_press(&mut self, wi: usize, x_phys: f64, y_phys: f64) -> bool {
        let Some(idx) = win!(self, wi).layout.hit_test(x_phys, y_phys) else { return false };
        let Some(bar) = self.scrollbar_of(wi, idx, false) else { return false };
        let Some(grab) = bar.press(x_phys, y_phys) else { return false };
        self.set_focus(wi, idx);
        self.scrollbar_drag = Some(ScrollbarDrag { wi, idx, grab });
        self.scroll_history_to(wi, idx, bar.offset_at(grab, y_phys));
        true
    }

    /// Follows a drag of the scrollbar, and says whether one is held.
    pub(super) fn scrollbar_drag_to(&mut self, y_phys: f64) -> bool {
        let Some(ScrollbarDrag { wi, idx, grab }) = self.scrollbar_drag else { return false };
        if let Some(bar) = self.scrollbar_of(wi, idx, true) {
            self.scroll_history_to(wi, idx, bar.offset_at(grab, y_phys));
        }
        true
    }

    /// Lets go of the scrollbar, and says whether it was held.
    pub(super) fn scrollbar_release(&mut self) -> bool {
        self.scrollbar_drag.take().is_some()
    }

    fn scroll_history_to(&mut self, wi: usize, idx: usize, offset: u16) {
        let Some(pane) = self.shown_pane_mut(wi, idx) else { return };
        let delta = offset as i32 - pane.view_offset() as i32;
        if pane.scroll_own_view(delta) != 0 {
            win!(self, wi).needs_render = true;
        }
    }
}
