//! The pointer over the main window's chrome — toolbar, sidebar and
//! title strips — answered by the trees those are drawn from, laid out
//! as the next frame will draw them.

use marspot::render_metal::ChromeShape;
use marspot::ui::components::chrome_target::ChromeTarget;
use marspot::ui::components::title_strip_view::BadgesOnly;

use super::*;

impl CoreApp {
    /// Run `f` on window `wi`'s chrome with the shape its next frame
    /// has: the layout, a sidebar row per pane, a strip per pane that
    /// has a cell, with the badge each strip will show.
    fn with_chrome<T>(
        &mut self,
        wi: usize,
        f: impl FnOnce(&mut marspot::render_metal::WindowRender, &mut marspot::font_cache::FontCache, &ChromeShape<'_>) -> T,
    ) -> T {
        let w = &mut self.windows[wi];
        let badges = &self.pane_badges;
        let panes = &w.panes;
        let badge = |i: usize| {
            panes[i].shelld_session_id().and_then(|sid| badges.get(&sid)).map_or("", String::as_str)
        };
        let strips = BadgesOnly(panes.len().min(w.layout.cells.len()), badge);
        let shape = ChromeShape { layout: &w.layout, rows: panes.len(), strips: &strips };
        f(&mut w.render, self.renderer.font_mut(), &shape)
    }

    /// What `(x, y)` is on in window `wi`'s chrome.
    pub(super) fn chrome_at(&mut self, wi: usize, x: f64, y: f64) -> Option<ChromeTarget> {
        self.with_chrome(wi, |r, font, shape| r.chrome_at(font, shape, (x, y)))
    }

    /// Follow the pointer over the chrome.  A control lighting or going
    /// dark is a frame to paint, not a layout to redo.
    pub(super) fn hover_chrome(&mut self, wi: usize, x: f64, y: f64) {
        if self.with_chrome(wi, |r, font, shape| r.chrome_pointer_move(font, shape, (x, y))) {
            win!(self, wi).needs_render = true;
        }
    }

    /// A press on the chrome, remembered until the release.
    pub(super) fn press_chrome(&mut self, wi: usize, x: f64, y: f64) -> Option<ChromeTarget> {
        self.with_chrome(wi, |r, font, shape| r.chrome_press(font, shape, (x, y)))
    }

    /// The release that ends a press: in `drop_window_id` at `(x, y)`.
    /// The control pressed acts if it is a button and the release is
    /// on it; true when one did.
    pub(super) fn release_chrome(&mut self, wi: usize, drop_window_id: u32, x: f64, y: f64) -> bool {
        // a release in another window, or off every window, is on no
        // control of this one
        let at = (drop_window_id == win!(self, wi).window_id).then_some((x, y));
        let Some(t) = self.with_chrome(wi, |r, font, shape| r.chrome_release(font, shape, at)) else {
            return false;
        };
        match t {
            ChromeTarget::Toolbar(0) => self.toggle_sidebar(wi),
            ChromeTarget::Toolbar(i) => {
                if let Some(kind) = panel_of_button(i) {
                    self.toggle_panel(wi, kind);
                }
            }
            ChromeTarget::Add => self.add_pane(wi),
            // the last pane closes too: see `close_window_if_emptied`
            ChromeTarget::Close(idx) => {
                if idx < win!(self, wi).panes.len() {
                    self.close_session(wi, idx);
                    if wi < self.windows.len() {
                        self.rebuild_layout(wi);
                    }
                }
            }
            // the plugin owns what the prefix means and what cycling it
            // does
            ChromeTarget::Badge(i) => {
                if let Some(sid) = win!(self, wi).panes.get(i).and_then(|p| p.shelld_session_id()) {
                    let payload = marspot::shell_proto::encode_pane_badge_clicked(sid);
                    self.pending_to_shell.push((MsgType::PaneBadgeClicked, payload));
                }
            }
            ChromeTarget::Helpers(i) => {
                if let Some(sid) = win!(self, wi).panes.get(i).and_then(|p| p.shelld_session_id()) {
                    self.open_helpers(wi, sid);
                }
            }
            ChromeTarget::AllHelpers => self.open_helpers(wi, 0),
            ChromeTarget::Row(_) | ChromeTarget::Title(_) => return false,
        }
        true
    }
}

/// The panel toolbar button `i` opens; the first button shows and
/// hides the sidebar instead.
pub(super) fn panel_of_button(i: u8) -> Option<SurfaceKind> {
    match i {
        1 => Some(SurfaceKind::Layout),
        2 => Some(SurfaceKind::Processes),
        3 => Some(SurfaceKind::Accounts),
        4 => Some(SurfaceKind::Settings),
        _ => None,
    }
}
