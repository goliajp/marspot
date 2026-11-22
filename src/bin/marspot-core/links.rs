//! Cmd-C copy of the selection, and hit-testing the links and badges under the pointer.

use super::*;

impl CoreApp {
    pub(super) fn copy_selection_to_clipboard(&mut self, wi: usize) -> bool {
        let Some(sel) = win!(self, wi).selection else { return false };
        let idx = sel.session_idx;
        // L3 owns the real grid + scrollback; L2's mirror is a window-only
        // synthetic grid that `grid_selection_text` can't read back, so the
        // text round-trips through the session process.  In-process panes
        // read it locally.  The round-trip is made reliable by a per-request
        // sequence id (see `request_selection_text`) so a late reply from a
        // timed-out request can't alias the next copy.
        // A taped pane is read off the tape, not off the pane: the
        // program owns the picture there, and rows the selection covers
        // may have scrolled out of the frame the pane can still show.
        // The tape kept them.
        let blockwise = sel.mode == marspot::ui::SelectionMode::Blockwise;
        let taped = win!(self, wi)
            .selection_tape
            .as_ref()
            .filter(|t| t.pane_idx() == idx && !t.broken)
            .and_then(|t| t.text(blockwise));
        let is_l3 = win!(self, wi).panes.get(idx).is_some_and(|p| p.is_l3());
        let text = if taped.is_some() {
            taped
        } else if is_l3 {
            win!(self, wi).panes
                .get_mut(idx)
                .and_then(|p| p.session_mut().request_selection_text(sel.anchor, sel.focus, blockwise))
        } else {
            win!(self, wi).panes.get(idx).and_then(|pane| selection_text(pane, &sel))
        };
        // cc-only post-processing: claudecode renders to a fixed inner
        // width with hard `\n` wraps that we don't want on the clipboard.
        // Detect cc panes via the L1 plugin badge — non-empty entry on
        // this pane's shelld_session_id means cc plugin tagged it.  See
        // `src/cc.rs`.
        let text = text.map(|t| {
            let is_cc = win!(self, wi)
                .panes
                .get(idx)
                .and_then(|p| p.shelld_session_id())
                .and_then(|sid| self.pane_badges.get(&sid))
                .map(|b| !b.is_empty())
                .unwrap_or(false);
            if is_cc { marspot::cc::rejoin_wrapped_paragraphs(&t) } else { t }
        });
        match text {
            Some(text) => marspot::input::write_clipboard_text(&text),
            None => false,
        }
    }

    /// Hit-test an auto-detected link span on pane `idx`.  Scans the
    /// pane's visible grid (cheap — bounded by visible cells) and
    /// returns the first span containing the clicked (col, row).
    /// Returns None when the click missed every detected link.
    pub(super) fn hit_test_pane_link(
        &self, wi: usize,
        idx: usize,
        col: u16,
        row: u16,
    ) -> Option<(marspot::grid_links::LinkRange, bool)> {
        let focused = win!(self, wi).focused_idx == idx;
        let pane = win!(self, wi).panes.get(idx)?;
        // The offset the RENDERER used, not the pane's logical one —
        // see `Pane::render_view_offset`.
        let view_offset = pane.render_view_offset(focused);
        let grid = pane.session().grid();
        // a link the program declared (OSC 8) wins over one found by its shape
        if let Some(link) = marspot::osc8::link_at(grid, view_offset, col, row) {
            return Some((link, true));
        }
        // cc-mode: claudecode hard-newlines URLs / paths with a hanging
        // indent; see `grid_links::ScanOpts::cc_mode`.  Same answer the
        // render pass uses (see `agent_tui` there): a hit-test that
        // disagreed would underline one span and click another.
        let cc_mode = match pane
            .shelld_session_id()
            .and_then(|sid| self.pane_agent_tui.get(&sid))
        {
            Some(&on) => on,
            None => pane.shelld_session_id().is_some_and(|sid| {
                self.pane_wheel_keys.contains_key(&sid)
                    || self.pane_badges.get(&sid).is_some_and(|b| !b.is_empty())
            }),
        };
        // The same cwd the render pass resolved relative paths
        // against.  A hit-test with a different one underlines a span
        // and then clicks nothing.
        let cwd = pane
            .shelld_session_id()
            .and_then(|sid| self.pane_cwds.get(&sid))
            .map(String::as_str)
            .filter(|c| !c.is_empty());
        let opts = marspot::grid_links::ScanOpts { cc_mode, cwd };
        // The same non-blocking oracle the render pass uses, for two
        // reasons.  The obvious one: this runs on the main loop's
        // `events` phase, and the blocking default turns one click
        // into a full-screen stat sweep — caught in the field at
        // 6.39 s with every pane frozen, `mouse_down →
        // hit_test_link_at_xy → FsOracle::probe → lstat` accounting
        // for 948 of 1556 samples.
        //
        // The less obvious one is correctness: a link is clickable
        // because it is *painted*, and it is painted because the
        // render pass got `Exists` from this cache.  Asking a
        // different oracle here lets the hit-test disagree with what
        // is on screen in both directions — an underline that does
        // nothing, or a click that fires on a row showing no link.
        let links = marspot::grid_links::scan_visible_links_with(
            grid,
            view_offset,
            opts,
            marspot::link_probe::oracle(),
        );
        links
            .into_iter()
            .find(|link| link.row == row && col >= link.col_start && col <= link.col_end)
            .map(|link| (link, false))
    }

    /// Why a right-click that landed on painted link text produced no
    /// link menu — or `None` when the click was not on a pane cell.
    ///
    /// The scan the renderer runs and the one the hit-test runs take
    /// the same grid, the same view offset, the same options and the
    /// same oracle, and nothing checks that they agree.  When they
    /// drift the user sees an underline that does nothing.
    pub(super) fn link_miss_report(&self, wi: usize, x_phys: f64, y_phys: f64) -> Option<String> {
        let (cw, ch) = self.renderer.cell_dims();
        let (idx, col, row) = win!(self, wi).layout.hit_test_cell_pos(x_phys, y_phys, cw, ch)?;
        let focused = win!(self, wi).focused_idx == idx;
        let pane = win!(self, wi).panes.get(idx)?;
        let view_offset = pane.render_view_offset(focused);
        let sid = pane.shelld_session_id();
        let declared = sid.and_then(|s| self.pane_agent_tui.get(&s)).copied();
        let links = {
            let grid = pane.session().grid();
            let cc_mode = match declared {
                Some(on) => on,
                None => sid.is_some_and(|s| {
                    self.pane_wheel_keys.contains_key(&s)
                        || self.pane_badges.get(&s).is_some_and(|b| !b.is_empty())
                }),
            };
            let cwd = sid
                .and_then(|s| self.pane_cwds.get(&s))
                .map(String::as_str)
                .filter(|c| !c.is_empty());
            marspot::grid_links::scan_visible_links_with(
                grid,
                view_offset,
                marspot::grid_links::ScanOpts { cc_mode, cwd },
                marspot::link_probe::oracle(),
            )
        };
        let on_row: Vec<String> = links
            .iter()
            .filter(|l| l.row == row)
            .map(|l| format!("{}..{}:{:?}", l.col_start, l.col_end, l.text))
            .collect();
        Some(format!(
            "pane={idx} click=({col},{row}) view_offset={view_offset} \
             declared={declared:?} links_total={} on_this_row={}{}",
            links.len(),
            on_row.len(),
            if on_row.is_empty() { String::new() } else { format!(" {on_row:?}") }
        ))
    }

    /// Top-level link hit-test from physical pixel coordinates.
    /// Folds the (x_phys, y_phys) → (pane, col, row) step the
    /// renderer-aware caller would otherwise repeat.  Every LinkKind
    /// is actionable (URL / File / Ip open, Email mails, Uuid
    /// copies), so a `Some` always warrants a menu.  Returns owned
    /// text + kind so the result can be stashed in
    /// `ContextMenuState.link` and survive the menu's
    /// clear-on-dispatch.
    pub(super) fn hit_test_link_at_xy(
        &self, wi: usize,
        x_phys: f64,
        y_phys: f64,
    ) -> Option<LinkContext> {
        let (cw, ch) = self.renderer.cell_dims();
        let (idx, col, row) = win!(self, wi).layout.hit_test_cell_pos(x_phys, y_phys, cw, ch)?;
        let (link, declared) = self.hit_test_pane_link(wi, idx, col, row)?;
        Some(LinkContext { text: link.text, target: link.target, kind: link.kind, declared })
    }

    /// Why a right-click inside a badge-bearing title strip missed
    /// the badge — or `None` when it did not land in one at all.
    ///
    /// The badge's box is computed in two places from the same
    /// inputs: here, and in the renderer's `push_session`.  Nothing
    /// checks that the two agree, and when they drift the only
    /// symptom is a badge you can see and cannot click.  This turns
    /// that into a line naming the click, the box, and the text.
    pub(super) fn badge_miss_report(&self, wi: usize, x_phys: f64, y_phys: f64) -> Option<String> {
        let (cell_w, _) = self.renderer.cell_dims();
        let padding = win!(self, wi).layout.padding;
        let title_h = win!(self, wi).layout.cell_title_h;
        let cell_count = win!(self, wi).layout.cells.len();
        for (i, p) in win!(self, wi).panes.iter().enumerate().take(cell_count) {
            // `continue`, not `?`.  A pane with no session id yet — one
            // still starting, one whose L3 just died — is a pane to skip,
            // not a reason to abandon the search: with `?` a single such
            // pane ANYWHERE ahead of the clicked one silently suppressed
            // this whole report, which is why `core.badge_hit_miss` never
            // appeared once in 13k log lines while the badge menu was
            // demonstrably failing (2026-09-05).
            let sid = match p.shelld_session_id() {
                Some(s) => s,
                None => continue,
            };
            let rect = &win!(self, wi).layout.cells[i];
            let y_lo = rect.y_top;
            if y_phys < y_lo || y_phys >= y_lo + title_h {
                continue;
            }
            if x_phys < rect.x || x_phys >= rect.x + rect.w {
                continue;
            }
            let badge = self.pane_badges.get(&sid).cloned().unwrap_or_default();
            if badge.is_empty() {
                // No badge on this pane: a plain title-strip click,
                // which is not a miss.
                return None;
            }
            let chars = badge.chars().count() as f64;
            let lo = rect.x + rect.w - padding - chars * cell_w;
            let prefix = badge.split(' ').next().unwrap_or("").chars().count() as f64;
            return Some(format!(
                "sid={sid} click=({x_phys:.1},{y_phys:.1}) \
                 badge={badge:?} chars={chars} prefix_chars={prefix} \
                 box=[{lo:.1},{:.1}) cell_w={cell_w:.2} \
                 pane=[{:.1},{:.1}) focused_idx={}",
                lo + prefix * cell_w,
                rect.x,
                rect.x + rect.w,
                win!(self, wi).focused_idx,
            ));
        }
        None
    }
}
