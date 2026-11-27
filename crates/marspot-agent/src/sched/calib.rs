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

use crate::quota::Quant;

/// One window of readings, seconds.
pub const WINDOW_S: i64 = 6 * 3600;
/// Least use a window must see to move the factor, weeks (ten points).
pub const MIN_USE: f64 = 0.1;
/// Ratios inside this band are ordinary error: the factor is 1.
pub const BAND: [f64; 2] = [0.7, 1.4];
/// The factor never leaves this range: a ratio past it says more about
/// the readings than about the speeds.
pub const RANGE: [f64; 2] = [0.2, 5.0];
/// An account reading this little or less at the window's end ran dry.
const DRY: f64 = 0.02;

/// The window open now, per account by input order.
#[derive(Clone, Debug, Default)]
struct Window {
    at: i64,
    ids: Vec<u8>,
    r: Vec<f64>,
    t: Vec<f64>,
    /// The week the handed speeds put on each account since `at`.
    asked: Vec<f64>,
    /// The account was closed at some reading inside the window.
    closed: Vec<bool>,
}

#[derive(Clone, Debug)]
pub struct Calib {
    win: Option<Window>,
    last: i64,
    factor: f64,
    /// The last window's ratio, for the log.
    pub ratio: Option<f64>,
}

impl Default for Calib {
    fn default() -> Self {
        Calib { win: None, last: 0, factor: 1.0, ratio: None }
    }
}

impl Calib {
    /// The factor every pane speed is scaled by.
    pub fn factor(&self) -> f64 {
        self.factor
    }

    /// A reading at `now` (seconds): the accounts as read, and the week an
    /// hour the speeds handed to the planner (before the factor) put on
    /// each, by input order.
    pub fn observe(&mut self, now: i64, accounts: &[Quant], asked: &[f64]) {
        let same = self.win.as_ref().is_some_and(|w| w.ids.len() == accounts.len() && w.ids.iter().zip(accounts).all(|(a, q)| *a == q.id));
        if !same {
            self.open(now, accounts);
            return;
        }
        let dt = (now - self.last).max(0) as f64 / 3600.0;
        self.last = now;
        let Some(w) = self.win.as_mut() else { return };
        for (j, q) in accounts.iter().enumerate() {
            w.asked[j] += asked[j] * dt;
            w.closed[j] |= !q.open;
        }
        if now - w.at < WINDOW_S {
            return;
        }
        let (mut used, mut want) = (0.0, 0.0);
        for (j, q) in accounts.iter().enumerate() {
            let renewed = q.t > w.t[j] + 1e-6 || q.r > w.r[j] + 0.05;
            if renewed || w.closed[j] || q.r <= DRY {
                continue;
            }
            used += w.r[j] - q.r;
            want += w.asked[j];
        }
        if want >= MIN_USE {
            let ratio = (used / want).clamp(RANGE[0], RANGE[1]);
            self.ratio = Some(ratio);
            self.factor = if ratio >= BAND[0] && ratio <= BAND[1] { 1.0 } else { ratio };
        }
        self.open(now, accounts);
    }

    fn open(&mut self, now: i64, accounts: &[Quant]) {
        let n = accounts.len();
        let w = self.win.get_or_insert_with(Window::default);
        w.at = now;
        w.ids.clear();
        w.ids.extend(accounts.iter().map(|q| q.id));
        w.r.clear();
        w.r.extend(accounts.iter().map(|q| q.r));
        w.t.clear();
        w.t.extend(accounts.iter().map(|q| q.t));
        w.asked.clear();
        w.asked.resize(n, 0.0);
        w.closed.clear();
        w.closed.extend(accounts.iter().map(|q| !q.open));
        self.last = now;
    }
}

#[cfg(test)]
#[path = "calib_tests.rs"]
mod tests;
