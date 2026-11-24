//! Dev seam (`MARSPOT_DEV_HELPER_STRIP=n`): a strip of n rows with made
//! up text under every pane.

use super::*;
use marspot_term::grid::CellAttrs;

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

pub(super) fn fill(strip: &mut Strip) {
    let plain = CellAttrs::default();
    let dim = CellAttrs { dim: true, ..plain };
    strip.line(1, &[Run { col: 2, text: "● main", attrs: plain }]);
    for row in 2..strip.rows() {
        strip.line(
            row,
            &[
                Run { col: 2, text: "○ helper", attrs: dim },
                Run { col: 13, text: "a task of some length", attrs: plain },
                Run { col: 37, text: "1m 32s · ↓ 115.3k tokens", attrs: dim },
            ],
        );
    }
}
