//! A pane showing another session in its place.

use super::{Pane, PaneBackend, VacantPane};
use crate::grid::Grid;
use std::time::Instant;

/// What a pane shows instead of its own session.
pub enum Peek {
    /// The shadow pane of this session.
    Live(u64),
    /// The last frame of a shadow pane that has gone, until the user
    /// leaves it.
    Ended { last: Box<Pane>, name: String },
}

impl Pane {
    /// The session shown in this pane's place, while there is one.
    pub fn peeked(&self) -> Option<u64> {
        match self.peek {
            Some(Peek::Live(sid)) => Some(sid),
            _ => None,
        }
    }

    /// Take the frame last read off this pane, as a pane that shows it
    /// and nothing else.  The next `pump` reads this pane's frame anew.
    pub fn keep_last_frame(&mut self) -> Pane {
        let session_id = self.shelld_session_id().unwrap_or(0);
        let (cols, rows) = (self.session.grid().cols(), self.session.grid().rows());
        let blank = Grid::new(cols, rows);
        let grid = match &mut self.session {
            PaneBackend::L3(c) => {
                c.last_seq = 0;
                std::mem::replace(&mut c.grid, blank)
            }
            PaneBackend::Vacant(v) => std::mem::replace(&mut v.grid, blank),
            PaneBackend::Local(_) | PaneBackend::Web(_) => blank,
        };
        Pane::over(PaneBackend::Vacant(VacantPane {
            session_id,
            grid,
            dormant: false,
            pending: false,
            pending_since: Instant::now(),
        }))
    }

    /// What the slot this pane holds looks like whoever it shows: how
    /// far it has receded, and its dim.
    pub fn slot_look(&self) -> (u32, f32) {
        (self.recede, self.scrim.value(Instant::now()))
    }
}
