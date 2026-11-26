//! A pane asked for in a new window of its own.  L1 owns windows, so
//! the core asks it for one; the next window L1 announces is built
//! around a fresh pane, and that pane's spawn answers the request.  The
//! pane has no parent: nothing ties it to the pane of whoever asked,
//! and closing it empties the window, which then closes.

use super::super::spawn::spawn_l3_pane_async;
use super::*;
use marspot::shell_proto::{PaneSpawn, WINDOW_OPEN_USER, encode_window_open_request};
use std::collections::VecDeque;

/// Requests waiting for their window.  L1 opens one new pane at a time
/// across all callers, so a full queue means windows are not coming.
/// The window L1 announces carries no request id: a user's Cmd-N or a
/// move to a new window arriving in the same moment can swap windows
/// with this request (nothing leaks, the two panes trade places).
const CAP: usize = 8;
/// L1 stops waiting for the answer 10 s after it asked, and the answer
/// comes only once the pane's session is up; a window later than this
/// would be built for a request L1 has already failed.
const WINDOW_WITHIN: Duration = Duration::from_secs(6);

/// One request: L1's request id, the directory, when it was asked.
#[derive(Default)]
pub(in super::super) struct NewWindows(VecDeque<(u32, String, Instant)>);

impl CoreApp {
    /// `PaneSpawn` with `new_window`: ask L1 for a window and wait for it.
    pub(super) fn spawn_in_new_window(&mut self, m: PaneSpawn) -> Route {
        if self.pane_control.shadows.new_windows.0.len() >= CAP
            && let Some((old, ..)) = self.pane_control.shadows.new_windows.0.pop_front()
        {
            self.spawned(old, 0, "no window came");
        }
        self.pane_control
            .shadows
            .new_windows
            .0
            .push_back((m.req_id, m.cwd, Instant::now()));
        self.pending_to_shell.push((
            MsgType::WindowOpenRequest,
            encode_window_open_request(WINDOW_OPEN_USER),
        ));
        lx_event!(
            "PANE_SPAWN_NEW_WINDOW_REQUESTED",
            "asked L1 for a window to open a pane in",
            req_id = m.req_id
        );
        Route::Forwarded
    }

    /// The oldest request still waiting for its window; ones whose asker
    /// gave up are answered and dropped.
    pub(in super::super) fn take_new_window_spawn(
        &mut self,
        now: Instant,
    ) -> Option<(u32, String)> {
        while let Some((req_id, cwd, at)) = self.pane_control.shadows.new_windows.0.pop_front() {
            if now.saturating_duration_since(at) <= WINDOW_WITHIN {
                return Some((req_id, cwd));
            }
            self.spawned(req_id, 0, "the window came too late");
        }
        None
    }

    /// Window `window_id` just announced by L1, built as a 1×1 grid
    /// around a new pane whose shell starts in `cwd` (empty: `$HOME`).
    /// The new session's id, or `None` when no id was free.
    pub(in super::super) fn push_fresh_window(
        &mut self,
        window_id: u32,
        size_phys: (f64, f64),
        slot: Option<usize>,
        cwd: String,
    ) -> Option<u64> {
        let (w_phys, h_phys) = size_phys;
        let (cell_w, cell_h) = self.renderer.cell_dims();
        let cols = ((w_phys / cell_w) as u16).max(INITIAL_COLS);
        let rows = ((h_phys / cell_h) as u16).max(INITIAL_ROWS);
        let pane = match allocate_next_session_id() {
            Ok(id) => spawn_l3_pane_async(cols, rows, id, cwd, &self.event_tx),
            Err(e) => {
                lx_error!("core.window.session_id_failed", &format!("{e}"));
                return None;
            }
        };
        let sid = pane.shelld_session_id();
        let mut w = WindowState::new(window_id, vec![pane], 0, 1, 1, w_phys, h_phys);
        w.frame_index = self.slot_for_new_window(slot);
        // a brand-new window has never been painted
        w.render.mark_bg_clear_required();
        self.windows.push(w);
        let wi = self.windows.len() - 1;
        // a freshly opened window takes the keyboard focus; every other
        // window keeps pumping, painting and persisting as before
        self.key_window = wi;
        self.rebuild_layout(wi);
        lx_event!(
            "WINDOW_ADOPTED",
            "core took on a new window",
            window_id = window_id,
            windows = self.windows.len()
        );
        self.save_session_state();
        sid
    }

    /// The window for request `req_id` came: its pane answers the
    /// request once its session is up.
    pub(in super::super) fn adopt_window_for_spawn(
        &mut self,
        window_id: u32,
        size_phys: (f64, f64),
        slot: Option<usize>,
        (req_id, cwd): (u32, String),
    ) {
        match self.push_fresh_window(window_id, size_phys, slot, cwd) {
            Some(sid) => {
                self.pane_control
                    .shadows
                    .spawning
                    .insert(sid, (req_id, false));
                lx_event!(
                    "PANE_SPAWN_NEW_WINDOW",
                    "opened a pane in its own new window",
                    session = sid,
                    window_id = window_id
                );
            }
            None => {
                self.spawned(req_id, 0, "no session id was free");
            }
        }
    }
}

#[cfg(test)]
#[path = "spawn_window_tests.rs"]
mod tests;
