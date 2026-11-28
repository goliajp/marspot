//! The ways a person opens a web pane: a row of a pane's "Replace
//! with…" that shows a page in the pane's place, a row of a sidebar
//! slot's menu that opens it in a new pane, both for the address on the
//! clipboard, and Command and a click on a link in a terminal.  None of
//! them is offered unless `marspot::web_pane::entry_points` says so: a
//! page's content process does not run in a sandbox yet.

use marspot::ui::components::MenuItem;

use super::*;

/// "Replace with…" → the page on the clipboard.
const WEB_PAGE_TAG: u32 = 0x5300_0002;
/// A sidebar slot's menu → the page on the clipboard, in a new pane.
const WEB_OPEN_TAG: u32 = 0x5300_0003;

/// An address a web pane opens: a whole http, https or file URL.
pub(super) fn page_address(text: &str) -> Option<String> {
    let t = text.trim();
    let ok = ["http://", "https://", "file://"].iter().any(|p| t.starts_with(p))
        && t.len() <= madcawl_wire::input::MAX_URL
        && !t.chars().any(char::is_whitespace);
    ok.then(|| t.to_owned())
}

fn clipboard_address() -> Option<String> {
    marspot::web_pane::entry_points().then(marspot::input::read_clipboard_text).flatten().as_deref().and_then(page_address)
}

impl CoreApp {
    /// The rows "Replace with…" adds for a web pane.
    pub(crate) fn web_rows(&self) -> Vec<MenuItem> {
        let Some(url) = clipboard_address() else { return Vec::new() };
        vec![MenuItem::divider(), MenuItem::entry(&strings::text(Arg::WebPageRow(&url)), WEB_PAGE_TAG)]
    }

    /// The row a sidebar slot's menu adds to open a web pane.
    pub(crate) fn web_open_row(&self) -> Option<MenuItem> {
        let url = clipboard_address()?;
        Some(MenuItem::entry(&strings::text(Arg::WebOpenRow(&url)), WEB_OPEN_TAG))
    }

    /// A web pane's row of a menu; false when `tag` is none.
    pub(crate) fn pick_web(&mut self, wi: usize, region: ContextRegion, tag: u32) -> bool {
        if tag != WEB_PAGE_TAG && tag != WEB_OPEN_TAG {
            return false;
        }
        win!(self, wi).context_menu = None;
        win!(self, wi).needs_render = true;
        let Some(url) = clipboard_address() else { return true };
        let idx = match region {
            ContextRegion::Pane(idx) | ContextRegion::SidebarSlot(idx) => idx,
            _ => win!(self, wi).focused_idx,
        };
        match tag {
            WEB_PAGE_TAG => self.replace_with_web(wi, idx, url),
            _ => {
                self.open_web_pane(wi, url);
            }
        }
        true
    }

    /// Slot `idx` shows a web pane on `url`; the pane it showed runs on
    /// in no window, as "Replace with…" leaves any other.
    fn replace_with_web(&mut self, wi: usize, idx: usize, url: String) {
        if idx >= win!(self, wi).panes.len() || self.window_full(wi) {
            return;
        }
        self.unview(wi, idx);
        if !self.open_web_pane(wi, url) {
            return;
        }
        let Some(page) = win!(self, wi).panes.pop() else { return };
        let old = std::mem::replace(&mut win!(self, wi).panes[idx], page);
        if let Some(sid) = old.shelld_session_id().filter(|_| !old.is_dormant()) {
            unwatched::set(sid, true);
            self.shadows.push(old);
        }
        win!(self, wi).focused_idx = idx;
        self.rebuild_layout(wi);
        self.shadows_changed();
        lx_event!("WEB_PANE_REPLACED", "a slot shows a web pane", window_id = win!(self, wi).window_id, slot = idx as u32);
    }

    /// Command and a click on a link in a terminal opens it in a web pane.
    pub(crate) fn web_link_press(&mut self, wi: usize, x: f64, y: f64, mods: Modifiers) -> bool {
        if !mods.super_ || !marspot::web_pane::entry_points() {
            return false;
        }
        let Some(link) = self.hit_test_link_at_xy(wi, x, y) else { return false };
        let Some(url) = page_address(link.target.as_deref().unwrap_or(&link.text)) else { return false };
        self.open_web_pane(wi, url)
    }
}

#[cfg(test)]
mod tests {
    use super::page_address;
    use super::*;

    #[test]
    fn web_entry_takes_whole_page_addresses_only() {
        assert_eq!(page_address(" https://example.com/a?b \n").as_deref(), Some("https://example.com/a?b"));
        assert_eq!(page_address("file:///tmp/x.html").as_deref(), Some("file:///tmp/x.html"));
        for no in ["ftp://x", "javascript:alert(1)", "https://a b", "", "see https://x", "file:/etc/passwd", "file:x.html"] {
            assert_eq!(page_address(no), None, "{no}");
        }
    }

    /// A window holding as many panes as it can opens no web pane (a
    /// link a page asks to open included), and says so.
    #[test]
    fn web_entry_a_full_window_opens_no_web_pane_and_says_why() {
        use crate::window_state_tests::{app_in, win};
        let sb = crate::boot_assembly_tests::Sandbox::new();
        let n = SESSION_COUNT_HARD_CAP;
        let panes = (1..=n as u64).map(|s| Pane::new_vacant(s, 60, 16)).collect();
        let mut app = app_in(sb.dir(), vec![win(1, panes)]);
        assert!(!app.open_web_pane(0, "file:///tmp/x.html".into()));
        assert_eq!(win!(app, 0).panes.len(), n, "no pane added");
        assert_eq!(app.notices.shell_text(), Some(strings::text(Arg::WindowFull(n)).as_str()));
    }
}
