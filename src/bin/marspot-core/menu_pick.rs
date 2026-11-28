//! A row of the open context menu picked, by the pointer or the
//! keyboard: what its tag asks for.  The keys the open menu takes.

use marspot::input::{KeyState, LogicalKey, NamedKey};
use marspot::ui::components::context_menu_view::MenuKey;

use super::*;

impl CoreApp {
    /// The row tagged `tag` of window `wi`'s open menu was picked.
    pub(crate) fn pick_menu_tag(&mut self, wi: usize, tag: u32) {
        let Some(region) = win!(self, wi).context_menu.as_ref().map(|m| m.region) else { return };
        // a badge menu carries its plugin's own tags
        if let ContextRegion::PaneBadge(sid) = region {
            self.pick_badge_row(wi, sid, tag);
            return;
        }
        if tag == MOVE_TO_NEW_WINDOW_TAG || (MOVE_TO_WINDOW_TAG_BASE..MOVE_TO_NEW_WINDOW_TAG).contains(&tag) {
            win!(self, wi).context_menu = None;
            win!(self, wi).needs_render = true;
            let to = (tag != MOVE_TO_NEW_WINDOW_TAG).then(|| (tag - MOVE_TO_WINDOW_TAG_BASE) as usize);
            match (region, to) {
                (ContextRegion::SidebarPane(sid), to) => self.move_other(sid, to),
                (_, None) => self.move_pane_to_new_window(wi, self.region_slot(wi, region)),
                (_, Some(to)) => self.move_pane_to_window(wi, self.region_slot(wi, region), to),
            }
        } else if let Some(action) = ContextMenuAction::from_tag(tag) {
            self.dispatch_context_action(wi, action, region);
        } else if !self.pick_family_tag(wi, region, tag) {
            win!(self, wi).context_menu = None;
            win!(self, wi).needs_render = true;
        }
    }

    /// The slot a menu opened on is about: its own, or the focused one.
    fn region_slot(&self, wi: usize, region: ContextRegion) -> usize {
        match region {
            ContextRegion::Pane(i) | ContextRegion::SidebarSlot(i) => i,
            _ => win!(self, wi).focused_idx,
        }
    }

    /// A key while window `wi` has a menu open: the arrows move along
    /// it and into and out of a menu beside, Enter and Space pick, Esc
    /// closes it.  Whether the menu took the key; it takes every key
    /// while it is open, so nothing typed reaches the pane under it.
    pub(crate) fn menu_key(&mut self, wi: usize, event: &MarspotKeyEvent) -> bool {
        if win!(self, wi).context_menu.is_none() {
            return false;
        }
        if event.state != KeyState::Pressed {
            return true;
        }
        // its release reaches nobody, the menu gone by then or not
        self.pane_control.shadows.peeks.held.swallowed(&event.logical);
        use golia_ui_core::Key;
        let key = match event.logical {
            LogicalKey::Named(NamedKey::Escape) => {
                win!(self, wi).context_menu = None;
                win!(self, wi).needs_render = true;
                return true;
            }
            LogicalKey::Named(NamedKey::ArrowUp) => Key::Up,
            LogicalKey::Named(NamedKey::ArrowDown) => Key::Down,
            LogicalKey::Named(NamedKey::ArrowLeft) => Key::Left,
            LogicalKey::Named(NamedKey::ArrowRight) => Key::Right,
            LogicalKey::Named(NamedKey::Home) => Key::Home,
            LogicalKey::Named(NamedKey::End) => Key::End,
            LogicalKey::Named(NamedKey::Enter) => Key::Enter,
            LogicalKey::Char(' ') => Key::Space,
            _ => return true,
        };
        let items = win!(self, wi).context_menu.as_ref().map(|m| m.items.clone()).unwrap_or_else(|| Vec::new().into());
        match self.with_menu(wi, |s| s.key(key)) {
            Some(MenuKey::Item(i)) => self.pick_menu_tag(wi, items[i].action_tag),
            Some(MenuKey::SubItem(i, j)) => self.pick_menu_tag(wi, items[i].submenu[j].action_tag),
            Some(MenuKey::Opened) => {
                // the menu beside is laid out now, and the keyboard goes into it
                self.with_menu(wi, |s| s.key_into_sub());
                win!(self, wi).needs_render = true;
            }
            Some(MenuKey::Moved) => win!(self, wi).needs_render = true,
            Some(MenuKey::Ignored) | None => {}
        }
        true
    }

    /// A row about whose child a pane is, or what a slot shows; false
    /// when `tag` is none of those.
    fn pick_family_tag(&mut self, _wi: usize, _region: ContextRegion, _tag: u32) -> bool {
        false
    }
}

#[cfg(test)]
#[path = "menu_pick_tests.rs"]
pub(crate) mod tests;
