//! Stored resets as supply with a deadline, and when an account counts
//! as idle.  What a reset is and how two readings of one compare are
//! types every agent shares (`types::reset`).
//!
//! An account's quota is what is left of it plus what its resets can
//! bring back before they end.  A reset that may be used only once the
//! account is at its limit brings back a whole window, and only if the
//! panes on the account reach the limit before the reset ends and
//! before the window renews by itself; one usable any time is used at
//! the last moment it still can be, and brings back what was used by
//! then.  Nothing here uses a reset: this is what the allocation counts
//! on, nothing more.

use crate::quota::{EPS, Load, Quant};
pub use crate::types::reset::{AGREE_WITHIN_S, Agreement, Count, Supply, WeekCut, agree};

const WEEK_H: f64 = 168.0;

/// No account has more than this many resets counted at once; more
/// cannot be used inside one week anyway.
const MAX_UNITS: usize = 16;

/// When one reset may be used and what it brings back.
#[derive(Clone, Copy)]
struct Unit {
    starts_h: f64,
    ends_h: f64,
    requires_limit: bool,
    moves_week: bool,
    usable_below_left: Option<f64>,
    gives_back_week: bool,
    week_cut: WeekCut,
    /// Brings back the week and the restricted window at one use.
    both: bool,
}

/// One reset used: when, and what it added.
#[derive(Clone, Copy)]
struct Use {
    unit: Unit,
    at: f64,
    gain: f64,
}

/// The resets of `units`, in order, used one after another on a window
/// that has `left` now, holds `full` when renewed, is used at `rate` an
/// hour and renews in `renews_h`.  A use that moves the week also moves
/// the renewal, so a later reset may be used past the first renewal.
fn claims(left: f64, full: f64, rate: f64, mut renews_h: f64, units: &[Unit]) -> Vec<Option<Use>> {
    let (mut h, mut left) = (0.0, left.max(0.0));
    if rate <= EPS {
        return vec![None; units.len()];
    }
    let mut out = Vec::with_capacity(units.len());
    for u in units {
        let empty = h + left / rate;
        let at = if u.requires_limit { empty } else { empty.min(u.ends_h) }.max(u.starts_h).max(h);
        let before = (left - rate * (at - h)).max(0.0);
        // used at its very end at the latest; the window renewing by itself makes it worth nothing
        let late = at > u.ends_h || at >= renews_h;
        let too_full = u.usable_below_left.is_some_and(|share| before >= share * full);
        if late || too_full || full - before <= EPS {
            out.push(None);
            continue;
        }
        out.push(Some(Use { unit: *u, at, gain: full - before }));
        (h, left) = (at, full);
        if u.moves_week {
            renews_h = renews_h.max(at + WEEK_H);
        }
    }
    out
}

fn units(supply: &[Supply], pick: impl Fn(&Supply) -> bool) -> Vec<Unit> {
    let mut out: Vec<Unit> = supply
        .iter()
        .filter(|s| pick(s))
        .flat_map(|s| {
            let u = Unit {
                starts_h: s.starts_h.max(0.0),
                ends_h: s.ends_h.unwrap_or(f64::INFINITY),
                requires_limit: s.requires_limit,
                moves_week: s.moves_week,
                usable_below_left: s.usable_below_left,
                gives_back_week: s.gives_back_week,
                week_cut: s.week_cut,
                both: s.week && s.window,
            };
            std::iter::repeat_n(u, s.left as usize)
        })
        .collect();
    out.sort_by(|a, b| a.ends_h.total_cmp(&b.ends_h));
    out.truncate(MAX_UNITS);
    out
}

/// Account `q` with `load` on it, counting what `supply` brings back
/// before it ends; `window_full` is what a renewed restricted window
/// holds, in weeks.  With no supply, `q` as it is.
///
/// A reset that brings back both the week and the window is used once:
/// when the week runs out if it is worth something there, and the window
/// is refilled at that same moment; otherwise when the window runs out.
/// One that brings back the window alone takes its `week_cut` off what
/// the week has used by then, read without the week's own resets.
pub fn with_supply(q: &Quant, load: &Load, window_full: f64, supply: &[Supply]) -> Quant {
    let mut out = q.clone();
    if supply.is_empty() {
        return out;
    }
    let week_units = units(supply, |s| s.week);
    let week = claims(q.r, 1.0, load.total(), q.t, &week_units);
    for u in week.iter().flatten() {
        out.r += u.gain;
        if u.unit.moves_week {
            out.t = out.t.max(u.at + WEEK_H);
        }
    }
    let mut window_units: Vec<Unit> = units(supply, |s| s.window && !s.week);
    for (unit, used) in week_units.iter().zip(&week).filter(|(u, _)| u.both) {
        window_units.push(match used {
            Some(w) => Unit { starts_h: w.at, ends_h: w.at, requires_limit: false, ..*unit },
            None => *unit,
        });
    }
    // only the week's renewal moves
    for u in &mut window_units {
        u.moves_week = false;
    }
    window_units.sort_by(|a, b| a.ends_h.total_cmp(&b.ends_h));
    window_units.truncate(MAX_UNITS);
    for u in claims(q.f, window_full, load.restricted, q.tf, &window_units).iter().flatten() {
        out.f += u.gain;
        if u.unit.gives_back_week {
            out.r += u.gain;
        }
        if !u.unit.both {
            let week_left = (q.r - load.total() * u.at).max(0.0);
            out.r += u.unit.week_cut.of(1.0 - week_left);
        }
    }
    out
}

/// An account is idle when every pane on it waits for its user; one
/// with no pane is idle.
pub fn account_idle(resting: impl IntoIterator<Item = bool>) -> bool {
    resting.into_iter().all(|r| r)
}

#[cfg(test)]
#[path = "reset_tests.rs"]
mod tests;
