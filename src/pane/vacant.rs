//! The inert backend behind a slot with no session running in it.

use std::time::{Duration, Instant};

use crate::grid::Grid;
use crate::ui::strings::{self, t, Arg, Msg};

/// RFC-004 B.2 — the inert backend behind a slot that failed to
/// assemble.  See `PaneBackend::Vacant`.
pub struct VacantPane {
    pub(super) session_id: u64,
    pub(super) grid: Grid,
    /// RFC-006 — a dormant placeholder: the slot a pane left behind
    /// when it was MOVED to another window.  Holds the layout open
    /// but is not a session in any sense: it never revives on a
    /// keystroke (click only), counts as neither live nor
    /// resurrectable, and persists as layout (SavedPane flags bit 0).
    pub(super) dormant: bool,
    /// A spawn for this slot is in flight.
    ///
    /// "Starting" is not a peer state to "vacant" — it *is* a vacant
    /// slot (no live session behind it) that additionally has work
    /// coming.  Modelling it as a flag rather than a second backend
    /// variant keeps that relationship visible, and keeps the twenty
    /// match arms that already handle `Vacant` correct by construction.
    ///
    /// Two things change while it holds: the slot says "starting"
    /// instead of "unavailable", and `is_exited` reports false so the
    /// revive-on-keystroke path can't fire a second spawn on top of the
    /// one already running.
    ///
    /// It is deliberately time-boxed — see `pending_since`.
    pub(super) pending: bool,
    /// When the in-flight spawn started, for the pending state only.
    ///
    /// Suppressing `is_exited` is what stops a double spawn, but it also
    /// means the revive-on-keystroke path — the one affordance a user has
    /// for waking a dead slot — is disabled while it holds.  If the
    /// worker never reports back (its thread panicked, the event was
    /// dropped), the slot would sit at "starting…" forever with the
    /// guard locking out the only escape.  So the guard expires: past
    /// `PENDING_MAX`, the slot reports exited again and a keystroke can
    /// retry.  A late-arriving result is still adopted normally — the
    /// deadline only re-opens the escape hatch, it doesn't cancel
    /// anything.
    pub(super) pending_since: Instant,
}

/// How long a slot may claim "a spawn is in flight" before a keystroke
/// is allowed to start another.  Generously past the 10 s
/// `wait_and_connect` budget a spawn can legitimately take, so a slow
/// boot is never cut short.
pub(super) const PENDING_MAX: Duration = Duration::from_secs(30);

impl VacantPane {
    pub fn new(session_id: u64, cols: u16, rows: u16) -> Self {
        let mut grid = Grid::new(cols.max(20), rows.max(3));
        Self::paint_message(&mut grid, session_id, false);
        Self {
            session_id,
            grid,
            dormant: false,
            pending: false,
            pending_since: Instant::now(),
        }
    }

    /// A slot whose session is being spawned right now.
    pub fn new_pending(session_id: u64, cols: u16, rows: u16) -> Self {
        let mut grid = Grid::new(cols.max(20), rows.max(3));
        Self::paint_message(&mut grid, session_id, true);
        Self {
            session_id,
            grid,
            dormant: false,
            pending: true,
            pending_since: Instant::now(),
        }
    }

    /// RFC-006 — the placeholder a moved-out pane leaves behind.
    pub fn new_dormant(cols: u16, rows: u16) -> Self {
        let mut grid = Grid::new(cols.max(20), rows.max(3));
        let msg = t(Msg::PaneEmpty);
        let row = (grid.rows() / 2).min(grid.rows().saturating_sub(1));
        let start = grid.cols().saturating_sub(crate::pane_message::centring_width(msg)) / 2;
        crate::pane_message::put(&mut grid, start, row, msg);
        Self {
            session_id: 0,
            grid,
            dormant: true,
            pending: false,
            pending_since: Instant::now(),
        }
    }

    pub fn is_dormant(&self) -> bool {
        self.dormant
    }

    /// A spawn is in flight *and* still within its deadline.  Past the
    /// deadline the slot behaves like a plain vacant one so the user can
    /// retry — see `pending_since`.
    pub fn is_pending(&self) -> bool {
        self.pending && self.pending_since.elapsed() < PENDING_MAX
    }

    fn paint_message(grid: &mut Grid, session_id: u64, pending: bool) {
        let msg = strings::text(if pending {
            Arg::SessionStarting(session_id)
        } else {
            Arg::SessionUnavailable(session_id)
        });
        let row = 1u16.min(grid.rows().saturating_sub(1));
        crate::pane_message::put(grid, 2, row, &msg);
    }

    pub(super) fn resize(&mut self, cols: u16, rows: u16) {
        let mut grid = Grid::new(cols.max(20), rows.max(3));
        Self::paint_message(&mut grid, self.session_id, self.pending);
        self.grid = grid;
    }

    pub(super) fn repaint(&mut self) {
        let (cols, rows) = (self.grid.cols(), self.grid.rows());
        if self.dormant { self.grid = Self::new_dormant(cols, rows).grid } else { self.resize(cols, rows) }
    }
}
