//! What the profile's Fable protection forbids as a target.
//!
//! Under `Protect` and `Strict`, and only while Fable is short (level
//! 1 prices its reserve, `pi > 0`), a pane that uses only other models
//! is not put on an account with nothing but Fable-usable quota left,
//! and a mixed pane (each class over a fifth of its speed: a Fable
//! session whose agents run on another model) goes only where both its
//! classes last it the stay at their own speeds.  A mixed pane no
//! account takes so is treated as a Fable pane: its other-model use
//! there is accepted, not a reason to move it.  Under `Price` nothing is
//! forbidden and the reserve is only priced.

use super::{FableGuard, Input, Pane};
use crate::quota::{EPS, Quant};

/// A class over this share of a pane's speed makes it use both.
const MIXED_SHARE: f64 = 0.2;

pub(super) fn mixed(p: &Pane) -> bool {
    let total = p.speed[0] + p.speed[1];
    total > 0.0 && p.speed[0] > MIXED_SHARE * total && p.speed[1] > MIXED_SHARE * total
}

fn holds_both(input: &Input, p: &Pane, q: &Quant) -> bool {
    let stay = input.limits.dwell_h;
    q.n() >= p.speed[1] * stay - EPS && q.phi() >= p.speed[0] * stay - EPS
}

/// Pane `p` may be put on the account of `q` as far as the protection goes.
pub(super) fn admits(input: &Input, p: &Pane, q: &Quant) -> bool {
    if input.limits.fable == FableGuard::Price || input.pi <= 0.0 {
        return true;
    }
    if !p.restricted {
        return q.n() > EPS;
    }
    if !mixed(p) {
        return true;
    }
    holds_both(input, p, q) || !input.accounts.iter().any(|a| a.q.id != p.on && a.q.open && !a.q.stale && holds_both(input, p, &a.q))
}

/// The unattended other-model panes `Strict` would make wait: while
/// Fable is short and no open account has anything but Fable-usable
/// quota left.  An attended pane keeps working; the executor rehearses
/// these waits and records any that would hold a pane someone is at.
pub fn waits(input: &Input) -> Vec<u64> {
    let none_else = !input.accounts.iter().any(|a| a.q.open && !a.q.stale && a.q.n() > EPS);
    if input.limits.fable != FableGuard::Strict || input.pi <= 0.0 || !none_else {
        return Vec::new();
    }
    input.panes.iter().filter(|p| !p.restricted && !p.attended).map(|p| p.sid).collect()
}

#[cfg(test)]
#[path = "guard_tests.rs"]
mod tests;
