//! What floats over one window's panes, and the rules for it.
//!
//! Every panel opens, closes, takes the pointer and moves the same
//! way, and this is the one place that says how.  The front-end tells
//! the stack what happened — a key, a press, a toggle — and does what
//! it answers; it keeps the panels' own data and nothing about when
//! they are open.
//!
//! The rules:
//!
//! - One panel at a time.  Opening one closes the one that was open.
//! - A panel closes on its close button, on Esc, on Cmd-W, on a press
//!   outside its frame, and on its own toggle.
//! - While a panel is open the pointer and the keyboard are its own:
//!   nothing reaches the panes underneath.
//! - A panel moves by its header, the close button excepted.  Its
//!   header stays inside the window, and it is back in the middle the
//!   next time it opens.
//!
//! No painter and no window types: rects in, decisions out.

use marspot_term::layout::Rect;

use crate::ui::components::panel_chrome::PanelChrome;

/// A surface the stack manages.
///
/// Only panels so far.  The context menu and the search overlay still
/// keep their own state in the front-end; moving one here is a new
/// variant, and with it the answers the rules above give for its kind
/// (a menu does not dim, an overlay shares the keyboard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Settings,
    Layout,
    Processes,
    Accounts,
}

impl SurfaceKind {
    pub const PANELS: [SurfaceKind; 4] =
        [Self::Settings, Self::Layout, Self::Processes, Self::Accounts];
}

/// The keys the stack tells apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Esc,
    CmdW,
    Other,
}

/// What a key does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Nothing is open: the key is the panes'.
    PassThrough,
    /// A panel is open and has no use for the key; it goes nowhere.
    Swallow,
    /// The panel closed.
    Close(SurfaceKind),
}

/// What a press of the primary button does while a panel is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// On the close button or outside the frame: the panel closed.
    /// Consumed either way — a press that dismisses a panel does not
    /// also land on what was under it.
    Close(SurfaceKind),
    /// On the header: a drag began.
    BeginDrag,
    /// On the panel's content, which decides what it means.
    Content(SurfaceKind),
}

/// What opening or closing changed, for the front-end to set up and
/// tear down the panels' own data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Change {
    pub closed: Option<SurfaceKind>,
    pub opened: Option<SurfaceKind>,
}

#[derive(Debug, Clone, Copy)]
struct Grab {
    pointer: (f64, f64),
    offset: (f64, f64),
    /// The panel's frame before it was ever moved.
    home: Rect,
}

/// The surfaces open in one window.
#[derive(Debug, Clone, Default)]
pub struct SurfaceStack {
    open: Option<SurfaceKind>,
    /// How far the open panel has been dragged from the middle.  One
    /// is enough: there is one panel, and it forgets on close.
    offset: (f64, f64),
    grab: Option<Grab>,
}

impl SurfaceStack {
    /// The open panel.
    pub fn open(&self) -> Option<SurfaceKind> {
        self.open
    }

    /// Does a surface own the pointer — moves, presses, the wheel?
    pub fn owns_pointer(&self) -> bool {
        self.open.is_some()
    }

    /// How far the open panel has been dragged; feed it to [`place`].
    pub fn offset(&self) -> (f64, f64) {
        self.offset
    }

    /// A toolbar button or a shortcut: closes `kind` if it is open,
    /// otherwise opens it in place of whatever was.
    pub fn on_toggle(&mut self, kind: SurfaceKind) -> Change {
        if self.open == Some(kind) { self.close() } else { self.show(kind) }
    }

    /// Opens `kind` in place of whatever was open.  Nothing changes if
    /// it already is.
    pub fn show(&mut self, kind: SurfaceKind) -> Change {
        if self.open == Some(kind) {
            return Change::default();
        }
        let closed = self.close().closed;
        self.open = Some(kind);
        Change { closed, opened: Some(kind) }
    }

    /// Closes whatever is open.
    pub fn close(&mut self) -> Change {
        self.offset = (0.0, 0.0);
        self.grab = None;
        Change { closed: self.open.take(), opened: None }
    }

    pub fn on_key(&mut self, key: Key) -> KeyAction {
        match (self.open, key) {
            (None, _) => KeyAction::PassThrough,
            (Some(kind), Key::Esc | Key::CmdW) => {
                self.close();
                KeyAction::Close(kind)
            }
            (Some(_), Key::Other) => KeyAction::Swallow,
        }
    }

    /// A press at `(x, y)` with the open panel's chrome at `chrome`.
    /// `None` when nothing is open and the press is the panes'.
    pub fn on_press(&mut self, x: f64, y: f64, chrome: &PanelChrome) -> Option<Press> {
        let kind = self.open?;
        Some(if chrome.hit_close(x, y) || !chrome.frame.contains(x, y) {
            self.close();
            Press::Close(kind)
        } else if chrome.hit_header(x, y) {
            let f = chrome.frame;
            let home = Rect { x: f.x - self.offset.0, y_top: f.y_top - self.offset.1, ..f };
            self.grab = Some(Grab { pointer: (x, y), offset: self.offset, home });
            Press::BeginDrag
        } else {
            Press::Content(kind)
        })
    }

    /// The pointer moved to `(x, y)` with the button held.  True when
    /// a header drag took it.
    ///
    /// The offset kept is how far the panel actually went, not how far
    /// the pointer did: a drag past an edge does not have to be undone
    /// before the panel moves again.
    pub fn on_drag(&mut self, x: f64, y: f64, window_w: f64, window_h: f64, top_inset: f64) -> bool {
        let Some(g) = self.grab else { return false };
        let asked = (g.offset.0 + x - g.pointer.0, g.offset.1 + y - g.pointer.1);
        let at = place(g.home, asked, window_w, window_h, top_inset);
        self.offset = (at.x - g.home.x, at.y_top - g.home.y_top);
        true
    }

    /// The button came up.
    pub fn on_release(&mut self) {
        self.grab = None;
    }

    pub fn dragging(&self) -> bool {
        self.grab.is_some()
    }
}

/// Where a panel whose frame is `home` when untouched sits after being
/// dragged by `offset`: moved, but with its whole header still inside
/// the window and under the window's own chrome.
pub fn place(home: Rect, offset: (f64, f64), window_w: f64, window_h: f64, top_inset: f64) -> Rect {
    // An untouched panel is where its own layout put it, even in a
    // window too small for the bounds below to hold.
    if offset == (0.0, 0.0) {
        return home;
    }
    let header_h = PanelChrome::new(home, top_inset).header.h;
    let right = (window_w - home.w).max(0.0);
    let bottom = (window_h - header_h).max(top_inset);
    Rect {
        x: (home.x + offset.0).clamp(0.0, right).round(),
        y_top: (home.y_top + offset.1).clamp(top_inset, bottom).round(),
        ..home
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SurfaceKind::*;

    const WINDOW: (f64, f64) = (1600.0, 1000.0);
    const TOP: f64 = 60.0;

    fn home() -> Rect {
        Rect { x: 400.0, y_top: 200.0, w: 800.0, h: 600.0 }
    }

    fn chrome(stack: &SurfaceStack) -> PanelChrome {
        PanelChrome::new(place(home(), stack.offset(), WINDOW.0, WINDOW.1, TOP), TOP)
    }

    fn drag(s: &mut SurfaceStack, x: f64, y: f64) -> bool {
        s.on_drag(x, y, WINDOW.0, WINDOW.1, TOP)
    }

    fn opened(kind: SurfaceKind) -> SurfaceStack {
        let mut s = SurfaceStack::default();
        s.show(kind);
        s
    }

    #[test]
    fn nothing_open_owns_nothing() {
        let mut s = SurfaceStack::default();
        assert_eq!(s.open(), None);
        assert!(!s.owns_pointer());
        for key in [Key::Esc, Key::CmdW, Key::Other] {
            assert_eq!(s.on_key(key), KeyAction::PassThrough, "{key:?}");
        }
        assert_eq!(s.on_press(500.0, 500.0, &chrome(&s)), None);
        assert!(!drag(&mut s, 1.0, 1.0));
        assert_eq!(s.close(), Change::default());
    }

    #[test]
    fn a_toggle_opens_closes_and_replaces() {
        for kind in SurfaceKind::PANELS {
            let mut s = SurfaceStack::default();
            assert_eq!(s.on_toggle(kind), Change { closed: None, opened: Some(kind) });
            assert_eq!(s.open(), Some(kind));
            assert!(s.owns_pointer());
            assert_eq!(s.on_toggle(kind), Change { closed: Some(kind), opened: None });
            assert_eq!(s.open(), None);

            for other in SurfaceKind::PANELS.into_iter().filter(|o| *o != kind) {
                let mut s = opened(other);
                assert_eq!(
                    s.on_toggle(kind),
                    Change { closed: Some(other), opened: Some(kind) },
                    "{kind:?} over {other:?}"
                );
                assert_eq!(s.open(), Some(kind), "one panel at a time");
            }
        }
    }

    #[test]
    fn showing_what_is_open_changes_nothing() {
        let mut s = opened(Layout);
        s.on_press(600.0, 210.0, &chrome(&s));
        drag(&mut s, 650.0, 240.0);
        assert_eq!(s.show(Layout), Change::default());
        assert_eq!(s.offset(), (50.0, 30.0), "it stays where it was dragged");
        assert_eq!(s.show(Settings), Change { closed: Some(Layout), opened: Some(Settings) });
        assert_eq!(s.offset(), (0.0, 0.0));
    }

    #[test]
    fn keys_close_or_go_nowhere() {
        let table = [
            (Key::Esc, true),
            (Key::CmdW, true),
            (Key::Other, false),
        ];
        for kind in SurfaceKind::PANELS {
            for (key, closes) in table {
                let mut s = opened(kind);
                let want = if closes { KeyAction::Close(kind) } else { KeyAction::Swallow };
                assert_eq!(s.on_key(key), want, "{kind:?} {key:?}");
                assert_eq!(s.open().is_none(), closes, "{kind:?} {key:?}");
            }
        }
    }

    #[test]
    fn a_press_closes_drags_or_is_the_contents() {
        let c = PanelChrome::new(home(), TOP);
        let close = (c.close.x + c.close.w * 0.5, c.close.y_top + c.close.h * 0.5);
        let table: [(&str, (f64, f64), fn(SurfaceKind) -> Press); 7] = [
            ("close button", close, Press::Close),
            ("left of the frame", (399.0, 500.0), Press::Close),
            ("above the frame", (800.0, 199.0), Press::Close),
            ("on the toolbar strip", (800.0, 10.0), Press::Close),
            ("header", (600.0, 210.0), |_| Press::BeginDrag),
            ("content", (800.0, 500.0), Press::Content),
            ("bottom padding", (800.0, 799.0), Press::Content),
        ];
        for kind in SurfaceKind::PANELS {
            for (what, (x, y), want) in table {
                let mut s = opened(kind);
                let want = want(kind);
                assert_eq!(s.on_press(x, y, &c), Some(want), "{kind:?} {what}");
                assert_eq!(s.open().is_none(), matches!(want, Press::Close(_)), "{kind:?} {what}");
                assert_eq!(s.dragging(), want == Press::BeginDrag, "{kind:?} {what}");
            }
        }
    }

    #[test]
    fn the_close_button_is_not_a_drag_handle() {
        let mut s = opened(Processes);
        let c = chrome(&s);
        assert!(c.header.contains(c.close.x + 1.0, c.close.y_top + 1.0));
        assert_eq!(s.on_press(c.close.x + 1.0, c.close.y_top + 1.0, &c), Some(Press::Close(Processes)));
        assert!(!s.dragging());
    }

    #[test]
    fn a_header_drag_moves_the_panel_and_a_close_forgets_it() {
        for kind in SurfaceKind::PANELS {
            let mut s = opened(kind);
            assert_eq!(s.on_press(600.0, 210.0, &chrome(&s)), Some(Press::BeginDrag));
            assert!(drag(&mut s, 650.4, 239.6));
            assert_eq!(s.offset(), (50.0, 30.0), "whole pixels");
            s.on_release();
            assert!(!s.dragging());
            assert!(!drag(&mut s, 900.0, 900.0), "the button is up");

            // A second drag starts from where the first one ended.
            let c = chrome(&s);
            assert_eq!((c.frame.x, c.frame.y_top), (450.0, 230.0));
            assert_eq!(s.on_press(650.0, 240.0, &c), Some(Press::BeginDrag));
            drag(&mut s, 640.0, 240.0);
            assert_eq!(s.offset(), (40.0, 30.0));

            assert_eq!(s.on_key(Key::Esc), KeyAction::Close(kind));
            assert_eq!(s.offset(), (0.0, 0.0), "{kind:?} reopens in the middle");
            assert!(!s.dragging());
            s.show(kind);
            assert_eq!(chrome(&s).frame, home());
        }
    }

    #[test]
    fn a_dragged_panel_keeps_its_header_in_the_window() {
        let at = |dx: f64, dy: f64| place(home(), (dx, dy), WINDOW.0, WINDOW.1, TOP);
        let header_h = PanelChrome::new(home(), TOP).header.h;
        let table = [
            ((0.0, 0.0), (400.0, 200.0)),
            ((50.0, 30.0), (450.0, 230.0)),
            ((-1e6, 0.0), (0.0, 200.0)),
            ((1e6, 0.0), (WINDOW.0 - 800.0, 200.0)),
            ((0.0, -1e6), (400.0, TOP)),
            ((0.0, 1e6), (400.0, WINDOW.1 - header_h)),
        ];
        for (offset, want) in table {
            let r = at(offset.0, offset.1);
            assert_eq!((r.x, r.y_top), want, "{offset:?}");
            assert_eq!((r.w, r.h), (800.0, 600.0), "{offset:?}");
        }
    }

    #[test]
    fn an_untouched_panel_is_left_where_its_layout_put_it() {
        // Taller than the window: outside the bounds a drag is held to.
        let tight = Rect { x: -10.0, y_top: 5.0, w: 800.0, h: 600.0 };
        assert_eq!(place(tight, (0.0, 0.0), 700.0, 300.0, TOP), tight);
    }

    #[test]
    fn a_drag_past_the_edge_keeps_only_what_moved_the_panel() {
        let mut s = opened(Accounts);
        s.on_press(600.0, 210.0, &chrome(&s));
        drag(&mut s, 600.0 - 5000.0, 210.0);
        assert_eq!(s.offset(), (-400.0, 0.0), "stopped at the window's left edge");
        // Still the same drag: back to where it was grabbed is back home.
        drag(&mut s, 600.0, 210.0);
        assert_eq!(s.offset(), (0.0, 0.0));
        drag(&mut s, 600.0 - 5000.0, 210.0);
        s.on_release();
        s.on_press(200.0, 210.0, &chrome(&s));
        drag(&mut s, 210.0, 210.0);
        assert_eq!(s.offset(), (-390.0, 0.0), "the next drag moves it at once");
    }
}
