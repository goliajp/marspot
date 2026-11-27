//! What level 2 plans on, from one scan: the class plan's accounts and
//! prices, the panes the forecast counts at pane level with their
//! levels and bands, and the rest of the fleet's use as demand below
//! pane level.
//!
//! A pane's level is the forecast's 24-hour mean per class, its
//! three-hour speed when that lies above the pane band around the mean,
//! the band's bottom when the three-hour speed lies below it (a pane
//! idle for three hours is not planned at nothing), and the scan's
//! measured speed before it has a mean; its high band is
//! the level times the pane band's high multiplier.  A switch rewrites the context the
//! pane's last reply read (the typical one where none was read).  Lost
//! background work is the pane's plain speed times its background-work
//! length.  The dwell runs from the pane's last change of account,
//! whoever made it; a pin from the standing plan comes on top.

use std::hash::{Hash, Hasher};

use marspot_agent::sched::forecast::Forecast;
use marspot_agent::sched::level1::{H2, Level1};
use marspot_agent::sched::level2::standing::Standing;
use marspot_agent::sched::level2::{Account, DWELL_H, FLEET_PER_DAY, Pane, planned_speed};

use super::allocation_adapter::{Allocation, WEEK_S};
use super::allocation_decide::STALE_FEED_S;
use super::allocation_inputs::Snapshot;
use super::allocation_level1::Inputs as ClassInputs;
use super::allocation_view::View;
use crate::plugins::allocation::rest::MOVES_PER_WEEK;
use crate::plugins::allocation::waste::switch_cost;
use crate::plugins::allocation::Class;

/// Background work this long or longer is long, seconds.
const LONG_BACKGROUND_S: u32 = 900;
/// Disturbing actions a pane may take in a day.
const DISTURB_PER_DAY: u8 = 4;
/// Panes planned at pane level at most.
pub(super) const PANES_CAP: usize = 64;

/// Level 2's inputs, owned.
pub(super) struct Built {
    pub accounts: Vec<Account>,
    pub panes: Vec<Pane>,
    pub young: [f64; 2],
    pub pi: f64,
    pub fleet_left: usize,
    pub stale_feed: bool,
    /// Panes whose three-hour speed is outside their band.
    pub outside: Vec<u64>,
}

pub(super) fn build(a: &Allocation, snap: &Snapshot, view: &View, forecast: Option<&Forecast>, class: &(ClassInputs, Level1), standing: &Standing) -> Built {
    let now = snap.now;
    let (ci, l1) = class;
    let accounts = ci.accounts.iter().enumerate().map(|(j, q)| Account { q: q.clone(), spare: l1.spare[j], price: l1.price[j].unwrap_or([0.0, 0.0]) }).collect();
    let mut young = [0.0; 2];
    let mut panes = Vec::new();
    let mut outside = Vec::new();
    for row in &view.panes {
        let (sid, on, cls, rate, _helper) = *row;
        let scan_a = view.fable.get(&sid).copied().unwrap_or(if cls == Class::Restricted { rate } else { 0.0 }).min(rate);
        let fc = forecast.and_then(|f| f.panes.iter().find(|p| p.sid == sid));
        let Some(fp) = fc.filter(|p| p.counts) else {
            young[0] += scan_a;
            young[1] += rate - scan_a;
            continue;
        };
        if panes.len() >= PANES_CAP {
            young[0] += scan_a;
            young[1] += rate - scan_a;
            continue;
        }
        let band = forecast.map_or([2.5, 1.5], |f| [f.pane_band[0].hi, f.pane_band[1].hi]);
        let lo = forecast.map_or([0.3, 0.6], |f| [f.pane_band[0].lo, f.pane_band[1].lo]);
        let level = |c: usize, scan: f64| if fp.day[c] > 0.0 { planned_speed(fp.day[c], fp.short[c], lo[c], band[c]) } else { scan };
        let speed = [level(0, scan_a), level(1, rate - scan_a)];
        if (0..2).any(|c| fp.day[c] > 0.0 && (fp.short[c] > fp.day[c] * band[c] || fp.short[c] < fp.day[c] * lo[c])) {
            outside.push(sid);
        }
        let last = a.last.get(&sid);
        let hours = |at: i64| (now - at) as f64 / 3600.0;
        let since = last.and_then(|l| l.bound_at).map_or(f64::INFINITY, hours);
        let pinned = standing.pinned_h(sid, now).unwrap_or(0.0);
        let moves_week = last.map_or(0, |l| l.rest_moves.iter().filter(|t| now - **t < WEEK_S).count());
        let tokens = view.tokens.get(&sid).copied();
        panes.push(Pane {
            sid,
            on,
            restricted: cls == Class::Restricted,
            speed,
            high: [speed[0] * band[0], speed[1] * band[1]],
            rewrite: switch_cost(cls, tokens),
            lost_work: speed[1] * f64::from(fp.background_s.unwrap_or(0)) / 3600.0,
            clean_per_h: fp.clean_per_h,
            long_background: fp.background_s.is_some_and(|s| s >= LONG_BACKGROUND_S),
            earliest_h: (DWELL_H - since).max(0.0).max(pinned).min(H2),
            left: last.and_then(|l| l.left).map(|(id, at)| (id, DWELL_H - hours(at))),
            barred: 0,
            switches_left: MOVES_PER_WEEK.saturating_sub(moves_week) as u8,
            disturb_left: DISTURB_PER_DAY,
        });
    }
    Built {
        accounts,
        panes,
        young,
        pi: l1.pi,
        fleet_left: FLEET_PER_DAY.saturating_sub(a.day_moves.len() + a.day_pending.len()),
        stale_feed: snap.feed_at.is_none_or(|f| now - f > STALE_FEED_S),
        outside,
    }
}

/// A digest of what the plan read, in tenths of a point and minutes.
pub(super) fn digest(b: &Built) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let q = |x: f64| (x * 1e3).round() as i64;
    for a in &b.accounts {
        (a.q.id, q(a.q.r), q(a.q.f), (a.q.t * 60.0) as i64, a.q.open, q(a.spare)).hash(&mut h);
    }
    for p in &b.panes {
        (p.sid, p.on, q(p.speed[0] * 100.0), q(p.speed[1] * 100.0)).hash(&mut h);
    }
    (q(b.young[0] * 100.0), q(b.young[1] * 100.0), q(b.pi)).hash(&mut h);
    h.finish()
}
