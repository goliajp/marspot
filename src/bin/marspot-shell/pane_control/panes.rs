//! What the connection threads know about panes beyond the observer's
//! snapshot: the state the main loop publishes on change, where each
//! pane sits in the windows (read from the layout L2 saves), and which
//! sessions are shadow panes.

use std::collections::HashMap;
use std::time::SystemTime;

use marspot_term::session_registry::SessionEntry;

/// One pane as the main loop last saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PaneView {
    /// The state machine's label (`awaiting_user`, `busy`, …).
    pub state: String,
    /// A multi-step script running on it, by name.
    pub busy: Option<&'static str>,
    /// Its agent was taken down idle and waits for the user.
    pub dormant: bool,
}

pub(crate) type PaneViews = HashMap<u64, PaneView>;

/// The parent of a shadow pane, 0 for a pane that is not one.
pub(super) fn shadow_of(entry: &SessionEntry) -> u64 {
    entry.shadow_of
}

/// `sid → (window, x, y)`, 1-based, from the layout L2 persists, read
/// again only when the file changed.
pub(super) struct Cells {
    stamp: Option<SystemTime>,
    cells: HashMap<u64, (usize, usize, usize)>,
}

static CELLS: std::sync::LazyLock<std::sync::Mutex<Cells>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Cells { stamp: None, cells: HashMap::new() }));

pub(super) fn cells() -> HashMap<u64, (usize, usize, usize)> {
    let path = marspot::state::state_file_path();
    let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut c = CELLS.lock().unwrap_or_else(|p| p.into_inner());
    if stamp != c.stamp || stamp.is_none() {
        c.stamp = stamp;
        c.cells = super::names::layout_cells();
    }
    c.cells.clone()
}
