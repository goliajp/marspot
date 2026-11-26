//! Drawing a window: what each of its panes and panels shows this frame.

use super::*;

impl CoreApp {
    /// Where the renderer's last IOSurface frame spent its time —
    /// forwarded so the loop's stall report can carry it.
    pub(super) fn renderer_split(&self) -> marspot::render_metal::RenderSplit {
        self.renderer.last_render_split()
    }

    /// Commit a frame and return; the caller presents it once
    /// `WindowRender::settled()` says the GPU is done.
    pub(super) fn render(
        &mut self,
        wi: usize,
        target_tex: &objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>,
    ) -> Option<(f64, f64, f64, f64)> {
        self.render_inner(wi, target_tex, false)
    }

    /// Render and wait, for the one caller that announces the surface
    /// in the same breath.
    ///
    /// Attach installs a fresh pair and acks `SurfaceReady` right
    /// away, so the frame must genuinely be finished — an unfinished
    /// surface announced here is a black window until the next frame.
    /// It happens once per attach and nobody is typing into it, so the
    /// wait costs nothing worth reclaiming.
    pub(super) fn render_blocking(
        &mut self,
        wi: usize,
        target_tex: &objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>,
    ) -> Option<(f64, f64, f64, f64)> {
        self.render_inner(wi, target_tex, true)
    }

    /// What each pane of the window is called on screen.
    fn pane_labels(&self, wi: usize) -> Vec<String> {
        // Pane numbers from the static table: this used to be a `Vec`
        // and one `String` per pane, rebuilt every frame for text that
        // never changes.
        let labels = &marspot::ui::PANE_NUMBER_LABELS[..win!(self, wi)
            .panes
            .len()
            .min(marspot::ui::PANE_NUMBER_LABELS.len())];
        // Title placeholder = basename of the cwd cached in
        // `pane_cwds`.  Pure read: `sweep_pane_cwds` owns population
        // (once per CWD_SWEEP_INTERVAL from the main loop's periodic
        // block, plus a one-shot resolve at pane spawn and on
        // LayoutModal open).  Nothing here may issue a syscall — this
        // runs per frame, per window.
        let cwd_basenames: Vec<Option<&str>> = (0..win!(self, wi).panes.len())
            .map(|i| {
                let sid = win!(self, wi).panes[i].shelld_session_id()?;
                let path = self.pane_cwds.get(&sid)?;
                std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
            })
            .collect();

        // Resolved label per cell: the pane's own name (its directory,
        // with its code when shared — `marspot::pane_name`) → plugin-set
        // title (MsgType::PaneTitle, cc/...) → ordinal fallback.
        //
        // There is no user-set title any more: a pane's name is derived,
        // never stored, so it cannot disagree with what `--send` will
        // accept.  A rename would have made the name a second source of
        // truth about which pane is which.
        let pane_names = self.pane_names();
        (0..win!(self, wi).panes.len())
            .map(|i| {
                if let Some(name) = win!(self, wi).panes[i]
                    .shelld_session_id()
                    .and_then(|sid| pane_names.get(&sid))
                    .filter(|n| !n.is_empty() && *n != "?")
                {
                    name.clone()
                } else if let Some(plugin_title) = win!(self, wi).panes[i]
                    .shelld_session_id()
                    .and_then(|sid| self.pane_titles.get(&sid))
                {
                    plugin_title.clone()
                } else if let Some(Some(name)) = cwd_basenames.get(i) {
                    (*name).to_string()
                } else {
                    labels.get(i).map(|s| (*s).to_string()).unwrap_or_default()
                }
            })
            .collect()
    }

    /// Hand the renderer whatever is open over the panes.
    fn publish_overlays(&mut self, wi: usize, wr: &mut marspot::render_metal::WindowRender, resolved_labels: &[String]) {
        // F3+1.3 — build + push process-panel data BEFORE we
        // borrow `win!(self, wi).panes` into `views`.  Renderer holds the
        // panel data via `set_process_panel`, freeing `&self` for
        // the render call below.
        let panel_data = self.build_process_panel_render(wi);
        self.renderer.set_process_panel(panel_data);
        // cc — publish `Cc` usage modal render state (refreshing the
        // feed at most every 5 s while open; zero I/O when closed).
        let cc_data = self.build_cc_usage_render(wi);
        self.renderer.set_cc_usage(cc_data);
        // The settings panel.  A snapshot per frame — one consistent
        // set of values, so a toggle and a segment can never be drawn
        // from either side of the same click.
        let settings_data =
            (win!(self, wi).panels.open() == Some(SurfaceKind::Settings)).then(|| self.settings_render(wi));
        self.renderer.set_settings_panel(settings_data);
        let shortcuts =
            (win!(self, wi).panels.open() == Some(SurfaceKind::Shortcuts)).then(|| self.shortcuts_render(wi));
        self.renderer.set_shortcuts_panel(shortcuts);
        self.publish_helpers(wi, wr);
        // RFC-006 — this window's drop-preview ghost (usually None),
        // and the index of its pane mid-drag (dimmed by the renderer).
        self.renderer.set_drop_preview(win!(self, wi).drop_preview);
        self.renderer.set_drag_source(
            self.pane_drag
                .filter(|d| d.active && d.from_wi == wi)
                .map(|d| d.idx),
        );

        let menu = self.menu_render(wi);
        self.renderer.set_context_menu(menu);
        // F3+3.0 / 3.3 — publish LayoutModal state every frame.
        // Per-slot titles built from card_slots → resolved title
        // chain (custom > cwd basename > ordinal).  Empty slot if
        // the slot points at a pane index past the live count.
        let layout_modal = if win!(self, wi).panels.open() == Some(SurfaceKind::Layout) {
            let cells = win!(self, wi).pending_grid_cols * win!(self, wi).pending_grid_rows;
            let mut titles = std::mem::take(&mut win!(self, wi).layout_titles);
            titles.resize_with(cells, Default::default);
            for (slot, title) in titles.iter_mut().enumerate() {
                let pane_idx = win!(self, wi).card_slots.get(slot).copied().unwrap_or(usize::MAX);
                title.clear();
                title.push_str(resolved_labels.get(pane_idx).map_or("", |s| s.as_str()));
            }
            win!(self, wi).layout_modal_label_w = marspot::ui::components::layout_modal::widest_label(
                &titles,
                &mut marspot::ui::components::controls::font_measure(self.renderer.font_mut()),
            );
            win!(self, wi).layout_titles = titles;
            Some(self.layout_render(wi))
        } else {
            None
        };
        self.renderer.set_layout_modal(layout_modal);
    }

    /// Render the full UI into `target_tex` and return the focused-
    /// pane caret rect (view-local physical pixels) for the IME
    /// candidate window, or `None` when the slot takes no input.
    fn render_inner(
        &mut self,
        wi: usize,
        target_tex: &objc2::rc::Retained<ProtocolObject<dyn MTLTexture>>,
        block: bool,
    ) -> Option<(f64, f64, f64, f64)> {
        let focused = win!(self, wi).focused_idx;
        // Everything below borrows the window — the pane grids, the
        // preedit string, the layout — while the renderer wants `&mut`
        // on that same window's render state.  They are disjoint
        // fields, but the borrow checker cannot see through the
        // `windows[key_window]` index, so move the render state out
        // for the duration of the frame and put it back at the end.
        let mut wr = std::mem::take(&mut win!(self, wi).render);

        let states: Vec<SessionState> =
            win!(self, wi).panes.iter().map(|p| p.session().state()).collect();

        let resolved_labels = self.pane_labels(wi);
        let titles: Vec<String> = (0..win!(self, wi).panes.len())
            .map(|i| {
                let mut s = resolved_labels.get(i).cloned().unwrap_or_default();
                // a pane being dragged wears a ⇢: drop it on another
                // window to move it there, anywhere else to cancel
                if self
                    .pane_drag
                    .is_some_and(|d| d.active && d.from_wi == wi && d.idx == i)
                {
                    s.insert_str(0, "⇢ ");
                }
                if let Some(view) = self.view_label(wi, i) {
                    s.push_str(&view);
                }
                s
            })
            .collect();
        let sidebar_labels: Vec<String> = resolved_labels
            .iter()
            .map(|s| truncate_for_sidebar(s, MAX_SIDEBAR_LABEL_CHARS))
            .collect();

        // the titles say how many helpers only where no strip lists them
        self.place_strips(wi);
        self.publish_view_line(wi, &mut wr, &titles);
        self.publish_overlays(wi, &mut wr, &resolved_labels);
        // Cap views to the layout's cell count — sessions past it
        // stay alive in the sidebar without a main-area cell.
        let cell_count = win!(self, wi).layout.cells.len();
        // Advance every pane's dim towards where its attention level
        // says it belongs, and keep the frames coming while any of them
        // is still moving.  Nothing here is a timer: when the last one
        // arrives this stops asking, and the window is idle again.
        let now = Instant::now();
        let mut fading = false;
        for (i, p) in win!(self, wi).panes.iter_mut().enumerate().take(cell_count) {
            fading |= p.aim_scrim(i == focused, now);
        }
        let views: Vec<SessionView> = win!(self, wi)
            .panes
            .iter()
            .take(cell_count)
            .enumerate()
            .map(|(i, host)| {
                // the slot's badge, title and dim; the rest is of what it shows
                let p = shown::shown_of(host, &self.shadows);
                let badge = host
                    .shelld_session_id()
                    .and_then(|sid| self.pane_badges.get(&sid).map(|s| s.as_str()))
                    .unwrap_or("");
                // A plugin only declares wheel keys about a program
                // it is actually driving, and the declaration survives
                // a core swap — so it is the durable answer to "does
                // an agent TUI paint this pane", where a badge that
                // can momentarily read empty is not.
                // The plugin's own declaration when there is one; the
                // old inference only as a fallback for an L1 that does
                // not send it yet.
                let agent_tui = match p
                    .shelld_session_id()
                    .and_then(|sid| self.pane_agent_tui.get(&sid))
                {
                    Some(&on) => on,
                    None => {
                        p.shelld_session_id()
                            .is_some_and(|sid| self.pane_wheel_keys.contains_key(&sid))
                            || p.shelld_session_id().is_some_and(|sid| self.pane_badges.contains_key(&sid))
                    }
                };
                let pane_cwd = p
                    .shelld_session_id()
                    .and_then(|sid| self.pane_cwds.get(&sid))
                    .map(String::as_str)
                    .unwrap_or("");
                let mut v = p.view(
                    i == focused,
                    titles.get(i).map(|s| s.as_str()).unwrap_or(""),
                    badge,
                    agent_tui,
                    pane_cwd,
                );
                (v.recede, v.scrim) = host.slot_look();
                // a panel has the keyboard: what is being composed is not the pane's
                if i == focused && p.view_offset() == 0 && !win!(self, wi).panels.owns_pointer() {
                    v.ime_preedit = win!(self, wi).ime_preedit.as_str();
                }
                v.strip = self.strips.view(host.shelld_session_id());
                v.notice = self.notices.over(p.shelld_session_id(), i == focused);
                v.selection = win!(self, wi)
                    .selection
                    .as_ref()
                    .and_then(|sel| selection_view_for_pane(p, sel, i));
                v
            })
            .collect();
        let entries: Vec<SidebarEntry> = sidebar_labels
            .iter()
            .zip(states.iter())
            .map(|(label, state)| SidebarEntry {
                label: label.as_str(),
                state: *state,
            })
            .collect();
        let (cell_w, cell_h) = self.renderer.cell_dims();
        if block {
            self.renderer.render_layout_to_texture(
                &mut wr,
                target_tex,
                &win!(self, wi).layout,
                &views,
                &entries,
                focused,
            );
        } else {
            self.renderer.render_layout_to_texture_async(
                &mut wr,
                target_tex,
                &win!(self, wi).layout,
                &views,
                &entries,
                focused,
            );
        }
        win!(self, wi).render = wr;
        // Cleared here, then set again if a dim is still on its way:
        // the order matters, because this reset runs after the frame
        // and would otherwise wipe the request for the next one.
        win!(self, wi).needs_render = fading;
        frames::note_drawn(&mut win!(self, wi), cell_count);

        self.shown_pane(wi, focused).and_then(|pane| {
            let (col, row) = pane.session().ime_caret_cell()?;
            win!(self, wi).layout
                .caret_view_phys_rect(focused, col, row, cell_w, cell_h)
        })
    }
}
