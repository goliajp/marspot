//! Who is waiting for which answer: requests by id, watches by watch
//! id, and how many inputs each pane has in flight.
//!
//! Every table here is bounded by the connections that fill it: a
//! connection waits on one request at a time, holds at most its
//! caller's share of watches, and takes its entries out when it stops
//! waiting, answered or not.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;

use marspot::shell_proto::{
    MsgType, WatchBody, decode_pane_done, decode_pane_spawned, decode_pane_typed, decode_pane_watch_fired,
};
use marspot_peer::control::{ErrorCode, Reject};

use super::Reply;
use super::caller::CallerKey;

/// Watch ids with this bit are the helpers plugin's; the session keeps
/// a slot for them.  Callers' ids never carry it.
const HELPER_BIT: u32 = 1 << 31;

pub(super) struct WatchEntry {
    pub sid: u64,
    pub caller: CallerKey,
    pub body: WatchBody,
    pub tx: Sender<Reply>,
}

pub(super) struct Links {
    next_req: AtomicU32,
    next_watch: AtomicU32,
    pending: Mutex<HashMap<u32, Sender<Reply>>>,
    watches: Mutex<HashMap<u32, WatchEntry>>,
    inflight: Mutex<HashMap<u64, u8>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where watch ids start: from the clock, so a shell that replaced
/// another under the same core does not hand out ids the core may
/// still hold reports for.  Ids are never reused within a run.
fn first_watch_id() -> u32 {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_secs());
    ((secs as u32).wrapping_mul(4096) & !HELPER_BIT).max(1)
}

impl Links {
    pub fn new() -> Self {
        Self {
            next_req: AtomicU32::new(1),
            next_watch: AtomicU32::new(first_watch_id()),
            pending: Mutex::new(HashMap::new()),
            watches: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// A request id, never 0.
    pub fn req_id(&self) -> u32 {
        loop {
            let id = self.next_req.fetch_add(1, Ordering::Relaxed);
            if id != 0 {
                return id;
            }
        }
    }

    fn watch_id(&self) -> u32 {
        loop {
            let id = self.next_watch.fetch_add(1, Ordering::Relaxed) & !HELPER_BIT;
            if id != 0 {
                return id;
            }
        }
    }

    pub fn expect(&self, req_id: u32, tx: Sender<Reply>) {
        lock(&self.pending).insert(req_id, tx);
    }

    pub fn unexpect(&self, req_id: u32) {
        lock(&self.pending).remove(&req_id);
    }

    /// Register a watch for `caller`, within its share and the whole
    /// table's; its id.
    pub fn add_watch(
        &self,
        sid: u64,
        caller: CallerKey,
        mut body: WatchBody,
        tx: Sender<Reply>,
        (per_caller, total): (usize, usize),
    ) -> Result<u32, Reject> {
        let mut w = lock(&self.watches);
        if w.len() >= total {
            return Err(Reject::new(ErrorCode::LimitCount, format!("{total} waits are pending, the most at once")));
        }
        if w.values().filter(|e| e.caller == caller).count() >= per_caller {
            return Err(Reject::new(ErrorCode::LimitCount, format!("this caller has {per_caller} waits pending, its most at once")));
        }
        body.watch_id = self.watch_id();
        let id = body.watch_id;
        w.insert(id, WatchEntry { sid, caller, body, tx });
        Ok(id)
    }

    /// The position watch `id` counts from, once the session said.
    pub fn set_since(&self, id: u32, pos: u64) {
        if let Some(e) = lock(&self.watches).get_mut(&id) {
            e.body.since_pos = pos;
        }
    }

    pub fn drop_watch(&self, id: u32) {
        lock(&self.watches).remove(&id);
    }

    /// Every watch, for registering again with a new core.
    pub fn watch_bodies(&self) -> Vec<(u64, WatchBody)> {
        lock(&self.watches).values().map(|e| (e.sid, e.body.clone())).collect()
    }

    /// Count an input in flight to `sid`, unless `max` already are.
    pub fn begin_input(&self, sid: u64, max: u8) -> bool {
        let mut f = lock(&self.inflight);
        let n = f.entry(sid).or_insert(0);
        if *n >= max {
            return false;
        }
        *n += 1;
        true
    }

    pub fn end_input(&self, sid: u64) {
        let mut f = lock(&self.inflight);
        if let Some(n) = f.get_mut(&sid) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                f.remove(&sid);
            }
        }
    }

    pub fn retain_panes(&self, open: &dyn Fn(u64) -> bool) {
        lock(&self.inflight).retain(|sid, _| open(*sid));
    }

    #[cfg(test)]
    pub fn sizes(&self) -> (usize, usize, usize) {
        (lock(&self.pending).len(), lock(&self.watches).len(), lock(&self.inflight).len())
    }

    /// Hand a frame from the core to whoever waits for it; false when it
    /// is not a pane-control answer.  Nobody waiting: dropped.
    pub fn route(&self, msg: MsgType, payload: &[u8]) -> bool {
        let (req_id, reply) = match msg {
            MsgType::PaneTyped => match decode_pane_typed(payload) {
                Ok(t) => (t.req_id, Reply::Typed(t)),
                Err(_) => return true,
            },
            MsgType::PaneDone => match decode_pane_done(payload) {
                Ok(d) => (d.req_id, Reply::Done(d)),
                Err(_) => return true,
            },
            MsgType::PaneSpawned => match decode_pane_spawned(payload) {
                Ok(s) => (s.req_id, Reply::Spawned(s)),
                Err(_) => return true,
            },
            MsgType::PaneWatchFired => {
                if let Ok(f) = decode_pane_watch_fired(payload)
                    && let Some(e) = lock(&self.watches).get(&f.watch_id)
                    && e.sid == f.sid
                {
                    let _ = e.tx.send(Reply::Fired(f));
                }
                return true;
            }
            _ => return false,
        };
        if let Some(tx) = lock(&self.pending).get(&req_id) {
            let _ = tx.send(reply);
        }
        true
    }
}
