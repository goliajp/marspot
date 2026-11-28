//! What stands beside a context menu: the menu a row opens, and the
//! reason a disabled row is off.
//!
//! A row that opens a menu opens it when the pointer comes onto it,
//! when it is pressed, and from the keyboard with the right arrow or
//! Enter; the pointer onto another row of the menu, or the left arrow,
//! closes it.  The menu beside stands against the menu's right edge
//! level with its row, or against its left edge where the window ends.
//! The reason is said under its row while the pointer is on it.

use golia_ui_core::{Key, KeyResult, Target};

use super::*;

/// What a key did to the menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKey {
    /// Not one the menu has a use for.
    Ignored,
    /// The keyboard moved, or the menu beside closed.
    Moved,
    /// The row the keyboard is on opened its menu beside: once that is
    /// laid out, [`MenuSurface::key_into_sub`] takes the keyboard there.
    Opened,
    /// A row acts.
    Item(usize),
    /// A row of the menu beside row `.0` acts.
    SubItem(usize, usize),
}

/// The size of a menu of `items`, as `frame_of` makes it.
fn size_of(window: (f64, f64), items: &[MenuItem]) -> (f64, f64) {
    let f = context_menu::frame_of(window.0, window.1, crate::ui::chrome_scale(), 0.0, 0.0, 0.0, items);
    (f.w, f.h)
}

impl MenuSurface {
    /// Where the menu open beside it and the reason said stand, for the
    /// menu and rows placed last.
    pub(super) fn place_beside(&mut self, items: &[MenuItem], window: (f64, f64), top: f64) {
        let row = |i: Option<usize>| i.and_then(|i| self.rows.get(i).copied());
        self.sub_frame = row(self.open).zip(self.open.and_then(|i| items.get(i))).map(|(r, it)| {
            let (w, h) = size_of(window, &it.submenu);
            let right = self.frame.x + self.frame.w;
            let x = if right + w <= window.0 { right } else { (self.frame.x - w).max(0.0) };
            let y = (r.y_top - chrome::px(chrome::MENU_PAD_Y)).min(window.1 - h).max(top);
            Rect { x, y_top: y, w, h }
        });
        self.tip_frame = row(self.tip).zip(self.tip.and_then(|i| items.get(i))).map(|(r, it)| {
            // as wide as the reason, which a menu's widest row is not held to
            let pad = chrome::px(chrome::MENU_PAD_X);
            let w = (context_menu::label_w(&it.tooltip) + 2.0 * pad).min(window.0);
            let h = chrome::px(chrome::MENU_ROW_H) + 2.0 * chrome::px(chrome::MENU_PAD_Y);
            let x = (r.x + chrome::px(chrome::MENU_PAD_X)).min(window.0 - w).max(0.0);
            let below = r.y_top + r.h;
            let y = if below + h <= window.1 { below } else { (r.y_top - h).max(top) };
            Rect { x, y_top: y, w, h }
        });
    }

    /// The menu's row the point is on, if any.
    fn row_at(&self, x: f64, y: f64) -> Option<usize> {
        self.rows.iter().position(|r| r.contains(x, y))
    }

    /// The items the menu was last described from.
    fn items(&self) -> Option<Rc<[MenuItem]>> {
        self.described.as_ref().map(|d| d.items.clone())
    }

    /// The pointer is at `(x, y)`.  True when the picture changed: a
    /// row lit or went dark, a menu beside opened or closed, or a
    /// reason came or went.
    pub fn pointer_move(&mut self, x: f64, y: f64) -> bool {
        let lit = self.kept.surface.pointer_move((x, y));
        let Some(items) = self.items() else { return lit };
        let (open, tip) = (self.open, self.tip);
        let on_sub = self.sub_frame.is_some_and(|f| f.contains(x, y));
        if !on_sub && let Some(i) = self.row_at(x, y) {
            let it = &items[i];
            self.open = it.opens().then_some(i);
            self.tip = (!it.enabled && !it.tooltip.is_empty()).then_some(i);
        } else {
            self.tip = None;
        }
        lit || open != self.open || tip != self.tip
    }

    /// What a press at `(x, y)` lands on: a row that acts, the menu
    /// where nothing acts (a divider, a disabled row, its padding, a
    /// row that opens a menu, which opens it), or the window outside
    /// both menus.
    pub fn press(&mut self, x: f64, y: f64) -> ContextMenuHit {
        let on_sub = self.sub_frame.is_some_and(|f| f.contains(x, y));
        if !on_sub && !self.frame.contains(x, y) {
            return ContextMenuHit::Outside;
        }
        let s = &mut self.kept.surface;
        s.pointer_down((x, y));
        match s.pointer_up((x, y)) {
            Some(a) if a.id == SUB_ROW => self.open.map_or(ContextMenuHit::Frame, |o| ContextMenuHit::SubItem(o, a.arg as usize)),
            Some(a) if a.id == ROW => {
                let i = a.arg as usize;
                if self.items().is_some_and(|it| it[i].opens()) {
                    self.open = Some(i);
                    ContextMenuHit::Frame
                } else {
                    ContextMenuHit::Item(i)
                }
            }
            _ => ContextMenuHit::Frame,
        }
    }

    /// The row the keyboard is on: in the menu beside (`true`) or not,
    /// and its index.
    fn keyed(&self) -> Option<(bool, usize)> {
        let t = self.kept.surface.interaction.focused?;
        let n = self.kept.surface.tree().nodes().iter().find(|n| n.widget == Some(t.id) && n.item == t.item && n.leaf_tag == preset::MENU_ROW)?;
        Some((t.id == SUB_ROWS, n.action.arg as usize))
    }

    /// A key while the menu is open.  A menu beside that the key opens
    /// is laid out with the next layout; [`Self::key_into_sub`] then
    /// takes the keyboard into it.
    pub fn key(&mut self, key: Key) -> MenuKey {
        let Some(items) = self.items() else { return MenuKey::Ignored };
        let keyed = self.keyed();
        match (key, keyed) {
            (Key::Up | Key::Down, None) => self.answer(Key::Tab),
            (Key::Right | Key::Enter | Key::Space, Some((false, i))) if items[i].opens() => {
                self.open = Some(i);
                MenuKey::Opened
            }
            (Key::Left, Some((true, _))) => {
                if let Some(o) = self.open.take() {
                    self.key_onto(ROWS, ROW, o);
                }
                MenuKey::Moved
            }
            (Key::Up | Key::Down | Key::Home | Key::End | Key::Enter | Key::Space, Some(_)) => self.answer(key),
            _ => MenuKey::Ignored,
        }
    }

    /// The keyboard onto the first row of the menu open beside, once it
    /// is laid out.
    pub fn key_into_sub(&mut self) {
        if self.open.is_some() && self.keyed().is_some_and(|(sub, _)| !sub) {
            self.kept.surface.key(Key::Tab);
        }
    }

    /// The keyboard onto row `i` of the rows `id`, whose action is
    /// `action`.
    fn key_onto(&mut self, id: golia_ui_core::WidgetId, action: u32, i: usize) {
        let s = &mut self.kept.surface;
        let item = s.tree().nodes().iter().find(|n| n.widget == Some(id) && n.action.id == action && n.action.arg == i as u32).map(|n| n.item);
        if let Some(item) = item {
            s.interaction.focused = Some(Target { id, item, part: 0 });
            s.interaction.focus_visible = true;
        }
    }

    fn answer(&mut self, key: Key) -> MenuKey {
        match self.kept.surface.key(key) {
            KeyResult::Ignored => MenuKey::Ignored,
            KeyResult::Handled => MenuKey::Moved,
            KeyResult::Action(a) if a.id == SUB_ROW => self.open.map_or(MenuKey::Ignored, |o| MenuKey::SubItem(o, a.arg as usize)),
            KeyResult::Action(a) => MenuKey::Item(a.arg as usize),
        }
    }
}
