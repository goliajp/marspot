//! Dev seam (`MARSPOT_DEV_HELPER_STRIP=n`): a strip of n rows with made
//! up text under every pane.

use super::*;
use super::compose::{compose, Arrow, List, Pick, Row, State};
use super::theme::Theme;
use marspot_term::grid::Grid;

impl Strips {
    pub(in super::super) fn from_env() -> Self {
        let dev = std::env::var("MARSPOT_DEV_HELPER_STRIP").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0);
        let dump = std::env::var_os("MARSPOT_DEV_STRIP_DUMP").is_some();
        Self { dev, dump, on: marspot::settings::get().helper_strip, born: Some(Instant::now()), ..Self::default() }
    }

    pub(super) fn dev_ask(&mut self, panes: &[Pane]) {
        let Some(rows) = self.dev else { return };
        for sid in panes.iter().filter(|p| !p.is_dormant()).filter_map(|p| p.shelld_session_id()) {
            self.set_rows(sid, rows);
        }
    }
}

/// Log what a strip holds, when that is not what `last` was (dev seam).
pub(super) fn dump(strip: &Strip, sid: u64, last: &mut u64) {
    let Some(v) = strip.view().filter(|v| v.seq != *last) else { return };
    *last = v.seq;
    let line = |r| (0..v.grid.cols()).map(|c| v.grid.cell(c, r).ch).filter(|c| *c != '\0').collect::<String>();
    // a value of the log has a bounded length: a gap between columns is
    // kept as two spaces, one between words as one
    let words = |l: String| l.split("  ").map(str::trim).filter(|w| !w.is_empty()).collect::<Vec<_>>().join("  ");
    let lines: Vec<String> = (0..v.grid.rows()).map(|r| words(line(r))).collect();
    lx_event!("DEV_STRIP", "what a strip holds", session = sid, top = v.top_row, lines = lines.join("|"));
}

const ROW: Row = Row {
    name: "helper",
    named: false,
    task: "a task of some length",
    state: State::Running,
    elapsed_ms: 92_000,
    tokens: Some(115_300),
    arrow: Arrow::Down,
};

const ROWS: [Row; 6] = [
    ROW,
    Row { name: "reviewer", named: true, tokens: None, arrow: Arrow::Up, ..ROW },
    Row { state: State::Awaiting, ..ROW },
    Row { state: State::Done, ..ROW },
    Row { state: State::Failed, ..ROW },
    Row { state: State::Stopped, ..ROW },
];

/// The made up list, going on from the agent's own on `screen` if it
/// draws one.
pub(super) fn fill(strip: &mut Strip, screen: &Grid) {
    let light = marspot_term::palette::current_id() == marspot_term::palette::ThemeId::Light;
    let native = native_list::find(screen);
    compose(
        strip,
        &List {
            rows: &ROWS,
            selected: native.as_ref().filter(|n| n.on_last).map(|_| Pick::Row(0)),
            viewed: Pick::Main,
            theme: Theme::of_screen(screen).unwrap_or_else(|| Theme::for_paper(light)),
            native: native.and_then(|n| n.columns),
        },
    );
}

impl CoreApp {
    /// Dev seam (`MARSPOT_DEV_PANELS`): the middle of the first line of
    /// the first pane's strip and the height of a line, for a script to
    /// click.
    pub(in super::super) fn dev_strip_spot(&self, wi: usize) -> Option<(f64, f64, f64)> {
        let w = self.windows.get(wi)?;
        let top = self.strips.top_row(w.panes.first()?.shelld_session_id())?;
        let c = w.layout.cells.first()?;
        let (cw, ch) = self.renderer.cell_dims();
        let g = marspot::layout::cell_grid(c, w.layout.cell_title_h, w.layout.padding, cw, ch);
        Some((g.x + 3.5 * g.w, g.row_top(top) + g.h * 0.5, g.h))
    }
}
