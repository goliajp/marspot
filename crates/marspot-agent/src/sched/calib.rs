//! The pane speeds checked against what the accounts measure.
//!
//! A pane's speed is read from its own use and forecast a day ahead; the
//! accounts' readings say what the panes on them really took.  Over a
//! window of `WINDOW_S`, the week each account lost is set against the
//! week the speeds the planner was handed put on it, summed over the
//! accounts that neither renewed, closed nor ran dry inside the window
//! (a dry account serves less than is asked of it).  When the two differ
//! by more than the band of ordinary error, every pane speed is scaled
//! by their ratio until the next window says otherwise: a forecast three
//! times too high or too low is otherwise planned on as if true, and
//! every switch it times is early or late by that factor.
//!
//! Readings carry up to a point of error each, so the window must see
//! enough use for the ratio to mean anything (`MIN_USE`); with less the
//! factor in force stays.
//!
//! Only readings measured during the window count: an account read stale
//! at any reading (its week then extrapolated from the panes' own speeds)
//! is left out, a stale feed drops the window, and a gap between readings
//! longer than `GAP_S` (a sleep, a stuck worker) restarts it, since what
//! the speeds asked across the gap was not asked of anyone.  All of an
//! account's loss is put down to the panes on it: use from another
//! machine or the web reads as panes faster than their speeds, which is
//! why each window's loss and ask per account are kept for the log.

use crate::quota::Quant;

/// One window of readings, seconds.
pub const WINDOW_S: i64 = 6 * 3600;
/// A gap between readings longer than this restarts the window: as long
/// as the feed takes to count as stale.
pub const GAP_S: i64 = 15 * 60;
/// Least use a window must see, lost or asked, to move the factor, weeks
/// (ten points).
pub const MIN_USE: f64 = 0.1;
/// Ratios inside this band leave a factor of 1 alone.
pub const BAND: [f64; 2] = [0.7, 1.4];
/// A factor in force goes back to 1 only on a ratio inside this narrower
/// band: a ratio wandering about a band edge would otherwise flip the
/// factor every window.
pub const BACK: [f64; 2] = [0.8, 1.25];
/// The factor never leaves this range: a ratio past it says more about
/// the readings than about the speeds.
pub const RANGE: [f64; 2] = [0.2, 5.0];
/// An account reading this little or less at the window's end ran dry.
const DRY: f64 = 0.02;

/// The window open now, per account by input order.
#[derive(Clone, Debug, Default)]
struct Window {
    live: bool,
    at: i64,
    ids: Vec<u8>,
    r: Vec<f64>,
    t: Vec<f64>,
    /// The week the handed speeds put on each account since `at`.
    asked: Vec<f64>,
    /// The account was closed or read stale at some reading inside the
    /// window.
    skip: Vec<bool>,
}

/// The last window that closed.
#[derive(Clone, Debug, Default)]
pub struct Closed {
    /// Each account counted: id, week lost, week asked.
    pub accounts: Vec<(u8, f64, f64)>,
    pub used: f64,
    pub want: f64,
    /// Its ratio, when it saw enough use.
    pub ratio: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct Calib {
    win: Window,
    last: i64,
    factor: f64,
    /// The last ratio worked out, for the log.
    pub ratio: Option<f64>,
    pub closed: Closed,
}

impl Default for Calib {
    fn default() -> Self {
        Calib { win: Window::default(), last: 0, factor: 1.0, ratio: None, closed: Closed::default() }
    }
}

impl Calib {
    /// The factor every pane speed is scaled by.
    pub fn factor(&self) -> f64 {
        self.factor
    }

    /// A reading at `now` (seconds): the accounts as read, the week an
    /// hour the speeds handed to the planner (before the factor) put on
    /// each, by input order, and whether the feed is stale.  True when a
    /// window closed at this reading (`closed`).
    pub fn observe(&mut self, now: i64, accounts: &[Quant], asked: &[f64], stale_feed: bool) -> bool {
        if stale_feed {
            self.win.live = false;
            return false;
        }
        let w = &self.win;
        let same = w.live && w.ids.len() == accounts.len() && w.ids.iter().zip(accounts).all(|(a, q)| *a == q.id);
        if !same || now < self.last || now - self.last > GAP_S {
            self.open(now, accounts);
            return false;
        }
        let dt = (now - self.last) as f64 / 3600.0;
        self.last = now;
        let w = &mut self.win;
        for (j, q) in accounts.iter().enumerate() {
            w.asked[j] += asked[j] * dt;
            w.skip[j] |= !q.open || q.stale;
        }
        if now - w.at < WINDOW_S {
            return false;
        }
        let c = &mut self.closed;
        c.accounts.clear();
        (c.used, c.want, c.ratio) = (0.0, 0.0, None);
        for (j, q) in accounts.iter().enumerate() {
            let renewed = q.t > w.t[j] + 1e-6 || q.r > w.r[j] + 0.05;
            // an account no pane was on all window is used, if at all, from
            // elsewhere: its loss says nothing of the panes' speeds
            if renewed || w.skip[j] || q.r <= DRY || w.asked[j] <= 0.0 {
                continue;
            }
            let used = w.r[j] - q.r;
            c.accounts.push((q.id, used, w.asked[j]));
            c.used += used;
            c.want += w.asked[j];
        }
        if c.used.max(c.want) >= MIN_USE {
            let ratio = (c.used / c.want).clamp(RANGE[0], RANGE[1]);
            c.ratio = Some(ratio);
            self.ratio = Some(ratio);
            self.factor = next(self.factor, ratio);
        }
        self.open(now, accounts);
        true
    }

    fn open(&mut self, now: i64, accounts: &[Quant]) {
        let n = accounts.len();
        let w = &mut self.win;
        w.live = true;
        w.at = now;
        w.ids.clear();
        w.ids.extend(accounts.iter().map(|q| q.id));
        w.r.clear();
        w.r.extend(accounts.iter().map(|q| q.r));
        w.t.clear();
        w.t.extend(accounts.iter().map(|q| q.t));
        w.asked.clear();
        w.asked.resize(n, 0.0);
        w.skip.clear();
        w.skip.extend(accounts.iter().map(|q| !q.open || q.stale));
        self.last = now;
    }
}

/// The factor after a window's ratio: a factor of 1 moves only on a
/// ratio outside `BAND`, one in force follows the ratio until it lies
/// inside `BACK`.
fn next(factor: f64, ratio: f64) -> f64 {
    let band = if factor == 1.0 { BAND } else { BACK };
    if ratio >= band[0] && ratio <= band[1] { 1.0 } else { ratio }
}

#[cfg(test)]
#[path = "calib_tests.rs"]
mod tests;
