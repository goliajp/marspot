//! Opening, closing and resizing panes for L1, shadow panes above all.
//!
//! A shadow pane is a session with no window: its `Pane` lives in
//! `CoreApp::shadows`, is never laid out or drawn, and wakes nobody
//! when it publishes a frame (`unwatched`).  Whose shadow it is lives
//! in the session's registry entry alone, written by the session.
//!
//! Every request is answered: `PaneSpawn` with `PaneSpawned` once the
//! new session's handshake is done or failed, the others with
//! `PaneDone`.  Moving a pane between a window and the shadows is in
//! `shadow_place`.

use super::pane_control::Route;
use super::spawn::{ShadowEnv, spawn_l3_pane_async_as};
use super::*;
use marspot::shell_proto::{
    DoneCode, IN_A_WINDOW_TEXT, PaneRetire, PaneSpawn, PaneSpawned, RetireMode, decode_pane_retire, decode_pane_size,
    decode_pane_spawn, encode_pane_spawned,
};
use std::collections::HashMap;

/// A shadow pane opened without a size gets this one.
pub(super) const DEFAULT_COLS: u16 = 120;
pub(super) const DEFAULT_ROWS: u16 = 40;
/// Larger than any screen holds; a request past it is refused.
const MAX_DIM: u16 = 1000;

/// What the core keeps about requests still in flight.
#[derive(Default)]
pub(super) struct Shadows {
    /// Session id → (L1's request id, opened as a shadow), until the
    /// session's spawn lands.  One entry per open L1 asked for, which
    /// L1 rate-limits; each leaves when its spawn reports back.
    spawning: HashMap<u64, (u32, bool)>,
    pub(super) unsaid: super::spawn_said::SpawnsUnsaid,
    pub(super) closed: ClosedSlots,
    pub(super) places: super::shadow_place::Places,
    pub(super) peeks: super::peek::Peeks,
    pub(super) family: super::helper_panel::Family,
    /// Dev seam only (`MARSPOT_DEV_SHADOWS=n`): how many shadow panes
    /// to open under the first pane, and when.
    pub(super) dev: Option<(usize, Instant)>,
}

impl Shadows {
    pub(super) fn from_env() -> Self {
        let dev = std::env::var("MARSPOT_DEV_SHADOWS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .map(|n| (n, Instant::now() + Duration::from_secs(3)));
        Self { dev, ..Self::default() }
    }

    /// When the loop has to wake for these.
    pub(super) fn wait(&self) -> Option<Duration> {
        let dev = self.dev.map(|(_, t)| {
            t.saturating_duration_since(Instant::now())
                .max(Duration::from_millis(250))
        });
        [self.places.wait(), self.peeks.wait(), self.unsaid.wait(), dev].into_iter().flatten().min()
    }
}

/// Slots with no L3 that were closed, whose spawn may yet land: the
/// session is closed then.  The oldest is forgotten past the cap, which
/// leaves its session running.
#[derive(Default)]
pub(super) struct ClosedSlots(std::collections::VecDeque<u64>);

impl ClosedSlots {
    const CAP: usize = 64;

    pub(super) fn note(&mut self, sid: u64) {
        if sid == 0 || self.0.contains(&sid) {
            return;
        }
        if self.0.len() == Self::CAP {
            self.0.pop_front();
        }
        self.0.push_back(sid);
    }

    /// Whether the slot of `sid` was closed; asked once.
    pub(super) fn take(&mut self, sid: u64) -> bool {
        let at = self.0.iter().position(|s| *s == sid);
        at.and_then(|i| self.0.remove(i)).is_some()
    }
}

/// The frames this file and `shadow_place` answer.
pub(super) fn is_shadow_request(t: MsgType) -> bool {
    matches!(
        t,
        MsgType::PaneSpawn | MsgType::PaneRetire | MsgType::PanePlace | MsgType::PaneSize
    )
}

fn size_ok(cols: u16, rows: u16) -> bool {
    (1..=MAX_DIM).contains(&cols) && (1..=MAX_DIM).contains(&rows)
}

impl CoreApp {
    /// L1 asked to open, close, place or resize a pane.
    pub(super) fn route_shadow_request(&mut self, f: Frame, req_id: u32) -> Route {
        let route = match f.msg_type {
            MsgType::PaneSpawn => decode_pane_spawn(&f.payload).ok().map(|m| self.spawn_pane(m)),
            MsgType::PaneRetire => decode_pane_retire(&f.payload)
                .ok()
                .map(|m| self.retire_pane(m)),
            MsgType::PanePlace => marspot::shell_proto::decode_pane_place(&f.payload)
                .ok()
                .map(|m| self.place_pane(m)),
            MsgType::PaneSize => decode_pane_size(&f.payload)
                .ok()
                .map(|m| self.size_pane(m.req_id, m.sid, m.cols, m.rows)),
            _ => None,
        };
        route.unwrap_or_else(|| self.answer(f.msg_type, req_id, 0, DoneCode::Other, "malformed request"))
    }

    pub(super) fn spawned(&mut self, req_id: u32, sid: u64, err: &str) -> Route {
        let m = PaneSpawned { req_id, ok: err.is_empty(), sid, err: err.to_string() };
        lx_event!(
            "PANE_SPAWN_REPLY",
            "answered L1's request for a new pane",
            req_id = req_id,
            session = sid,
            err = err
        );
        if let Ok(p) = encode_pane_spawned(&m) {
            self.pane_control.reports.push(Frame::new(MsgType::PaneSpawned, p));
        }
        if err.is_empty() { Route::Answered(DoneCode::Ok) } else { Route::Answered(DoneCode::Other) }
    }

    /// `PaneSpawn`: a shadow pane under `parent_sid`, or a pane in the
    /// window that holds `parent_sid` (the key window when none does),
    /// which leaves the keyboard focus where it is.
    fn spawn_pane(&mut self, m: PaneSpawn) -> Route {
        let (cols, rows) = self.spawn_size(&m);
        if !size_ok(cols, rows) {
            return self.spawned(m.req_id, 0, "size out of range");
        }
        if !m.shadow {
            let wi = self
                .find_window_pane_by_sid(m.parent_sid)
                .map(|(wi, _)| wi)
                .unwrap_or(self.key_window)
                .min(self.windows.len().saturating_sub(1));
            if self.windows.is_empty() {
                return self.spawned(m.req_id, 0, "no window is open");
            }
            let Some(sid) = self.spawn_session_at(wi, m.cwd) else {
                return self.spawned(m.req_id, 0, "the window holds no more panes, or no session id was free");
            };
            self.rebuild_layout(wi);
            self.pane_control.shadows.spawning.insert(sid, (m.req_id, false));
            return Route::Forwarded;
        }
        let parent_known = m.parent_sid != 0
            && self.panes_mut().any(|p| p.shelld_session_id() == Some(m.parent_sid));
        if !parent_known {
            return self.spawned(m.req_id, 0, "no such parent pane");
        }
        let sid = match allocate_next_session_id() {
            Ok(id) => id,
            Err(e) => return self.spawned(m.req_id, 0, &format!("no session id: {e}")),
        };
        // before the reader exists: its first frame must not wake the loop
        unwatched::set(sid, true);
        let env = ShadowEnv { parent: m.parent_sid, idle_s: m.idle_s };
        let pane = spawn_l3_pane_async_as(cols, rows, sid, m.cwd, Some(env), &self.event_tx);
        self.shadows.push(pane);
        self.pane_control.shadows.family.adopted(sid, m.parent_sid);
        self.shadows_changed();
        self.pane_control.shadows.spawning.insert(sid, (m.req_id, true));
        lx_event!(
            "SHADOW_SPAWN",
            "starting a shadow pane",
            session = sid,
            parent = m.parent_sid,
            cols = cols,
            rows = rows
        );
        Route::Forwarded
    }

    /// A spawn finished; answer the request that asked for it, if any.
    pub(super) fn spawn_landed(&mut self, sid: u64, outcome: Result<(), String>) {
        let Some((req_id, shadow)) = self.pane_control.shadows.spawning.remove(&sid) else {
            return;
        };
        match outcome {
            Ok(()) => self.answer_spawn_when_said(req_id, sid),
            Err(e) => {
                if shadow && let Some(i) = self.shadow_index(sid) {
                    let pane = self.take_shadow(i);
                    self.retire_pane_session(sid, pane.is_l3());
                    self.shadows_changed();
                    if !pane.is_l3() {
                        marspot::session_close::close_dead_session(sid);
                    }
                }
                self.spawned(req_id, 0, &e);
            }
        }
    }

    pub(super) fn shadow_index(&self, sid: u64) -> Option<usize> {
        self.shadows.iter().position(|p| p.shelld_session_id() == Some(sid))
    }

    /// `PaneRetire`: the existing close; `Recycle` keeps the session's
    /// directory in the recycle dir instead of deleting it.  One asked
    /// only of a shadow pane is refused once the pane is in a window: the
    /// move may have come in after it was asked.
    fn retire_pane(&mut self, m: PaneRetire) -> Route {
        let PaneRetire { req_id, sid, mode, shadow_only } = m;
        let recycle = mode == RetireMode::Recycle;
        let in_window = self.find_window_pane_by_sid(sid);
        if shadow_only && in_window.is_some() {
            lx_event!("SHADOW_RETIRE_REFUSED", "a close asked of a shadow pane found it in a window", session = sid);
            return self.answer(MsgType::PaneRetire, req_id, sid, DoneCode::Other, IN_A_WINDOW_TEXT);
        }
        if let Some((wi, idx)) = in_window {
            if recycle {
                recycle_session(sid);
            }
            self.close_session(wi, idx);
            if wi < self.windows.len() {
                self.rebuild_layout(wi);
            }
        } else if let Some(i) = self.shadow_index(sid) {
            if recycle {
                recycle_session(sid);
            }
            let pane = self.take_shadow(i);
            if !pane.is_l3() {
                self.pane_control.shadows.closed.note(sid);
            }
            self.retire_pane_session(sid, pane.is_l3());
            self.shadows_changed();
            lx_event!("SHADOW_RETIRED", "closed a shadow pane", session = sid, recycled = recycle as u32);
        } else {
            return self.answer(MsgType::PaneRetire, req_id, sid, DoneCode::NoSuchSession, "");
        }
        self.answer(MsgType::PaneRetire, req_id, sid, DoneCode::Ok, "")
    }

    /// `PaneSize`: a shadow pane takes any size; a pane in a window,
    /// or viewed in one, takes its window's layout.
    fn size_pane(&mut self, req_id: u32, sid: u64, cols: u16, rows: u16) -> Route {
        if !size_ok(cols, rows) {
            return self.answer(MsgType::PaneSize, req_id, sid, DoneCode::Other, "size out of range");
        }
        if let Some(i) = self.shadow_index(sid).filter(|_| self.viewer_of(sid).is_none()) {
            self.shadows[i].resize(cols, rows);
            self.strips.sized(sid);
            return self.answer(MsgType::PaneSize, req_id, sid, DoneCode::Ok, "");
        }
        if self.slot_showing(sid).is_some() {
            return self.answer(MsgType::PaneSize, req_id, sid, DoneCode::Other, "not_resizable");
        }
        self.answer(MsgType::PaneSize, req_id, sid, DoneCode::NoSuchSession, "")
    }
}

/// Stop the session and move its directory to the recycle dir, where
/// it is kept for the recycle period; the close that follows finds
/// nothing left to delete.
fn recycle_session(sid: u64) {
    let entry = session_registry::read_session_entry(sid).ok();
    let regions = entry.as_ref().and_then(grid_shm::SessionRegions::of_entry);
    match session_registry::retire_session_dir(sid) {
        Ok(dst) => lx_event!(
            "PANE_RECYCLED",
            "a closed pane's session dir went to the recycle dir",
            session = sid,
            to = dst.display()
        ),
        Err(e) => lx_warn!("core.retire.recycle_failed", &format!("{e}"), session = sid),
    }
    if let Some(e) = &entry {
        let _ = marspot::session_signal::signal_entry(e, libc::SIGHUP);
    }
    if let Some(r) = regions {
        r.delete();
    }
}

#[cfg(test)]
#[path = "close_dir_tests.rs"]
mod close_dir_tests;

#[cfg(test)]
#[path = "shadows_tests.rs"]
pub(crate) mod tests;
