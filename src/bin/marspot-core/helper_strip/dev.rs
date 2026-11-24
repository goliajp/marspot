//! Dev seam (`MARSPOT_DEV_HELPER_STRIP=n`): a strip of n rows with made
//! up text under every pane.

use super::*;
use super::compose::{compose, Arrow, List, Pick, Row, State};
use super::theme::Theme;
use marspot_term::grid::Grid;

impl Strips {
    pub(in super::super) fn from_env() -> Self {
        let dev = std::env::var("MARSPOT_DEV_HELPER_STRIP").ok().and_then(|v| v.parse().ok()).filter(|n| *n > 0);
        Self { dev, ..Self::default() }
    }

    pub(super) fn dev_ask(&mut self, panes: &[Pane]) {
        let Some(rows) = self.dev else { return };
        for sid in panes.iter().filter(|p| !p.is_dormant()).filter_map(|p| p.shelld_session_id()) {
            self.set_rows(sid, rows);
        }
    }
}

const ROW: Row = Row {
    name: "helper",
    named: false,
    task: "a task of some length",
    state: State::Running,
    elapsed_ms: 92_000,
    tokens: Some(115_300),
    arrow: Arrow::Down,
    gone: false,
};

const ROWS: [Row; 6] = [
    ROW,
    Row { name: "reviewer", named: true, tokens: None, arrow: Arrow::Up, ..ROW },
    Row { state: State::Awaiting, ..ROW },
    Row { state: State::Done, gone: true, ..ROW },
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
