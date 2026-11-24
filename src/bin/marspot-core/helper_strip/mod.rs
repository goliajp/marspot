//! The rows a pane gives up at its bottom, and what is drawn in them.
//!
//! A strip belongs to a session, so it goes with its pane from window
//! to window.  The layout takes its rows from the pane's terminal
//! (`Layout::with_strips`); drawing and the pointer read where it is
//! from the same place.

use super::*;
use marspot::layout::CellRect;
use marspot::render::StripView;

mod dev;
mod strip;

pub(super) use strip::{Run, Strip, MAX_ROWS};

#[derive(Default)]
pub(super) struct Strips {
    by_sid: std::collections::HashMap<u64, Strip>,
    /// Dev seam only (`MARSPOT_DEV_HELPER_STRIP=n`): every pane gets a
    /// strip of n rows.
    dev: Option<u16>,
}

impl Strips {
    /// No pane has a strip.
    pub(super) fn is_empty(&self) -> bool {
        self.by_sid.is_empty() && self.dev.is_none()
    }

    /// Ask for `rows` rows under session `sid`'s terminal, none for 0.
    /// The caller rebuilds the layout of the window it is in.
    pub(super) fn set_rows(&mut self, sid: u64, rows: u16) {
        let rows = rows.min(MAX_ROWS);
        if rows == 0 {
            self.by_sid.remove(&sid);
            return;
        }
        self.by_sid.entry(sid).or_insert_with(|| Strip::new(rows)).set_rows(rows);
    }

    pub(super) fn forget(&mut self, sid: u64) {
        self.by_sid.remove(&sid);
    }

    pub(super) fn view(&self, sid: Option<u64>) -> Option<StripView<'_>> {
        self.by_sid.get(&sid?)?.view()
    }

    fn top_row(&self, sid: Option<u64>) -> Option<u16> {
        self.by_sid.get(&sid?)?.top_row()
    }

    fn rows(&self, sid: Option<u64>) -> u16 {
        sid.and_then(|sid| self.by_sid.get(&sid)).map_or(0, Strip::rows)
    }

    /// Give every strip of a window's panes its size in the layout.
    /// One the layout has no rows for is not drawn.
    fn fit(&mut self, panes: &[Pane], cells: &[CellRect]) {
        for (i, pane) in panes.iter().enumerate() {
            let Some(strip) = pane.shelld_session_id().and_then(|sid| self.by_sid.get_mut(&sid)) else { continue };
            match cells.get(i).filter(|c| c.strip_rows > 0) {
                Some(c) => {
                    if strip.fit(c.cols, c.strip_rows) && self.dev.is_some() {
                        dev::fill(strip);
                    }
                }
                None => strip.hide(),
            }
        }
    }
}

impl CoreApp {
    /// `layout` with the rows window `wi`'s panes give up taken out.
    pub(super) fn take_strip_rows(&mut self, wi: usize, layout: Layout) -> Layout {
        if self.strips.is_empty() {
            return layout;
        }
        let w = &self.windows[wi];
        self.strips.dev_ask(&w.panes);
        let wanted: Vec<u16> = w.panes.iter().map(|p| self.strips.rows(p.shelld_session_id())).collect();
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let layout = layout.with_strips(&wanted, cell_w, cell_h);
        self.strips.fit(&w.panes, &layout.cells);
        layout
    }

    /// Work out where the strips of window `wi` are drawn this frame.
    pub(super) fn place_strips(&mut self, wi: usize) {
        if self.strips.is_empty() {
            return;
        }
        let w = &self.windows[wi];
        for (pane, cell) in w.panes.iter().zip(&w.layout.cells).filter(|(_, c)| c.strip_rows > 0) {
            let Some(sid) = pane.shelld_session_id() else { continue };
            let Some(strip) = self.strips.by_sid.get_mut(&sid) else { continue };
            let screen = pane.session().grid();
            strip.place(screen, sid, pane.grid_seq(), pane.view_offset() != 0, cell.rows);
        }
    }

    /// The pane whose strip is at a point of window `wi`, and the row
    /// of the strip.
    pub(super) fn strip_at(&self, wi: usize, x_phys: f64, y_phys: f64) -> Option<(usize, u16)> {
        if self.strips.is_empty() {
            return None;
        }
        let w = &win!(self, wi);
        let (cw, ch) = self.renderer.cell_dims();
        w.layout.hit_test_strip(x_phys, y_phys, cw, ch, |i| {
            self.strips.top_row(w.panes.get(i).and_then(|p| p.shelld_session_id()))
        })
    }

    /// The pane, column and row of a terminal's cell at a point of
    /// window `wi`.  A strip is no terminal's, wherever it is drawn.
    pub(super) fn cell_pos_at(&self, wi: usize, x_phys: f64, y_phys: f64) -> Option<(usize, u16, u16)> {
        if self.strip_at(wi, x_phys, y_phys).is_some() {
            return None;
        }
        let (cw, ch) = self.renderer.cell_dims();
        win!(self, wi).layout.hit_test_cell_pos(x_phys, y_phys, cw, ch)
    }

    /// Whether a press landed on a pane's strip.  It focuses the pane,
    /// and starts no selection.
    pub(super) fn strip_press(&mut self, wi: usize, x_phys: f64, y_phys: f64) -> bool {
        let Some((idx, _)) = self.strip_at(wi, x_phys, y_phys) else { return false };
        self.focus_pane(wi, idx);
        true
    }
}

#[cfg(test)]
mod tests;
