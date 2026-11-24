//! The selection that follows a program's own scrolling.

use super::*;

impl CoreApp {
    /// Keep the selection tape in step with the picture, and the
    /// selection in step with the tape.
    ///
    /// Called once per pump.  The tape's lifetime hangs off `selection`
    /// here rather than off the many places that clear one: a tape
    /// whose selection is gone, or whose pane the selection has moved
    /// away from, is dropped on the spot.
    ///
    /// While it lives, each frame is folded in (a shift, or a break if
    /// the program redrew rather than scrolled) and the selection's
    /// ends are re-expressed in the coordinates the renderer uses.  An
    /// end that has scrolled off an edge saturates there, which the
    /// renderer already draws as "carries on past this edge".
    pub(super) fn sync_selection_tape(&mut self, wi: usize) {
        let w = &mut win!(self, wi);
        let Some(tape) = w.selection_tape.as_mut() else { return };
        let idx = tape.pane_idx();
        let Some(sel) = w.selection.as_mut().filter(|s| s.session_idx == idx) else {
            w.selection_tape = None;
            return;
        };
        let Some(pane) = w.panes.get(idx).map(|p| shown::shown_of(p, &self.shadows)) else {
            w.selection_tape = None;
            return;
        };
        let grid_rows = pane.session().grid().rows();
        let shift = tape.fold(&pane.screen_rows());
        if shift.is_none() && tape.broken {
            // The program redrew rather than scrolled, so where the
            // selected lines went is not knowable.  Leaving the box
            // standing would put it over text nobody picked — the
            // whole defect this tape exists to end — so it goes.
            let a = tape.last_alignment();
            lx_info!(
                "core.selection_tape.broken",
                &format!(
                    "three frames running were not a shift of the last readable one; \
                     selection dropped (last: shift={} matched={} could={} rows={grid_rows})",
                    a.shift, a.matched, a.could
                )
            );
            w.selection = None;
            w.selection_dragging = false;
            w.selection_tape = None;
            w.needs_render = true;
            return;
        }
        sel.anchor.1 = tape.abs(tape.anchor.1, grid_rows);
        sel.focus.1 = tape.abs(tape.focus.1, grid_rows);
        // The gate opens when the step is SEEN TO LAND — a frame that
        // moved.  An unchanged frame must not open it: this runs on
        // every pump, and a window of busy panes pumps within a
        // millisecond of the tick being sent, so treating "a frame
        // arrived" as "the step landed" opened the gate again before
        // the program had repainted at all, and the whole rate limit
        // was a no-op.  A step that genuinely moves nothing is what
        // the gate's deadline is for.
        if shift.is_some_and(|s| s != 0) {
            w.tape_scroll_gate = None;
        }
        w.needs_render = true;
    }
}
