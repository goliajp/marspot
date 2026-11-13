//! Input for the panels that are drawn from an interface tree: what
//! the pointer, a press and a key do to the open panel's surface, and
//! what the surface asks for in return.

use golia_ui_core::{Action, Key, KeyResult};
use marspot::input::{LogicalKey, NamedKey};
use marspot::ui::components::layout_view::{LayoutModalDragRender, LayoutModalRender};
use marspot::ui::components::settings_view::{self, SettingsRender};
use marspot::ui::components::shortcuts_view::ShortcutsRender;
use marspot::ui::components::surface_stack::PressOn;
use marspot::ui::tree::{panel, KeptSurface};

use super::*;

impl CoreApp {
    pub(super) fn settings_modal_rect(&mut self, wi: usize) -> marspot_term::layout::Rect {
        let (w_phys, h_phys, top_inset) = (
            win!(self, wi).w_phys,
            win!(self, wi).h_phys,
            win!(self, wi).layout.top_inset,
        );
        let mut measure = marspot::ui::components::controls::font_measure(self.renderer.font_mut());
        let home = marspot::ui::components::settings_modal::panel_rect(
            w_phys, h_phys, &marspot::settings::get(), &mut measure, top_inset,
        );
        surface_stack::place(home, win!(self, wi).panels.offset(), w_phys, h_phys, top_inset)
    }

    /// What a frame of the open settings panel is drawn from.
    pub(super) fn settings_render(&mut self, wi: usize) -> SettingsRender {
        let rect = self.settings_modal_rect(wi);
        let w = &win!(self, wi);
        SettingsRender {
            rect,
            scroll: w.settings_modal_scroll.px(),
            settings: marspot::settings::get(),
            path: w.settings_path.clone(),
            pointer: None,
            surface: w.settings_surface.clone(),
        }
    }

    /// What a frame of the open shortcuts panel is drawn from.
    pub(super) fn shortcuts_render(&mut self, wi: usize) -> ShortcutsRender {
        ShortcutsRender {
            rect: self.shortcuts_panel_rect(wi),
            pointer: None,
            surface: win!(self, wi).shortcuts_surface.clone(),
        }
    }

    /// The surface of the open panel `kind`, laid out as the next
    /// frame will show it.  `None` for a panel not drawn from a tree.
    fn with_tree_panel<T>(
        &mut self,
        wi: usize,
        kind: SurfaceKind,
        f: impl FnOnce(&mut KeptSurface) -> T,
    ) -> Option<T> {
        let top_inset = win!(self, wi).layout.top_inset;
        match kind {
            SurfaceKind::Settings => {
                let sp = self.settings_render(wi);
                let mut s = sp.surface.borrow_mut();
                s.lay_out(self.renderer.font_mut(), &sp, top_inset);
                Some(f(&mut s.kept))
            }
            SurfaceKind::Shortcuts => {
                let sp = self.shortcuts_render(wi);
                let mut s = sp.surface.borrow_mut();
                s.lay_out(self.renderer.font_mut(), &sp, top_inset);
                Some(f(&mut s.kept))
            }
            SurfaceKind::Layout => {
                let state = self.layout_render(wi);
                let window = (win!(self, wi).w_phys, win!(self, wi).h_phys);
                let mut s = state.surface.borrow_mut();
                s.lay_out(self.renderer.font_mut(), &state, window, top_inset);
                Some(f(&mut s.kept))
            }
            // against the frame last drawn: the feed is read where a
            // frame is built, not where the pointer moves
            SurfaceKind::Accounts => {
                let surface = win!(self, wi).cc_usage_modal.as_ref()?.surface.clone();
                let mut s = surface.borrow_mut();
                Some(f(&mut s.kept))
            }
            SurfaceKind::Processes => {
                let surface = win!(self, wi).process_panel.as_ref()?.surface.clone();
                let mut s = surface.borrow_mut();
                Some(f(&mut s.kept))
            }
        }
    }

    /// Follow the pointer over the panel.
    pub(super) fn tree_panel_hover(&mut self, wi: usize, kind: SurfaceKind, x: f64, y: f64) {
        if self.with_tree_panel(wi, kind, |s| s.surface.pointer_move((x, y))) == Some(true) {
            win!(self, wi).needs_render = true;
        }
    }

    /// Which part of the panel a press at `(x, y)` is on.
    pub(super) fn tree_panel_press_on(&mut self, wi: usize, kind: SurfaceKind, x: f64, y: f64) -> PressOn {
        let frame = self.panel_frame(wi, kind);
        let hit = self.with_tree_panel(wi, kind, |s| s.surface.hit((x, y)).map(|h| h.action)).flatten();
        match hit {
            Some(a) if a == panel::CLOSE => PressOn::Close,
            Some(a) if a == panel::DRAG => PressOn::Header,
            _ if frame.contains(x, y) => PressOn::Content,
            _ => PressOn::Outside,
        }
    }

    /// A press on the panel's content: the control under it acts.
    pub(super) fn tree_panel_content_press(&mut self, wi: usize, kind: SurfaceKind, x: f64, y: f64) {
        if let Some(action) = self.tree_panel_action_at(wi, kind, x, y) {
            self.tree_panel_act(wi, action);
        }
    }

    /// What a press at `(x, y)` on the panel's content asks for.
    pub(super) fn tree_panel_action_at(&mut self, wi: usize, kind: SurfaceKind, x: f64, y: f64) -> Option<Action> {
        self.with_tree_panel(wi, kind, |s| {
            s.surface.pointer_down((x, y));
            s.surface.pointer_up((x, y))
        })
        .flatten()
    }

    /// Do what a row or a button of the process monitor asked for.
    pub(super) fn process_panel_do(&mut self, wi: usize, on: ProcessAsk) {
        let Some(panel) = win!(self, wi).process_panel.as_mut() else { return };
        match on {
            ProcessAsk::Kill(row) => {
                if let Some(pid) = panel.row_pids.get(row).copied() {
                    self.kill_and_track(wi, pid);
                    self.refresh_process_panel(wi);
                }
            }
            ProcessAsk::Pane(row) => {
                panel.selected_pane = row;
                panel.scroll_y = Default::default();
            }
            ProcessAsk::Process(_) | ProcessAsk::Close | ProcessAsk::None => {}
        }
        win!(self, wi).needs_render = true;
    }

    /// What a frame of the open layout panel is drawn from.
    pub(super) fn layout_render(&mut self, wi: usize) -> LayoutModalRender {
        let w = &win!(self, wi);
        LayoutModalRender {
            cols: w.pending_grid_cols,
            rows: w.pending_grid_rows,
            slot_titles: w.layout_titles.clone(),
            drag: w.layout_drag.map(|d| LayoutModalDragRender {
                from_slot: d.from_slot,
                grab_offset_phys: d.grab_offset_phys,
                mouse_phys: d.mouse_phys,
            }),
            pointer: None,
            offset: w.panels.offset(),
            surface: w.layout_surface.clone(),
        }
    }

    /// Do what a control of the layout panel asked for; `at` is where
    /// the pointer was, for a card that is picked up.
    pub(super) fn layout_panel_do(&mut self, wi: usize, on: LayoutAsk, at: (f64, f64)) {
        use marspot::ui::components::layout_modal::{step, Axis};
        match on {
            LayoutAsk::Card(slot) => {
                let modal = self.layout_modal_geometry(wi);
                let Some(card) = modal.cards.get(slot) else { return };
                win!(self, wi).layout_drag = Some(LayoutModalDrag {
                    from_slot: slot,
                    grab_offset_phys: (at.0 - card.x, at.1 - card.y_top),
                    mouse_phys: at,
                });
            }
            LayoutAsk::Apply => {
                let change = win!(self, wi).panels.close(CloseBy::Content);
                self.apply_surface_change(wi, change);
                win!(self, wi).grid_cols = win!(self, wi).pending_grid_cols;
                win!(self, wi).grid_rows = win!(self, wi).pending_grid_rows;
                // The drag-reordered arrangement lands in the actual
                // grid.  Only the in-cells portion is reordered (panes
                // past the grid cells stay in sidebar order).
                let cells = win!(self, wi).grid_cols * win!(self, wi).grid_rows;
                self.apply_card_slot_permutation(wi, cells);
                // If the focused pane's slot is beyond the new cell
                // count, focus the last surviving cell so the user sees
                // a focused pane in the grid.  Overflowed sessions stay
                // alive in the sidebar.
                if cells > 0 && win!(self, wi).focused_idx >= cells {
                    win!(self, wi).focused_idx = cells - 1;
                }
                self.rebuild_layout(wi);
                self.save_session_state();
            }
            LayoutAsk::Step(axis, part) => {
                let w = &mut win!(self, wi);
                let value = match axis {
                    Axis::Columns => &mut w.pending_grid_cols,
                    Axis::Rows => &mut w.pending_grid_rows,
                };
                if let Some(next) = step(*value, part) {
                    *value = next;
                    self.reset_card_slots(wi);
                }
            }
            LayoutAsk::Close | LayoutAsk::None => {}
        }
        win!(self, wi).needs_render = true;
    }

    /// A key while a panel has the keyboard: Tab moves the focus,
    /// Space and Enter act on it, the arrows move inside a control.
    pub(super) fn tree_panel_key(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) {
        let Some(kind) = win!(self, wi).panels.open() else { return };
        if modifiers.control || modifiers.alt || modifiers.super_ {
            return;
        }
        let key = match event.logical {
            LogicalKey::Named(NamedKey::Tab) if modifiers.shift => Key::BackTab,
            LogicalKey::Named(NamedKey::Tab) => Key::Tab,
            LogicalKey::Char(' ') => Key::Space,
            LogicalKey::Named(NamedKey::Enter) => Key::Enter,
            LogicalKey::Named(NamedKey::ArrowLeft) => Key::Left,
            LogicalKey::Named(NamedKey::ArrowRight) => Key::Right,
            LogicalKey::Named(NamedKey::ArrowUp) => Key::Up,
            LogicalKey::Named(NamedKey::ArrowDown) => Key::Down,
            LogicalKey::Named(NamedKey::Home) => Key::Home,
            LogicalKey::Named(NamedKey::End) => Key::End,
            _ => return,
        };
        let Some((result, scroll)) = self.with_tree_panel(wi, kind, |s| {
            (s.surface.key(key), s.surface.interaction.scroll[settings_view::SCROLL as usize])
        }) else {
            return;
        };
        let w = &mut win!(self, wi);
        if kind == SurfaceKind::Settings {
            // the focus may have brought a row into view
            w.settings_modal_scroll = marspot::ui::Units::from_px(scroll);
        }
        w.needs_render = true;
        if let KeyResult::Action(action) = result {
            self.tree_panel_act(wi, action);
        }
    }

    /// Do what a control of the open panel asked for.
    fn tree_panel_act(&mut self, wi: usize, action: Action) {
        if action == panel::CLOSE {
            let change = win!(self, wi).panels.close(CloseBy::CloseButton);
            self.apply_surface_change(wi, change);
            return;
        }
        match win!(self, wi).panels.open() {
            Some(SurfaceKind::Settings) if action.id == settings_view::CHOOSE => {}
            Some(SurfaceKind::Processes) => {
                self.process_panel_do(wi, process_view::asked(action));
                return;
            }
            Some(SurfaceKind::Layout) => {
                // by a key: nothing is under the pointer to pick up
                if !matches!(layout_view::asked(action), LayoutAsk::Card(_)) {
                    self.layout_panel_do(wi, layout_view::asked(action), (0.0, 0.0));
                }
                return;
            }
            _ => return,
        }
        let Some((row, seg)) = settings_view::chosen(action.arg) else { return };
        let cur = marspot::settings::get();
        let Some(next) = row.apply(&cur, seg) else { return };
        // Write, then adopt.  If the disk write fails the panel must
        // keep showing what the file says, not what the click asked
        // for — a control that lies about having saved is worse than
        // one that does not move.
        match marspot::settings::write(&next) {
            Ok(()) => {
                marspot::settings::reload_if_changed();
                lx_event!(
                    "SETTINGS_CHANGED",
                    "settings panel wrote a new value",
                    row = format!("{:?}", row),
                    reclaim = next.reclaim_enabled as u32,
                    idle_min = next.reclaim_idle_minutes as u64,
                    prefetch = next.reclaim_prefetch as u32
                );
            }
            Err(e) => lx_warn!(
                "core.settings.write_failed",
                &format!("{e}; the panel keeps showing the file's values")
            ),
        }
        win!(self, wi).needs_render = true;
    }
}
