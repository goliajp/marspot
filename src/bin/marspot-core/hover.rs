//! What the pointer is on, beyond the main window's chrome: the open
//! panel's controls and the context menu's rows, and forgetting all of
//! it when the pointer is no longer over the window.

use super::*;

impl CoreApp {
    /// Follow the pointer over the open panel and the context menu.
    pub(super) fn hover_panels(&mut self, wi: usize, x_phys: f64, y_phys: f64) {
        if win!(self, wi).panels.open() == Some(SurfaceKind::Settings) {
            self.tree_panel_hover(wi, SurfaceKind::Settings, x_phys, y_phys);
        }
        if win!(self, wi).cc_usage_modal.is_some() {
            self.tree_panel_hover(wi, SurfaceKind::Accounts, x_phys, y_phys);
        }
        if win!(self, wi).panels.open() == Some(SurfaceKind::Shortcuts) {
            self.tree_panel_hover(wi, SurfaceKind::Shortcuts, x_phys, y_phys);
        }
        if win!(self, wi).panels.open() == Some(SurfaceKind::Layout) {
            self.tree_panel_hover(wi, SurfaceKind::Layout, x_phys, y_phys);
        }
        if win!(self, wi).process_panel.is_some() {
            self.tree_panel_hover(wi, SurfaceKind::Processes, x_phys, y_phys);
        }
        let w = &mut win!(self, wi);
        // F3+9 — drive context menu hover highlight when open.
        if let Some(state) = w.context_menu.as_mut() {
            use marspot::ui::components::ContextMenu;
            let menu = ContextMenu::layout(
                w.layout.window_w, w.layout.window_h, w.scale,
                state.anchor_x.px(), state.anchor_y.px(),
                w.layout.top_inset,
                &state.items,
            );
            let new_hover = menu.hover_index(&state.items, x_phys, y_phys);
            if new_hover != state.hovered_idx {
                state.hovered_idx = new_hover;
                w.needs_render = true;
            }
        }
    }

    /// Forget everything the pointer was on in a window: its chrome,
    /// its open panel, its context menu, in that order.
    pub(super) fn clear_hover(&mut self, wi: usize) -> [bool; 3] {
        let w = &mut win!(self, wi);
        let chrome = w.hover_chrome_btn.take().is_some();
        let mut panel = w.settings_surface.borrow_mut().kept.surface.pointer_leave();
        panel |= w.layout_surface.borrow_mut().kept.surface.pointer_leave();
        panel |= w.shortcuts_surface.borrow_mut().kept.surface.pointer_leave();
        if let Some(modal) = w.cc_usage_modal.as_mut() {
            panel |= modal.surface.borrow_mut().kept.surface.pointer_leave();
        }
        if let Some(p) = w.process_panel.as_mut() {
            panel |= p.surface.borrow_mut().kept.surface.pointer_leave();
        }
        let menu = w.context_menu.as_mut().is_some_and(|m| m.hovered_idx.take().is_some());
        w.needs_render |= chrome || panel || menu;
        [chrome, panel, menu]
    }

    /// A move to a point outside the window is the pointer leaving it:
    /// nothing in the window is under it any more.  `mouseMoved:` stops
    /// at the window's edge, so without this the last control the
    /// pointer crossed stayed lit.
    pub(super) fn pointer_left(&mut self, wi: usize, x_phys: f64, y_phys: f64) -> bool {
        let w = &win!(self, wi);
        if (0.0..w.w_phys).contains(&x_phys) && (0.0..w.h_phys).contains(&y_phys) {
            return false;
        }
        let id = w.window_id;
        let [chrome, panel, menu] = self.clear_hover(wi);
        lx_event!(
            "POINTER_LEFT",
            "the pointer left the window",
            window_id = id,
            chrome = chrome as u32,
            panel = panel as u32,
            menu = menu as u32
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use marspot::commands::{Chord, Key, CMD};
    use marspot_term::test_env::sandbox_state_dir;

    use super::super::window_state_tests::{app_with, win};
    use super::*;

    const OUTSIDE: (f64, f64) = marspot::app_pointer::OUTSIDE;

    fn centre(r: marspot_term::layout::Rect) -> (f64, f64) {
        (r.x + r.w * 0.5, r.y_top + r.h * 0.5)
    }

    fn press(app: &mut CoreApp, c: char, mods: u8) {
        let chord = Chord { key: Key::Char(c), mods };
        app.key(0, chord.key_event(), chord.modifiers());
    }

    fn app(sandbox: &marspot_term::test_env::StateSandbox) -> CoreApp {
        let mut app = app_with(sandbox, vec![win(1, vec![Pane::new_vacant(10, 80, 24)])]);
        app.rebuild_layout(0);
        app
    }

    #[test]
    fn a_toolbar_button_goes_dark_when_the_pointer_leaves_the_window() {
        let sandbox = sandbox_state_dir();
        let mut app = app(&sandbox);
        let (x, y) = centre(app.windows[0].layout.settings_button_rect);
        app.mouse_moved(0, x, y);
        assert!(matches!(app.windows[0].hover_chrome_btn, Some(ChromeHover::Toolbar(_))));
        app.windows[0].needs_render = false;
        app.mouse_moved(0, OUTSIDE.0, OUTSIDE.1);
        assert_eq!(app.windows[0].hover_chrome_btn, None);
        assert!(app.windows[0].needs_render, "the unlit button is owed a frame");
        // Just past the far corner is outside too; the corner pixel is not.
        let (w, h) = (app.windows[0].w_phys, app.windows[0].h_phys);
        assert!(app.pointer_left(0, w, h - 1.0) && app.pointer_left(0, w - 1.0, h));
        assert!(!app.pointer_left(0, w - 1.0, h - 1.0) && !app.pointer_left(0, 0.0, 0.0));
    }

    #[test]
    fn an_open_panels_controls_go_dark_when_the_pointer_leaves_or_the_app_resigns() {
        let sandbox = sandbox_state_dir();
        let mut app = app(&sandbox);
        let top = app.windows[0].layout.top_inset;
        let lit = |app: &CoreApp| {
            let w = &app.windows[0];
            w.settings_surface.borrow().kept.surface.interaction.hovered.is_some()
                || w.layout_surface.borrow().kept.surface.interaction.hovered.is_some()
        };
        for (c, mods, kind) in [(',', CMD, SurfaceKind::Settings), ('l', CMD | marspot::commands::SHIFT, SurfaceKind::Layout)] {
            press(&mut app, c, mods);
            let (x, y) = centre(PanelChrome::new(app.panel_frame(0, kind), top).close);
            app.mouse_moved(0, x, y);
            assert!(lit(&app), "{kind:?}: the close button lights under the pointer");
            app.mouse_moved(0, OUTSIDE.0, OUTSIDE.1);
            assert!(!lit(&app), "{kind:?}: and goes dark when the pointer leaves");
            app.mouse_moved(0, x, y);
            assert_eq!(app.clear_hover(0), [false, true, false], "{kind:?}: resigning clears it too");
            assert!(!lit(&app));
            assert_eq!(app.clear_hover(0), [false; 3], "nothing is left to clear");
        }
    }
}
