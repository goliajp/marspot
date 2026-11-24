//! The rows a pane gives up at its bottom, and what is drawn in them.
//!
//! A strip belongs to a session, so it goes with its pane from window
//! to window.  The layout takes its rows from the pane's terminal
//! (`Layout::with_strips`); drawing and the pointer read where it is
//! from the same place.

use super::*;
use marspot::frame_build::text_run::preedit_last_row;
use marspot::layout::CellRect;
use marspot::render::StripView;

mod compose;
mod dev;
mod figures;
mod native_list;
mod strip;
mod theme;

pub(super) use strip::{Over, Strip, MAX_ROWS};

#[derive(Default)]
pub(super) struct Strips {
    by_sid: std::collections::HashMap<u64, Strip>,
    /// Dev seam only (`MARSPOT_DEV_HELPER_STRIP=n`): every pane gets a
    /// strip of n rows.
    dev: Option<u16>,
    /// Where the pointer last was, and in which window.
    pointer: Option<(u32, f64, f64)>,
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

    /// How long until a strip has to move.
    pub(super) fn wait(&self, now: Instant) -> Option<Duration> {
        self.by_sid.values().filter_map(Strip::due).min().map(|due| due.saturating_duration_since(now))
    }

    pub(super) fn pointer_at(&mut self, window_id: u32, x_phys: f64, y_phys: f64) {
        self.pointer = Some((window_id, x_phys, y_phys));
    }

    /// Give every strip of a window's panes its size in the layout.
    /// One the layout has no rows for is not drawn.
    fn fit(&mut self, panes: &[Pane], cells: &[CellRect]) {
        for (i, pane) in panes.iter().enumerate() {
            let Some(strip) = pane.shelld_session_id().and_then(|sid| self.by_sid.get_mut(&sid)) else { continue };
            match cells.get(i).filter(|c| c.strip_rows > 0) {
                Some(c) => {
                    strip.fit(c.cols, c.strip_rows);
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
        let now = Instant::now();
        let w = &self.windows[wi];
        let composing = !w.panels.owns_pointer();
        for (i, (pane, cell)) in w.panes.iter().zip(&w.layout.cells).enumerate().filter(|(_, (_, c))| c.strip_rows > 0) {
            let Some(sid) = pane.shelld_session_id() else { continue };
            let Some(strip) = self.strips.by_sid.get_mut(&sid) else { continue };
            let pane = shown::shown_of(pane, &self.shadows);
            let screen = pane.session().grid();
            if self.strips.dev.is_some() {
                dev::fill(strip, screen);
            }
            strip.place(Over {
                screen,
                shown: pane.shelld_session_id().unwrap_or(sid),
                seq: pane.grid_seq(),
                scrolled: pane.view_offset() != 0,
                rows: cell.rows,
                preedit_end: (composing && i == w.focused_idx).then(|| preedit_last_row(&w.ime_preedit, screen)).flatten(),
                now,
            });
        }
    }

    /// Ask for the frames in which strips move.
    pub(super) fn strips_tick(&mut self) {
        let now = Instant::now();
        for (sid, strip) in self.strips.by_sid.iter_mut() {
            if strip.ripen(now)
                && let Some(w) = self.windows.iter_mut().find(|w| w.panes.iter().any(|p| p.shelld_session_id() == Some(*sid)))
            {
                w.needs_render = true;
            }
        }
    }

    /// The last row a selection in pane `idx` can reach: the one above
    /// its strip, where that is drawn over the live screen.
    pub(super) fn selection_floor(&self, wi: usize, idx: usize, last: u16) -> u16 {
        if self.shown_pane(wi, idx).is_none_or(|p| p.view_offset() != 0) {
            return last;
        }
        let top = self.strips.top_row(self.windows[wi].panes[idx].shelld_session_id());
        top.map_or(last, |t| last.min(t.saturating_sub(1)))
    }

    /// Whether the pointer is on a strip and a wheel tick would go to
    /// the program in pane `idx`: a strip is no program's.
    pub(super) fn strip_takes_wheel(&self, wi: usize, idx: usize) -> bool {
        let Some((window, x, y)) = self.strips.pointer else { return false };
        let w = &self.windows[wi];
        window == w.window_id
            && self.strip_at(wi, x, y).is_some()
            && self.shown_pane(wi, idx).is_some_and(|p| {
                p.program_owns_scroll() || p.shelld_session_id().is_some_and(|sid| self.pane_wheel_keys.contains_key(&sid))
            })
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
    /// and drops the selection as a press off one does.
    pub(super) fn strip_press(&mut self, wi: usize, x_phys: f64, y_phys: f64) -> bool {
        let Some((idx, _)) = self.strip_at(wi, x_phys, y_phys) else { return false };
        let w = &mut win!(self, wi);
        w.selection_dragging = false;
        w.needs_render |= w.selection.take().is_some();
        self.focus_pane(wi, idx);
        true
    }
}

#[cfg(test)]
mod alloc_tests;
#[cfg(test)]
mod compose_tests;
#[cfg(test)]
mod row_tests;
#[cfg(test)]
mod moving_tests;
#[cfg(test)]
mod tests;
