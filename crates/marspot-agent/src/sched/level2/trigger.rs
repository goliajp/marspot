//! When a pass runs a full search instead of re-timing the standing
//! plan: a refusal or an account closing that the plan did not foresee
//! (at once), and at most once a minute: a committed switch ended, a
//! reading outside the band, a renewal, a pane that counts appearing or
//! ending, a pane's three-hour speed outside its band for half an
//! hour, a stored reset appearing, changing or claimed, a segment
//! boundary passing.

use crate::quota::{EPS, Quant};

/// Full searches for the rate-limited reasons are this far apart, seconds.
pub const EVERY_S: i64 = 60;
/// A speed outside its band this long counts, seconds.
pub const OUTSIDE_S: i64 = 1800;
/// One point of reading error.
const READING_ERR: f64 = 0.01;
const OUTSIDE_CAP: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    First,
    Refusal,
    Closed,
    Ended,
    Reading,
    Renewal,
    Panes,
    Speed,
    Offers,
    Segment,
}

impl Why {
    pub fn word(self) -> &'static str {
        match self {
            Why::First => "first",
            Why::Refusal => "refusal",
            Why::Closed => "closed",
            Why::Ended => "switch_ended",
            Why::Reading => "reading",
            Why::Renewal => "renewal",
            Why::Panes => "panes",
            Why::Speed => "speed",
            Why::Offers => "offers",
            Why::Segment => "segment",
        }
    }
}

/// What a pass sees.
pub struct Now<'a> {
    pub at: i64,
    pub accounts: &'a [Quant],
    /// Per account, the high-band load the plan put on it, weeks an hour.
    pub high: &'a [f64],
    /// Panes that count at pane level, sorted.
    pub counted: &'a [u64],
    pub offers: u64,
    /// A pane was refused since the last pass.
    pub refused: bool,
    /// A committed switch was done, missed or cancelled.
    pub ended: bool,
    /// Panes whose three-hour speed is outside their band now.
    pub outside: &'a [u64],
}

#[derive(Clone, Debug, PartialEq)]
struct Seen {
    at: i64,
    /// id, week left, open, hours to renewal, high-band load, hours until it empties at the high band.
    accounts: Vec<(u8, f64, bool, f64, f64, f64)>,
    counted: Vec<u64>,
    offers: u64,
    next_cut_s: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Triggers {
    last: Option<Seen>,
    outside: Vec<(u64, i64, bool)>,
}

impl Triggers {
    /// Why a full search is due now, if it is.
    pub fn due(&mut self, n: &Now) -> Option<Why> {
        self.track(n);
        let Some(last) = &self.last else { return Some(Why::First) };
        let dt_h = (n.at - last.at) as f64 / 3600.0;
        if n.refused {
            return Some(Why::Refusal);
        }
        let find = |id: u8| last.accounts.iter().find(|a| a.0 == id);
        let closed = n.accounts.iter().any(|q| find(q.id).is_some_and(|a| a.2 && (!q.open || q.r <= EPS) && a.5 > dt_h));
        if closed {
            return Some(Why::Closed);
        }
        if n.at - last.at < EVERY_S {
            return None;
        }
        let reading = n.accounts.iter().any(|q| find(q.id).is_some_and(|a| q.r < a.1 - a.4 * dt_h - READING_ERR || q.r > a.1 + READING_ERR));
        let renewal = last.accounts.iter().any(|a| a.3 > 0.0 && a.3 <= dt_h);
        let speed = self.outside.iter().any(|o| !o.2 && n.at - o.1 >= OUTSIDE_S);
        [
            (n.ended, Why::Ended),
            (renewal, Why::Renewal),
            (reading, Why::Reading),
            (n.counted != last.counted.as_slice(), Why::Panes),
            (speed, Why::Speed),
            (n.offers != last.offers, Why::Offers),
            (n.at >= last.next_cut_s, Why::Segment),
        ]
        .into_iter()
        .find(|x| x.0)
        .map(|x| x.1)
    }

    fn track(&mut self, n: &Now) {
        self.outside.retain(|o| n.outside.contains(&o.0));
        for &sid in n.outside {
            if !self.outside.iter().any(|o| o.0 == sid) && self.outside.len() < OUTSIDE_CAP {
                self.outside.push((sid, n.at, false));
            }
        }
    }

    /// A full search ran at `n.at`; `empty_high` per account (hours, by
    /// `n.accounts` order) and the plan's first cut after now.
    pub fn searched(&mut self, n: &Now, empty_high: &[Option<f64>], next_cut_h: f64) {
        for o in &mut self.outside {
            o.2 |= n.at - o.1 >= OUTSIDE_S;
        }
        let accounts = n
            .accounts
            .iter()
            .enumerate()
            .map(|(j, q)| (q.id, q.r, q.open && q.r > EPS, q.t, n.high.get(j).copied().unwrap_or(0.0), empty_high.get(j).copied().flatten().unwrap_or(f64::INFINITY)))
            .collect();
        let next_cut_s = n.at + (next_cut_h * 3600.0).round().max(1.0) as i64;
        self.last = Some(Seen { at: n.at, accounts, counted: n.counted.to_vec(), offers: n.offers, next_cut_s });
    }
}
