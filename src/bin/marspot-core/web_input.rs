//! The person's input on a web pane: the wheel and keys of the focused
//! pane, the pointer over a page, copy and paste, and what the page says
//! back (the pointer's shape, copied text, a link to open in a pane of
//! its own).  Whether each pane is on screen and how large it is follow
//! the layout.  Marspot's own chords have run before a key gets here;
//! points are turned into the page's CSS px.

use madcawl_wire::input::{Action, Cursor};
use marspot::pane::PaneBackend;
use marspot::shell_proto::{encode_pointer_shape, PointerShape};
use marspot::web_pane::WebPane;

use super::page_rect;
use super::*;

/// How far a line of a wheel that is not a trackpad scrolls a page, CSS px.
const WHEEL_LINE: f64 = 40.0;

fn web(p: &mut Pane) -> Option<&mut WebPane> {
    match p.session_mut() {
        PaneBackend::Web(w) => Some(w),
        _ => None,
    }
}

impl CoreApp {
    /// The web pane of window `wi` whose page is under `(x, y)`, and the
    /// point in the page's CSS px.
    fn web_at(&self, wi: usize, x: f64, y: f64) -> Option<(usize, f64, f64)> {
        let w = &win!(self, wi);
        let d = marspot::layout::density();
        w.panes.iter().enumerate().take(w.layout.cells.len()).find_map(|(i, p)| {
            let PaneBackend::Web(_) = p.session() else { return None };
            let (px, py, pw, ph) = page_rect(&w.layout, i)?;
            let inside = x >= px && x < px + pw && y >= py && y < py + ph;
            inside.then(|| (i, (x - px) / d, (y - py) / d))
        })
    }

    /// The point `(x, y)` in the CSS px of pane `i`'s page, inside it or not.
    fn web_point(&self, wi: usize, i: usize, x: f64, y: f64) -> Option<(f64, f64)> {
        let (px, py, _, _) = page_rect(&win!(self, wi).layout, i)?;
        let d = marspot::layout::density();
        Some(((x - px) / d, (y - py) / d))
    }

    /// The wheel: a focused web pane's page scrolls, both ways; anything
    /// else goes where it always did.
    pub(crate) fn scroll_xy(&mut self, wi: usize, dx: f64, dy: f64, precise: bool) {
        let (invert, _) = marspot::ui::scroll_config();
        let sign = if invert { 1.0 } else { -1.0 };
        let per = if precise { 1.0 / marspot::layout::density() } else { WHEEL_LINE };
        let w = &mut win!(self, wi);
        let i = w.focused_idx;
        if !w.panels.owns_pointer()
            && let Some(page) = w.panes.get_mut(i).and_then(web)
        {
            page.scroll(sign * dx * per, sign * dy * per);
            return;
        }
        self.scroll(wi, dy, precise);
    }

    /// A press on a page: the pane takes the focus and the page the press.
    pub(crate) fn web_press(&mut self, wi: usize, x: f64, y: f64, mods: Modifiers) -> bool {
        let Some((i, cx, cy)) = self.web_at(wi, x, y) else { return false };
        if win!(self, wi).focused_idx != i {
            self.focus_pane(wi, i);
        }
        if let Some(page) = win!(self, wi).panes.get_mut(i).and_then(web) {
            page.pressed = true;
            page.pointer(Action::Down, cx, cy, mods);
        }
        true
    }

    /// The pointer moved with a button down: the page that took the
    /// press follows it, out of the page too.
    pub(crate) fn web_drag(&mut self, wi: usize, x: f64, y: f64) -> bool {
        self.web_held(wi, x, y, Action::Move)
    }

    pub(crate) fn web_release(&mut self, wi: usize, x: f64, y: f64) -> bool {
        self.web_held(wi, x, y, Action::Up)
    }

    fn web_held(&mut self, wi: usize, x: f64, y: f64, action: Action) -> bool {
        let w = &win!(self, wi);
        let held = w.panes.iter().position(|p| matches!(p.session(), PaneBackend::Web(page) if page.pressed));
        let Some(i) = held else { return false };
        let Some((cx, cy)) = self.web_point(wi, i, x, y) else { return false };
        if let Some(page) = win!(self, wi).panes.get_mut(i).and_then(web) {
            page.pressed = action != Action::Up;
            page.pointer(action, cx, cy, Modifiers::default());
        }
        true
    }

    /// The pointer over a window: a page under it hears of it; one it
    /// left gets the arrow back.
    pub(crate) fn web_hover(&mut self, wi: usize, x: f64, y: f64) -> bool {
        let at = self.web_at(wi, x, y);
        let id = win!(self, wi).window_id;
        let mut left = false;
        for (i, p) in win!(self, wi).panes.iter_mut().enumerate() {
            let Some(page) = web(p) else { continue };
            let over = at.is_some_and(|(j, _, _)| j == i);
            left |= page.hovered && !over;
            page.hovered = over;
        }
        if left {
            self.pending_to_shell.push((MsgType::PointerShape, encode_pointer_shape(PointerShape::Arrow, id)));
        }
        let Some((i, cx, cy)) = at else { return false };
        if let Some(page) = win!(self, wi).panes.get_mut(i).and_then(web) {
            page.pointer(Action::Move, cx, cy, Modifiers::default());
        }
        true
    }

    /// A key for the focused pane, when it is a web pane.
    pub(crate) fn key_to_web(&mut self, wi: usize, event: &MarspotKeyEvent, mods: Modifiers) -> bool {
        let w = &mut win!(self, wi);
        let i = w.focused_idx;
        let Some(page) = w.panes.get_mut(i).and_then(web) else { return false };
        page.key(event, mods);
        true
    }

    /// Cmd-C on a focused web pane asks the page for its selection.
    pub(crate) fn web_copy(&mut self, wi: usize) -> bool {
        let w = &mut win!(self, wi);
        let i = w.focused_idx;
        let Some(page) = w.panes.get_mut(i).and_then(web) else { return false };
        page.copy();
        true
    }

    /// What web pane `id`'s page said.
    pub(crate) fn web_said(&mut self, id: u64) {
        let found = self.windows.iter_mut().enumerate().find_map(|(wi, w)| {
            let id_of = w.window_id;
            w.panes.iter_mut().filter_map(web).find(|p| p.id() == id).map(|p| (wi, id_of, p.hovered, p.take_said()))
        });
        let Some((wi, window_id, hovered, said)) = found else { return };
        if let Some(c) = said.cursor.filter(|_| hovered) {
            let shape = match c {
                Cursor::Default => PointerShape::Arrow,
                Cursor::Pointer => PointerShape::Hand,
                Cursor::Text => PointerShape::IBeam,
            };
            lx_info!("web_pane.cursor", "the pointer's shape over a web pane", session = id, shape = format!("{shape:?}"));
            self.pending_to_shell.push((MsgType::PointerShape, encode_pointer_shape(shape, window_id)));
        }
        if let Some(text) = said.copied {
            lx_event!("WEB_PANE_COPIED", "a web pane's selection went to the clipboard", session = id, chars = text.chars().count());
            marspot::input::write_clipboard_text(&text);
        }
        if let Some(url) = said.open {
            lx_event!("WEB_PANE_OPEN_LINK", "a link asked to open in a web pane of its own", session = id);
            if marspot::web_pane::entry_points() {
                self.open_web_pane(wi, url);
            }
        }
    }

    /// Each web pane of window `wi` at its cell's size, and told whether
    /// it is on screen: in a drawn cell of a window that is not minimized.
    pub(crate) fn web_follow_layout(&mut self, wi: usize) {
        let shown = !self.pane_control.shadows.minimized.contains(&win!(self, wi).window_id);
        let scale = (marspot::layout::density() * 100.0).round() as u32;
        let w = &mut win!(self, wi);
        let cells = w.layout.cells.len();
        for i in 0..w.panes.len() {
            let rect = page_rect(&w.layout, i).filter(|_| i < cells);
            let Some(page) = web(&mut w.panes[i]) else { continue };
            if let Some((_, _, pw, ph)) = rect {
                page.resize_px((pw, ph), scale);
            }
            page.set_visible(shown && rect.is_some());
        }
    }
}
