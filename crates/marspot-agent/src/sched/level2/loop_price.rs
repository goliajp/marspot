//! What a switch costs in the closed loop.
//!
//! `Fixed` is the charge every switch was given before switches were
//! measured, for one context of 400k tokens: the loop's recorded digests
//! were taken with it.  `Context` prices each switch by the context the
//! pane has then (`sched::switch_cost::weeks`): every account is in an
//! organization of its own, so a session resumed on another finds none
//! of its prompt cache and writes the whole conversation again, and the
//! thinking the old organization signed is said again.
//!
//! A pane's context starts somewhere between 30k and 600k tokens, grows
//! by 10M tokens a week of use (a pane at a point an hour adds 100k an
//! hour) and is compacted to 40k at 800k, as a session is.  The starts
//! come from a stream of their own, so `Fixed` runs draw what they drew
//! before.  The growth, the compaction and `MEASURED` are assumptions
//! until this machine's switches are fitted (`switch_cost_report`).

use super::oracle_tests::Rng;
use crate::quota::Class;
use crate::quota::waste::switch_cost;
use crate::sched::switch_cost::{Shares, weeks};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Pricing {
    #[default]
    Fixed,
    Context(Shares),
}

/// The whole conversation written once to the one-hour cache, and
/// half a percent of it said again over the next replies.
pub(super) const MEASURED: Shares = Shares { rewrite: 1.0, one_hour: 1.0, regen: 0.005 };
pub(super) const FIXED_CTX: u64 = 400_000;
/// A context this long counts as a long session in the loop's report.
pub(super) const LONG: u64 = 400_000;
const START: [f64; 2] = [30_000.0, 600_000.0];
const TOKENS_PER_WEEK: f64 = 1e7;
const COMPACT_AT: u64 = 800_000;
const COMPACTED: u64 = 40_000;

/// Each pane's context at the start of a run.
pub(super) fn starts(seed: u64, n: usize, pricing: Pricing) -> Vec<u64> {
    let mut rng = Rng(seed ^ 0xC0_7E77_0C7E_57A7);
    (0..n).map(|_| match pricing {
        Pricing::Fixed => FIXED_CTX,
        Pricing::Context(_) => (START[0] + (START[1] - START[0]) * rng.next()) as u64,
    }).collect()
}

/// The context after `used` weeks more of a pane's work.
pub(super) fn grown(ctx: u64, used: f64, pricing: Pricing) -> u64 {
    match pricing {
        Pricing::Fixed => ctx,
        Pricing::Context(_) => {
            let next = ctx + (used * TOKENS_PER_WEEK) as u64;
            if next >= COMPACT_AT { COMPACTED } else { next }
        }
    }
}

/// What switching a pane of `class` with `ctx` tokens costs, weeks.
pub(super) fn cost(pricing: Pricing, class: Class, ctx: u64) -> f64 {
    match pricing {
        Pricing::Fixed => switch_cost(class, Some(ctx)),
        Pricing::Context(sh) => weeks(Some(sh), class, ctx),
    }
}
