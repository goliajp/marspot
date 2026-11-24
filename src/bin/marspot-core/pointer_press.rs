//! What a press can land on, in the order it is asked.

use super::*;

impl CoreApp {
    /// Whether the open context menu took the press.
    pub(super) fn menu_takes_press(&mut self, wi: usize, x_phys: f64, y_phys: f64) -> bool {
        // F3+9 — when context menu is open, a left click first
        // dispatches an item / swallows on frame / closes-on-outside.
        if let Some(hit) = self.menu_press(wi, x_phys, y_phys) {
            use marspot::ui::components::ContextMenuHit;
            let region = win!(self, wi).context_menu.as_ref().unwrap().region;
            match hit {
                ContextMenuHit::Item(idx) => {
                    let tag = win!(self, wi).context_menu.as_ref().unwrap()
                        .items[idx].action_tag;
                    // Badge menus carry plugin-opaque tags
                    if let ContextRegion::PaneBadge(sid) = region {
                        self.pick_badge_row(wi, sid, tag);
                    } else if tag == MOVE_TO_NEW_WINDOW_TAG
                        || (MOVE_TO_WINDOW_TAG_BASE..MOVE_TO_NEW_WINDOW_TAG).contains(&tag)
                    {
                        let idx = match region {
                            ContextRegion::Pane(i)
                            | ContextRegion::SidebarSlot(i) => i,
                            _ => win!(self, wi).focused_idx,
                        };
                        win!(self, wi).context_menu = None;
                        win!(self, wi).needs_render = true;
                        if tag == MOVE_TO_NEW_WINDOW_TAG {
                            self.move_pane_to_new_window(wi, idx);
                        } else {
                            let to = (tag - MOVE_TO_WINDOW_TAG_BASE) as usize;
                            self.move_pane_to_window(wi, idx, to);
                        }
                    } else if let Some(action) = ContextMenuAction::from_tag(tag) {
                        self.dispatch_context_action(wi, action, region);
                    } else {
                        win!(self, wi).context_menu = None;
                        win!(self, wi).needs_render = true;
                    }
                    return true;
                }
                ContextMenuHit::Frame => {
                    // Click on divider / disabled row / padding —
                    // swallow, NSMenu-style.
                    return true;
                }
                ContextMenuHit::Outside => {
                    win!(self, wi).context_menu = None;
                    win!(self, wi).needs_render = true;
                    // Fall through: click also triggers normal
                    // focus / selection behaviour.
                }
            }
        }
        false
    }

    /// Whether the open panel, or the button that toggles one, took the press.
    pub(super) fn panel_takes_press(&mut self, wi: usize, on: Option<ChromeTarget>, x_phys: f64, y_phys: f64) -> bool {
        // A toolbar button that opens a panel toggles it, whatever is
        // open, when it is let go on.  Any other press while a panel is
        // open is that panel's: the stack says whether it closes it,
        // starts a drag or lands on its content, and it never reaches
        // what is underneath.
        if let Some(ChromeTarget::Toolbar(i)) = on
            && chrome_input::panel_of_button(i).is_some()
        {
            self.press_chrome(wi, x_phys, y_phys);
            return true;
        }
        if let Some(kind) = win!(self, wi).panels.open() {
            let frame = self.panel_frame(wi, kind);
            let press = if matches!(kind, SurfaceKind::Settings | SurfaceKind::Shortcuts | SurfaceKind::Layout | SurfaceKind::Accounts | SurfaceKind::Helpers) {
                let on = self.tree_panel_press_on(wi, kind, x_phys, y_phys);
                win!(self, wi).panels.on_press_on(on, x_phys, y_phys, frame)
            } else {
                // the process monitor answers from the frame last
                // drawn, and has drawn none the moment it opens
                let chrome = PanelChrome::new(frame, win!(self, wi).layout.top_inset);
                win!(self, wi).panels.on_press(x_phys, y_phys, &chrome)
            };
            match press {
                Some(Press::Close(change)) => self.apply_surface_change(wi, change),
                Some(Press::Content(kind)) => self.panel_content_press(wi, kind, x_phys, y_phys),
                Some(Press::BeginDrag) | None => {}
            }
            win!(self, wi).needs_render = true;
            return true;
        }
        false
    }

    /// Whether a press on pane `idx`'s title strip is done with.
    pub(super) fn title_press(&mut self, wi: usize, idx: usize, x_phys: f64, y_phys: f64) -> bool {
        // RFC-006 — a dormant placeholder has no live content to
        // move; its title press is inert (the CELL click below is
        // what revives it).
        if win!(self, wi).panes.get(idx).is_some_and(|p| p.is_dormant()) {
            return true;
        }
        if idx < win!(self, wi).panes.len() {
            self.set_focus(wi, idx);
            self.snap_focused_to_live(wi);
            win!(self, wi).selection = None;
            win!(self, wi).selection_dragging = false;
            win!(self, wi).needs_render = true;
            self.pane_drag = Some(PaneDrag {
                from_wi: wi,
                idx,
                start: (x_phys, y_phys),
                active: false,
            });
            return true;
        }
        false
    }

    /// Whether the press landed on a link, and opened its menu.
    pub(super) fn link_press(&mut self, wi: usize, x_phys: f64, y_phys: f64) -> bool {
        // Auto-link hit-test: a click that lands on an underlined
        // URL / file span opens the same context menu the right-
        // click path uses (Open + Copy).  User-facing behaviour is
        // "any click on a URL/path gives you the choice" instead
        // of "left-click opens directly" — so an accidental click
        // doesn't silently spawn `/usr/bin/open`.  Email is
        // recognised but inert.  Sits ahead of selection so the
        // click doesn't simultaneously start a fresh selection
        // on the link cells.
        if let Some(link) = self.hit_test_link_at_xy(wi, x_phys, y_phys) {
            let items = self.build_link_menu_items(&link);
            if !items.is_empty() {
                let region = self.resolve_context_region(wi, x_phys, y_phys);
                win!(self, wi).context_menu = Some(ContextMenuState {
                    items: items.into(),
                    anchor_x: marspot::ui::Units::from_px(x_phys),
                    anchor_y: marspot::ui::Units::from_px(y_phys),
                    region,
                    surface: Default::default(),
                    link: Some(link),
                });
                win!(self, wi).needs_render = true;
            }
            return true;
        }
        false
    }

    /// Whether the press landed on a vacant seat, and revived it.
    pub(super) fn vacant_press(&mut self, wi: usize, cell_pos_hit: Option<(usize, u16, u16)>) -> bool {
        // RFC-006 — clicking a dormant placeholder is ITS revive
        // gesture: spawn a fresh shell into that slot, explicitly and
        // only on this click.  Checked before selection so the click
        // doesn't also start selecting the hint text.
        if let Some((idx, _, _)) = cell_pos_hit
            && win!(self, wi).panes.get(idx).is_some_and(|p| p.is_dormant()) {
                if let Ok(sid) = allocate_next_session_id() {
                    let (cols, rows) = win!(self, wi)
                        .layout
                        .cells
                        .get(idx)
                        .map(|c| (c.cols, c.rows))
                        .unwrap_or((INITIAL_COLS, INITIAL_ROWS));
                    let from = self
                        .focused_shown(wi)
                        .and_then(|p| p.shelld_session_id())
                        .map(live_cwd_of)
                        .unwrap_or_default();
                    win!(self, wi).panes[idx] =
                        spawn_l3_pane_async(cols, rows, sid, from, &self.event_tx);
                    win!(self, wi).focused_idx = idx;
                    win!(self, wi).needs_render = true;
                    self.save_session_state();
                    lx_event!(
                        "DORMANT_REVIVED",
                        "placeholder clicked; spawning a shell into the slot",
                        session_id = sid
                    );
                }
                return true;
            }
        false
    }

    /// Whether the press went to the program in the pane.
    pub(super) fn reported_press(&mut self, wi: usize, cell_pos_hit: Option<(usize, u16, u16)>, modifiers: Modifiers) -> bool {
        // Selecting wins the plain click; hold shift to hand it to the
        // program instead.
        //
        // That is the inverse of the convention, and deliberately. Every
        // other terminal gives the plain click to a program that asked
        // for mouse reports, because the program is usually an editor or
        // a pager the user is working *inside*. The panes here are agent
        // TUIs: clicking inside one does nothing at all, and selecting
        // its output is what the mouse is for, all day. Following the
        // convention meant the only way to copy a line of output was a
        // chord nobody had been told about -- reported as "I can't
        // select text in a pane any more" within a day of the forwarding
        // landing.
        //
        // The forwarding itself was never asked for. What was asked for
        // was the wheel, and the wheel is a different path (`scroll`),
        // untouched by this.
        if let Some((idx, col, row)) = cell_pos_hit
            && modifiers.shift
            && self.shown_pane(wi, idx)
                .map(|p| p.session().l3_mouse_tracking_active())
                .unwrap_or(false)
        {
            self.set_focus(wi, idx);
            win!(self, wi).needs_render = true;
            let (c1, r1) = (col as u32 + 1, row as u32 + 1);
            if let Some(pane) = self.shown_pane_mut(wi, idx) {
                // Both encodings count from one.
                pane.session_mut().l3_inject_mouse_button(0, true, c1, r1);
            }
            self.mouse_report_press = Some((wi, idx, c1, r1));
            self.mouse_report_last_cell = Some((wi, idx, c1, r1));
            return true;
        }
        false
    }

    /// Start a selection at a cell of pane `idx`, and focus the pane.
    pub(super) fn start_selection(&mut self, wi: usize, idx: usize, col: u16, row: u16, modifiers: Modifiers) -> bool {
        let pane = self.shown_pane(wi, idx);
        if let Some(pane) = pane {
            let rows = pane.session().grid().rows() as u32;
            let vo = pane.view_offset() as u32;
            let abs = vo + rows.saturating_sub(1).saturating_sub(row as u32);
            // A pane whose picture the program owns gets a tape:
            // its scroll is a repaint marspot files nothing from,
            // so without one the selection would sit still on the
            // glass while the text moved under it.  Built here,
            // while the pane is still borrowed for `abs`.
            let tape = pane.program_owns_scroll().then(|| {
                marspot::selection_tape::SelectionTape::new(
                    idx,
                    &pane.screen_rows(),
                    col,
                    row,
                )
            });
            win!(self, wi).selection = Some(Selection {
                session_idx: idx,
                anchor: (col, abs),
                focus: (col, abs),
                mode: if modifiers.alt_key() {
                    SelectionMode::Blockwise
                } else {
                    SelectionMode::Linewise
                },
            });
            win!(self, wi).selection_dragging = true;
            win!(self, wi).selection_tape = tape;
            self.set_focus(wi, idx);
            win!(self, wi).needs_render = true;
            return true;
        }
        false
    }
}
