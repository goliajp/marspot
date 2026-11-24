//! Viewing a shadow pane in the place of the pane at the top of its
//! chain of parents.
//!
//! The host pane keeps its slot, its size, its scroll position and its
//! title; `Pane::peek` says what is shown instead of its session.  The
//! helper is given the host's size before it is shown, and the host
//! stays on the screen until a frame of that size is there or its time
//! is up (`peek_wait.rs`).  Nothing of this is saved: after a
//! restart the host shows itself.

use marspot::pane::Peek;
use marspot::shell_proto::{DoneCode, PanePlace};

use super::pane_control::Route;
use super::pointer_drag::{ReportedAt, last_heard};
use super::shown::shown_of;
use super::*;

/// Why a view did not start.
type Refused = (DoneCode, &'static str);

impl CoreApp {
    /// The window and slot `sid` is viewed in.
    pub(super) fn viewer_of(&self, sid: u64) -> Option<(usize, usize)> {
        self.windows
            .iter()
            .enumerate()
            .find_map(|(wi, w)| Some((wi, w.panes.iter().position(|p| p.peeked() == Some(sid))?)))
    }

    /// Show `helper` in `host`'s place, or `host` itself again.  The
    /// helper has to be below `host`, and `host` to be in a window's
    /// grid.  Whether it is, or will be, shown.
    pub(super) fn view_helper(&mut self, host: u64, helper: Option<u64>) -> bool {
        let Some(sid) = helper else {
            self.leave_view(host);
            return true;
        };
        match self.start_view(host, sid, shadow_place::L2_REQ_UNTRACKED, true) {
            Ok(_) => true,
            Err((code, why)) => {
                lx_warn!("core.peek.refused", &format!("{code:?} {why}"), session = sid, host = host);
                false
            }
        }
    }

    /// `PanePlace` 2 and 3.  Host 0 is the helper's own; leaving takes
    /// either of the two.
    pub(super) fn place_peek(&mut self, m: PanePlace, on: bool) -> Route {
        let answer = |app: &mut Self, code, text: &str| app.answer(MsgType::PanePlace, m.req_id, m.sid, code, text);
        let in_window = self.find_window_pane_by_sid(m.sid).is_some();
        if !in_window && self.shadow_index(m.sid).is_none() {
            return answer(self, DoneCode::NoSuchSession, "");
        }
        let root = self.pane_control.shadows.family.root(m.sid);
        if !on {
            self.leave_view_of(root, m.sid);
            return answer(self, DoneCode::Ok, "");
        }
        if in_window {
            return answer(self, DoneCode::Other, "only a shadow pane is viewed in place");
        }
        let host = if m.host_sid == 0 { root } else { m.host_sid };
        match self.start_view(host, m.sid, m.req_id, m.force) {
            Ok(true) => answer(self, DoneCode::Ok, ""),
            Ok(false) => Route::Forwarded,
            Err((code, why)) => answer(self, code, why),
        }
    }

    fn show(&mut self, wi: usize, idx: usize, sid: u64) {
        self.unview(wi, idx);
        self.before_switch(wi, idx);
        self.resolve_pane_cwd(sid);
        let w = &mut win!(self, wi);
        w.panes[idx].peek = Some(Peek::Live(sid));
        w.needs_render = true;
        let host = &w.panes[idx];
        lx_event!("PEEK_SHOWN", "a pane shows a shadow pane in its place", session = sid,
            host = host.shelld_session_id().unwrap_or(0), host_offset = host.view_offset() as u32);
    }

    /// Slot `idx` shows its own session again.  Whether it did not.
    pub(super) fn unview(&mut self, wi: usize, idx: usize) -> bool {
        if win!(self, wi).panes.get(idx).is_none_or(|p| p.peek.is_none()) {
            return false;
        }
        self.before_switch(wi, idx);
        let w = &mut win!(self, wi);
        w.needs_render = true;
        if let Some(Peek::Live(sid)) = w.panes[idx].peek.take() {
            unwatched::set(sid, true);
        }
        let host = &w.panes[idx];
        lx_event!("PEEK_LEFT", "a pane shows its own session again",
            host = host.shelld_session_id().unwrap_or(0), host_offset = host.view_offset() as u32);
        true
    }

    /// `host` shows its own session, and waits to show no other.
    fn leave_view(&mut self, host: u64) {
        self.drop_pending(host, "the view was left");
        if let Some((wi, idx)) = self.find_window_pane_by_sid(host) {
            self.unview(wi, idx);
        }
    }

    /// Leave the view `sid` is in, or any when `sid` is the host.
    fn leave_view_of(&mut self, host: u64, sid: u64) {
        let waits = self.pane_control.shadows.peeks.pending.iter().any(|p| p.host == host && p.sid == sid);
        if sid == host || waits || self.viewer_of(sid).is_some() {
            self.leave_view(host);
        }
    }

    /// Leave the focused slot's view, if it has one.
    pub(super) fn leave_focused_view(&mut self, wi: usize) -> bool {
        self.unview(wi, win!(self, wi).focused_idx)
    }

    /// What was begun over the session shown until now ends with it: a
    /// selection, a button its program holds, a drag of its scrollbar.
    fn before_switch(&mut self, wi: usize, idx: usize) {
        let here = |r: &mut ReportedAt| (r.0, r.1) == (wi, idx);
        let last = self.mouse_report_last_cell.take_if(here);
        if let Some(press) = self.mouse_report_press.take_if(here) {
            let (_, _, c1, r1) = last_heard(press, last);
            if let Some(pane) = self.shown_pane_mut(wi, idx) {
                pane.session_mut().l3_inject_mouse_button(0, false, c1, r1);
            }
        }
        self.scrollbar_drag.take_if(|d| d.is_of(wi, idx));
        let w = &mut win!(self, wi);
        if w.selection.is_some_and(|s| s.session_idx == idx) {
            w.selection = None;
            w.selection_dragging = false;
            w.selection_tape = None;
        }
    }

    /// Shadow `i` leaves the shadows: the one way any does.  The slot
    /// it is viewed in keeps its last frame.
    pub(super) fn take_shadow(&mut self, i: usize) -> Pane {
        let sid = self.shadows[i].shelld_session_id().unwrap_or(0);
        let waiting = self.pane_control.shadows.peeks.pending.iter().find(|p| p.sid == sid).map(|p| p.host);
        if let Some(host) = waiting {
            self.drop_pending(host, "the pane went");
        }
        let viewer = self.viewer_of(sid);
        if let Some((wi, idx)) = viewer {
            self.before_switch(wi, idx);
        }
        let mut pane = self.shadows.remove(i);
        if let Some((wi, idx)) = viewer {
            pane.pump();
            let last = Box::new(pane.keep_last_frame());
            let name = self.helper_name(sid);
            let w = &mut win!(self, wi);
            w.panes[idx].peek = Some(Peek::Ended { last, name });
            w.needs_render = true;
            lx_event!("PEEK_ENDED", "a viewed shadow pane went; its last frame stays until the view is left", session = sid);
        }
        pane
    }

    /// The pane in front of the keys has no program to take them.
    pub(super) fn focused_view_ended(&self, wi: usize) -> bool {
        self.windows.get(wi).and_then(|w| w.panes.get(w.focused_idx)).is_some_and(|host| match host.peek {
            None => false,
            Some(Peek::Ended { .. }) => true,
            Some(Peek::Live(_)) => shown_of(host, &self.shadows).is_exited(),
        })
    }

    /// Whether the focused slot's view has ended, and so took the key:
    /// Esc, Enter and the up arrow leave it, any other does nothing.
    pub(super) fn ended_view_takes_key(&mut self, wi: usize, event: &MarspotKeyEvent) -> bool {
        use marspot::input::{KeyState, LogicalKey, NamedKey};
        if !self.focused_view_ended(wi) {
            return false;
        }
        if event.state == KeyState::Pressed && !event.repeat {
            self.pane_control.shadows.peeks.held.swallowed(&event.logical);
            if matches!(event.logical, LogicalKey::Named(NamedKey::Escape | NamedKey::Enter | NamedKey::ArrowUp)) {
                self.leave_focused_view(wi);
            }
        }
        true
    }

    fn helper_name(&self, sid: u64) -> String {
        match self.pane_titles.get(&sid) {
            Some(title) => title.clone(),
            None => {
                let cwd = self.pane_cwds.get(&sid).cloned().unwrap_or_default();
                marspot::pane_name::PaneRef::new(sid, cwd).full_name()
            }
        }
    }

    /// What slot `idx`'s title says after the pane's own name while it
    /// shows another: which, and the key that leaves it.
    pub(super) fn view_label(&self, wi: usize, idx: usize) -> Option<String> {
        let key = marspot::commands::hint(marspot::commands::Command::ClosePanel);
        match win!(self, wi).panes.get(idx)?.peek.as_ref()? {
            Peek::Live(sid) => Some(format!(" › {}  {key}", self.helper_name(*sid))),
            Peek::Ended { name, .. } => Some(format!(" › {name} · {}  {key}", t(Msg::HelperEnded))),
        }
    }

    /// What the header of window `wi` says while its one pane shows
    /// another: that pane's title, which has no strip to go in.
    pub(super) fn view_line<'a>(&self, wi: usize, titles: &'a [String]) -> &'a str {
        let w = &win!(self, wi);
        if w.layout.cell_title_h > 0.0 || w.panes.len() != 1 || w.panes[0].peek.is_none() {
            return "";
        }
        titles.first().map_or("", String::as_str)
    }

    /// Hand window `wi`'s header what its lone pane shows, and say so
    /// when that changes.
    pub(super) fn publish_view_line(&self, wi: usize, wr: &mut marspot::render_metal::WindowRender, titles: &[String]) {
        let line = self.view_line(wi, titles);
        if !wr.set_view_line(line) {
            return;
        }
        let (cw, ch) = self.renderer.cell_dims();
        // the ascent moves only the baseline, which the log does not say
        let drawn = marspot::ui::components::view_line::place(&win!(self, wi).layout, line, (cw as f32, ch as f32, 0.0));
        lx_event!("PEEK_LINE", "the header of a window with one pane says what the pane shows",
            chars = line.chars().count() as u32, drawn = drawn.is_some() as u32);
    }
}

#[path = "peek_wait.rs"]
mod wait;
pub(super) use wait::Peeks;

#[cfg(test)]
#[path = "peek_more_tests.rs"]
mod more_tests;
#[cfg(test)]
#[path = "peek_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "peek_line_tests.rs"]
mod line_tests;
