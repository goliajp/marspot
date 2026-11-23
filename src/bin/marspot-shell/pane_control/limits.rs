//! Rates and counts.  Token buckets are worked out when a request
//! arrives, from the time since the last one; nothing runs between
//! requests.  Every per-caller limit has a machine-wide one beside it,
//! so a caller claiming to be someone else cannot get past the total.
//!
//! The tables are keyed by session and cleared when the pane closes;
//! every caller outside the panes shares one entry.

use std::collections::HashMap;

use marspot_peer::control::{ErrorCode, Reject};

use super::caller::CallerKey;
use super::config::Config;

#[derive(Clone, Copy, Debug)]
pub(super) struct Bucket {
    tokens: f64,
    at_ms: u64,
}

impl Bucket {
    fn full(burst: f64, now_ms: u64) -> Self {
        Self { tokens: burst, at_ms: now_ms }
    }

    /// Take `n` at `rate` per second with room for `burst`; how long
    /// until there would be enough when there is not.
    fn take(&mut self, n: f64, rate: f64, burst: f64, now_ms: u64) -> Result<(), u64> {
        let dt = now_ms.saturating_sub(self.at_ms) as f64 / 1000.0;
        self.tokens = (self.tokens + dt * rate).min(burst);
        self.at_ms = now_ms.max(self.at_ms);
        // a request bigger than the burst still passes once the bucket is full
        let need = n.min(burst);
        if self.tokens >= need {
            self.tokens -= n;
            return Ok(());
        }
        Err((((need - self.tokens) / rate) * 1000.0).ceil() as u64)
    }
}

/// Which rate a request counts against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Input,
    Read,
    Spawn,
    Other,
}

#[derive(Default)]
pub(super) struct Limits {
    global: Option<Bucket>,
    input: HashMap<CallerKey, Bucket>,
    read: HashMap<CallerKey, Bucket>,
    spawn: HashMap<u64, Bucket>,
    spawn_global: Option<Bucket>,
    pane_bytes: HashMap<u64, Bucket>,
    /// Panes in windows each caller opened that are still open.
    window_panes: HashMap<CallerKey, Vec<u64>>,
    /// Who opened each pane opened through the socket that is still open.
    created: HashMap<u64, CallerKey>,
}

fn rate_limited(what: &str, ms: u64) -> Reject {
    Reject::new(ErrorCode::LimitRate, format!("{what}; try again in {ms} ms")).retry_after(ms.max(1))
}

fn take(map: &mut HashMap<CallerKey, Bucket>, key: CallerKey, n: f64, rate: f64, now: u64) -> Result<(), u64> {
    map.entry(key).or_insert_with(|| Bucket::full(rate, now)).take(n, rate, rate, now)
}

impl Limits {
    /// Count one request of `kind` from `caller`.
    pub fn admit(&mut self, cfg: &Config, caller: CallerKey, kind: Kind, now: u64) -> Result<(), Reject> {
        let g = cfg.rate_global as f64;
        self.global
            .get_or_insert_with(|| Bucket::full(g, now))
            .take(1.0, g, g, now)
            .map_err(|ms| rate_limited("too many requests from all callers together", ms))?;
        let (map, rate, what) = match kind {
            Kind::Input => (&mut self.input, cfg.rate_input as f64, "too many inputs from this caller"),
            Kind::Read => (&mut self.read, cfg.rate_read as f64, "too many reads from this caller"),
            Kind::Spawn | Kind::Other => return Ok(()),
        };
        take(map, caller, 1.0, rate, now).map_err(|ms| rate_limited(what, ms))
    }

    /// Count `bytes` written into pane `sid`.
    pub fn admit_bytes(&mut self, cfg: &Config, sid: u64, bytes: usize, now: u64) -> Result<(), Reject> {
        let rate = cfg.pane_bytes_per_s as f64;
        self.pane_bytes
            .entry(sid)
            .or_insert_with(|| Bucket::full(rate, now))
            .take(bytes as f64, rate, rate, now)
            .map_err(|ms| {
                Reject::new(ErrorCode::LimitBytes, format!("too many bytes into this pane; try again in {ms} ms")).retry_after(ms.max(1))
            })
    }

    /// Count a new pane under the top pane `root` (the caller's own
    /// pane for one in a window).
    pub fn admit_spawn(&mut self, cfg: &Config, root: u64, now: u64) -> Result<(), Reject> {
        let per_min = cfg.spawn_per_minute as f64 / 60.0;
        let burst = cfg.spawn_per_minute as f64;
        let g = burst * 4.0;
        self.spawn_global
            .get_or_insert_with(|| Bucket::full(g, now))
            .take(1.0, g / 60.0, g, now)
            .map_err(|ms| rate_limited("too many new panes from all callers together", ms))?;
        self.spawn
            .entry(root)
            .or_insert_with(|| Bucket::full(burst, now))
            .take(1.0, per_min, burst, now)
            .map_err(|ms| rate_limited("too many new panes under this pane", ms))
    }

    pub fn window_panes_of(&self, caller: CallerKey) -> usize {
        self.window_panes.get(&caller).map_or(0, Vec::len)
    }

    /// `caller` opened `sid`; in a window when not `shadow`.
    pub fn opened(&mut self, caller: CallerKey, sid: u64, shadow: bool) {
        if !shadow {
            self.window_panes.entry(caller).or_default().push(sid);
        }
        self.created.insert(sid, caller);
    }

    pub fn opened_by(&self, sid: u64) -> Option<CallerKey> {
        self.created.get(&sid).copied()
    }

    pub fn retain(&mut self, open: &dyn Fn(u64) -> bool) {
        let keep = |k: &CallerKey| k.sid().is_none_or(open);
        self.input.retain(|k, _| keep(k));
        self.read.retain(|k, _| keep(k));
        self.spawn.retain(|sid, _| open(*sid));
        self.pane_bytes.retain(|sid, _| open(*sid));
        self.created.retain(|sid, _| open(*sid));
        self.window_panes.retain(|k, _| keep(k));
        for v in self.window_panes.values_mut() {
            v.retain(|sid| open(*sid));
        }
        self.window_panes.retain(|_, v| !v.is_empty());
    }

    #[cfg(test)]
    pub fn table_sizes(&self) -> usize {
        self.input.len() + self.read.len() + self.spawn.len() + self.pane_bytes.len() + self.window_panes.len() + self.created.len()
    }
}

/// What the count limits look at for a new pane.
pub(super) struct SpawnCounts {
    pub live_sessions: usize,
    pub shadows_total: usize,
    pub shadows_under_root: usize,
    /// How many shadow levels the new pane's parent is down already.
    pub parent_depth: usize,
}

/// The session cap L2 enforces; a test lowers it the same way.
pub(super) fn session_cap() -> usize {
    std::env::var("MARSPOT_SESSION_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(64)
}

/// The count limits for a new pane: the user's headroom first, then
/// the shadow totals and the nesting depth.
pub(super) fn check_spawn(cfg: &Config, c: &SpawnCounts, shadow: bool, cap: usize) -> Result<(), Reject> {
    let room_kept = cap.saturating_sub(cfg.user_headroom);
    if c.live_sessions >= room_kept {
        return Err(Reject::new(
            ErrorCode::LimitCount,
            format!("{} sessions are running; the last {} of {cap} are kept for the user", c.live_sessions, cfg.user_headroom),
        ));
    }
    if !shadow {
        return Ok(());
    }
    if c.shadows_total >= cfg.shadows_total {
        return Err(Reject::new(ErrorCode::LimitCount, format!("{} shadow panes exist, the most at once", c.shadows_total)));
    }
    if c.shadows_under_root >= cfg.shadows_per_parent {
        return Err(Reject::new(ErrorCode::LimitCount, format!("{} shadow panes under this pane, the most", c.shadows_under_root)));
    }
    if c.parent_depth >= cfg.shadow_depth {
        return Err(Reject::new(ErrorCode::LimitCount, format!("shadow panes nest {} deep at most", cfg.shadow_depth)));
    }
    Ok(())
}
