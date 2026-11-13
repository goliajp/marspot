//! Input for the panels that are drawn from an interface tree: what
//! the pointer, a press and a key do to the open panel's surface, and
//! what the surface asks for in return.

use golia_ui_core::{Action, Key, KeyResult};
use marspot::input::{LogicalKey, NamedKey};
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
            SurfaceKind::Layout | SurfaceKind::Processes | SurfaceKind::Accounts => None,
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
        let action = self.with_tree_panel(wi, kind, |s| {
            s.surface.pointer_down((x, y));
            s.surface.pointer_up((x, y))
        });
        if let Some(Some(action)) = action {
            self.tree_panel_act(wi, action);
        }
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
        if action.id != settings_view::CHOOSE || win!(self, wi).panels.open() != Some(SurfaceKind::Settings) {
            return;
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
