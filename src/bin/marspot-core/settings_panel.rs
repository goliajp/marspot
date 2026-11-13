//! The settings panel's input: what the pointer, a press and a key do
//! to its surface, and what the surface asks for in return.

use golia_ui_core::{Action, Key, KeyResult};
use marspot::input::{LogicalKey, NamedKey};
use marspot::ui::components::settings_view::{self, SettingsRender, SettingsSurface};
use marspot::ui::components::surface_stack::PressOn;
use marspot::ui::tree::panel;

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

    /// The panel's surface, laid out as the next frame will show it.
    fn settings_surface(&mut self, wi: usize) -> std::rc::Rc<std::cell::RefCell<SettingsSurface>> {
        let sp = self.settings_render(wi);
        let top_inset = win!(self, wi).layout.top_inset;
        sp.surface.borrow_mut().lay_out(self.renderer.font_mut(), &sp, top_inset);
        sp.surface
    }

    /// Follow the pointer over the panel.
    pub(super) fn settings_hover(&mut self, wi: usize, x: f64, y: f64) {
        let surface = self.settings_surface(wi);
        if surface.borrow_mut().kept.surface.pointer_move((x, y)) {
            win!(self, wi).needs_render = true;
        }
    }

    /// Which part of the panel a press at `(x, y)` is on.
    pub(super) fn settings_press_on(&mut self, wi: usize, x: f64, y: f64) -> PressOn {
        let frame = self.settings_modal_rect(wi);
        let surface = self.settings_surface(wi);
        let hit = surface.borrow().kept.surface.hit((x, y)).map(|h| h.action);
        match hit {
            Some(a) if a == panel::CLOSE => PressOn::Close,
            Some(a) if a == panel::DRAG => PressOn::Header,
            _ if frame.contains(x, y) => PressOn::Content,
            _ => PressOn::Outside,
        }
    }

    /// A press on the panel's content: the control under it acts.
    pub(super) fn settings_content_press(&mut self, wi: usize, x: f64, y: f64) {
        let surface = self.settings_surface(wi);
        let action = {
            let mut s = surface.borrow_mut();
            s.kept.surface.pointer_down((x, y));
            s.kept.surface.pointer_up((x, y))
        };
        if let Some(action) = action {
            self.settings_act(wi, action);
        }
    }

    /// A key while the panel has the keyboard: Tab moves the focus,
    /// Space and Enter act on it, the arrows move inside a control.
    pub(super) fn settings_key(&mut self, wi: usize, event: &MarspotKeyEvent, modifiers: Modifiers) {
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
        let surface = self.settings_surface(wi);
        let (result, scroll) = {
            let mut s = surface.borrow_mut();
            let result = s.kept.surface.key(key);
            (result, s.kept.surface.interaction.scroll[settings_view::SCROLL as usize])
        };
        let w = &mut win!(self, wi);
        // the focus may have brought a row into view
        w.settings_modal_scroll = marspot::ui::Units::from_px(scroll);
        w.needs_render = true;
        if let KeyResult::Action(action) = result {
            self.settings_act(wi, action);
        }
    }

    /// Do what a control of the panel asked for.
    fn settings_act(&mut self, wi: usize, action: Action) {
        if action == panel::CLOSE {
            let change = win!(self, wi).panels.close(CloseBy::CloseButton);
            self.apply_surface_change(wi, change);
            return;
        }
        if action.id != settings_view::CHOOSE {
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
