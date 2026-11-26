//! Hard constraints: candidates that may not be taken are removed
//! before any term is compared, never priced out with a huge cost.

/// What a pane is doing when it is decided about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaneNow {
    /// Working on a turn.
    pub busy: bool,
    /// Its account refused it.
    pub refused: bool,
}

/// Drops every candidate the pane's state forbids: a busy pane that was
/// not refused is not interrupted, so only candidates for which
/// `interrupts` is false stay (doing nothing and telling it).
pub fn keep_feasible<A, T>(cands: &mut Vec<(A, T)>, pane: PaneNow, interrupts: impl Fn(&A) -> bool) {
    if pane.busy && !pane.refused {
        cands.retain(|(a, _)| !interrupts(a));
    }
}
