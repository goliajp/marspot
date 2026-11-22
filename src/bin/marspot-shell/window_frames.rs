//! Saving each window's frame by slot, merged over what is already on
//! disk, so the next launch reopens every window where it was.

use super::*;

/// Fold this process's live window frames into the list already on
/// disk: index *i* takes the live frame when there is one, otherwise
/// keeps whatever was saved there, and entries past the live windows
/// are preserved.
///
/// Both halves of that matter, and both were bugs:
///   - a live window with no frame yet (`None`) must NOT be skipped —
///     skipping shifts every later window's entry by one;
///   - the on-disk tail must NOT be truncated — during a cold-launch
///     restore those entries are the geometry the core is about to ask
///     for, and they are gone by the time it does.
fn merge_window_frames(
    prev: &[marspot::state::SavedWindow],
    live: &[Option<marspot::state::SavedWindow>],
) -> Vec<marspot::state::SavedWindow> {
    let mut out: Vec<marspot::state::SavedWindow> =
        Vec::with_capacity(prev.len().max(live.len()));
    for i in 0..prev.len().max(live.len()) {
        match live.get(i).and_then(|v| v.clone()) {
            Some(fresh) => out.push(fresh),
            // No live frame for this slot: keep the saved one.  With
            // nothing saved either (a window that somehow hasn't
            // reported its frame and has no history), write a zeroed
            // hole rather than skipping the slot — a skip would shift
            // every later entry, while a hole is rejected by the
            // restore path's own `w > 50 && h > 50` check and just
            // means "this one opens at the default rect".
            None => out.push(
                prev.get(i).cloned().unwrap_or(marspot::state::SavedWindow {
                    display_id: 0,
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                }),
            ),
        }
    }
    out
}

/// Lay the open windows out by their own slot, ready to be merged
/// over what is already on disk.
///
/// Indexed by `frame_index`, not by position: a window that closed
/// leaves a gap here, and the gap is what keeps the windows that are
/// still open pointing at their own geometry.  Writing by position
/// meant closing the first of two windows moved the second into slot
/// 0 — so the two swapped frames on the next launch.
fn live_frames_by_slot(
    windows: impl Iterator<Item = (usize, Option<marspot::state::SavedWindow>)>,
    saved_len: usize,
) -> Vec<Option<marspot::state::SavedWindow>> {
    let mut out: Vec<Option<marspot::state::SavedWindow>> = vec![None; saved_len];
    for (slot, frame) in windows {
        if out.len() <= slot {
            out.resize(slot + 1, None);
        }
        out[slot] = frame;
    }
    out
}

#[cfg(test)]
mod window_frame_merge_tests;

impl ShellApp {
    /// F3+6.1 — read the NSWindow's current frame + screen, atomic-
    /// write to `window-state.bin` ONLY when the values changed since
    /// the last save.  Without dedup the `resized` callback fires
    /// 60+ times during a live drag = a write storm; tracking the
    /// last-saved tuple turns that into a single save per genuine
    /// change.  Frames compared at 1-pt granularity (sub-pt diffs
    /// from AppKit's internal float math get coalesced).
    pub(super) fn save_window_state_if_changed(&mut self, ctx: &MarspotAppCtx) {
        let Some(wi) = self.window_index(ctx.window_id()) else { return };
        let (x, y, w, h) = ctx.window_frame_pt();
        let display_id = ctx.window_display_id().unwrap_or(0);
        let cur = (x.round(), y.round(), w.round(), h.round(), display_id);
        if self.windows[wi].last_saved_window == Some(cur) {
            return;
        }
        self.windows[wi].last_saved_window = Some(cur);
        self.save_window_frames();
    }

    /// RFC-005 step 6 — write every window's frame, in creation order,
    /// so entry *i* keeps pairing with window *i* of `shell-state.bin`.
    ///
    /// Merged against the previous list rather than replacing it.  The
    /// replacing version broke every silent update: an L1 self-execv
    /// tears down all the NSWindows, the successor opens window 0 (at
    /// the frame handed over in `MARSPOT_RESTORE_FRAME`) and saves — at
    /// which point `self.windows.len() == 1`, so the file was rewritten
    /// with one entry and window 1's geometry was deleted.  Moments
    /// later the core asked for window 1 to be reopened, the restore
    /// found no entry for index 1, and the window came back at the
    /// default 1200×800 rect.  `shell.window.restore_no_frame
    /// frame_index=1` was in the log for every single update.
    ///
    /// So: overwrite by index, never shorten.  A window that closed
    /// leaves its last geometry behind, which is harmless — the core
    /// drives restores from its own per-window layout records, and
    /// nothing asks for an index it has no window for.
    pub(super) fn save_window_frames(&mut self) {
        let live = live_frames_by_slot(
            self.windows.iter().map(|w| {
                (
                    w.frame_index,
                    w.last_saved_window.map(|(x, y, ww, h, display_id)| {
                        marspot::state::SavedWindow { display_id, x, y, w: ww, h }
                    }),
                )
            }),
            self.saved_window_frames.len(),
        );
        let frames = merge_window_frames(&self.saved_window_frames, &live);
        if frames.is_empty() {
            return;
        }
        if let Err(e) = marspot::state::write_windows(&frames) {
            marspot::lx_warn!(
                "shell.window_state.write_failed",
                &format!("{e}")
            );
            return;
        }
        self.saved_window_frames = frames;
    }
}
