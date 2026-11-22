//! Slot and frame bookkeeping: which slot a window or pane takes, and the card slot permutation.

use super::*;

impl CoreApp {
    /// Close the gap a discarded window left: everything above it
    /// moves down one.  Mirrored by `ShellApp::forget_slot`, which
    /// does the same to `window-state.bin` — the two lists are paired
    /// by position, so they compact together or not at all.
    pub(super) fn forget_slot(&mut self, slot: usize) {
        for w in &mut self.windows {
            if w.frame_index > slot {
                w.frame_index -= 1;
            }
        }
        for (s, _) in &mut self.parked_windows {
            if *s > slot {
                *s -= 1;
            }
        }
        for (s, _) in &mut self.saved_windows {
            if *s > slot {
                *s -= 1;
            }
        }
    }

    /// The slot for a window with no saved record: the one L1 named,
    /// unless something already holds it.
    pub(super) fn slot_for_new_window(&self, announced: Option<usize>) -> usize {
        let taken = |s: usize| {
            self.windows.iter().any(|w| w.frame_index == s)
                || self.parked_windows.iter().any(|(q, _)| *q == s)
                || self.saved_windows.iter().any(|(q, _)| *q == s)
        };
        match announced {
            Some(s) if !taken(s) => s,
            _ => self.next_frame_index(),
        }
    }

    /// The slot a brand-new window takes: one past every slot spoken
    /// for, whether by an open window, a parked one, or a saved record
    /// still waiting for L1 to reopen it.  Monotonic, so a new window
    /// can never land on top of a window that is merely away.
    pub(super) fn next_frame_index(&self) -> usize {
        let live = self.windows.iter().map(|w| w.frame_index);
        let parked = self.parked_windows.iter().map(|(slot, _)| *slot);
        let pending = self.saved_windows.iter().map(|(slot, _)| *slot);
        live.chain(parked)
            .chain(pending)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0)
    }

    /// One window's persistable shape — grid, focus, and its panes in
    /// slot order.  Shared by the live save and by the snapshot taken
    /// when a window is parked, so a parked window is byte-identical
    /// to what it would have saved while open.
    pub(super) fn window_layout_record(
        &self,
        w: &WindowState,
    ) -> marspot::state::SavedWindowLayout {
        use marspot::state::{SavedPane, SavedWindowLayout};
        SavedWindowLayout {
            grid_cols: w.grid_cols as u16,
            grid_rows: w.grid_rows as u16,
            focused_idx: w.focused_idx as u16,
            panes: w
                .panes
                .iter()
                .map(|p| {
                    let sid = p.shelld_session_id().unwrap_or(0);
                    // The format keeps the field so an older build can
                    // still read this file; nothing sets it.
                    let custom_title = String::new();
                    let last_cwd =
                        self.pane_cwds.get(&sid).cloned().unwrap_or_default();
                    let flags = if p.is_dormant() {
                        marspot::state::PANE_FLAG_DORMANT
                    } else {
                        0
                    };
                    SavedPane { sid, flags, custom_title, last_cwd }
                })
                .collect(),
        }
    }

    pub(super) fn reset_card_slots(&mut self, wi: usize) {
        let cells = win!(self, wi).pending_grid_cols * win!(self, wi).pending_grid_rows;
        win!(self, wi).card_slots = (0..cells).collect();
        win!(self, wi).layout_drag = None;
    }

    /// F3+3.3 — apply `card_slots` as a permutation on the leading
    /// `cells` panes.  After this, `win!(self).panes[slot_idx]` is the
    /// pane that previously sat at `card_slots[slot_idx]` (== the
    /// pane the user dragged into slot_idx in the modal).
    ///
    /// `card_slots` entries that point past the live pane count
    /// are skipped (empty cards stay empty).  The post-cells tail
    /// of `win!(self).panes` (sidebar overflow) is untouched.  Titles,
    /// scroll offsets, search state all travel for free — they are
    /// fields of `Pane`, and it is whole `Pane`s being permuted.
    /// Resets `card_slots` to identity afterwards.
    pub(super) fn apply_card_slot_permutation(&mut self, wi: usize, cells: usize) {
        let n_in_grid = cells.min(win!(self, wi).panes.len());
        if n_in_grid == 0 || win!(self, wi).card_slots.len() < n_in_grid {
            self.reset_card_slots(wi);
            return;
        }
        // Build new layouts for the leading n_in_grid slots.  Slots
        // pointing at out-of-range pane indices map to None (empty)
        // and the corresponding existing pane keeps its place at
        // the tail (skipped during reorder).
        let mut new_panes: Vec<Option<Pane>> = (0..n_in_grid).map(|_| None).collect();
        // Drain the leading n_in_grid panes into Option holders so
        // we can move them around without re-borrow conflicts.
        let mut drained: Vec<Option<Pane>> =
            win!(self, wi).panes.drain(..n_in_grid).map(Some).collect();
        for (slot_idx, slot) in new_panes.iter_mut().enumerate() {
            let from = win!(self, wi).card_slots[slot_idx];
            if from < drained.len() {
                *slot = drained[from].take();
            }
        }
        // Re-insert at the head.  Any leftover (None) means the slot
        // had no source pane — should not happen with identity-only
        // permutations but defensible: pull from a leftover pool to
        // avoid panicking.
        let mut leftover: Vec<Pane> = drained.into_iter().flatten().collect();
        let mut ordered: Vec<Pane> = Vec::with_capacity(n_in_grid);
        for slot in new_panes.iter_mut() {
            match slot.take() {
                Some(p) => ordered.push(p),
                None => {
                    if let Some(p) = leftover.pop() {
                        ordered.push(p);
                    }
                }
            }
        }
        // Re-prepend.
        let tail_panes = std::mem::take(&mut win!(self, wi).panes);
        win!(self, wi).panes = ordered;
        win!(self, wi).panes.extend(tail_panes);
        self.reset_card_slots(wi);
    }
}
