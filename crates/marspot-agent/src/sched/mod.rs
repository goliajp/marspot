//! The scheduler's pure core: one selector for every decision about
//! where quota, context and money go.
//!
//! `choose` picks among candidates by tiers (money, then efficiency,
//! then fairness); `constrain` removes what may not be done before any
//! term is compared; `price` says what a point of each account is
//! worth; `alloc` turns an account decision of `quota` into candidates;
//! `state` is what the scheduler remembers, in a fixed size; `forecast`
//! is the demand the planner plans on, derived from facts; `level1` is
//! the class plan over the week ahead.  Nothing
//! here reads a file, starts a thread or knows which agent it serves.

pub mod alloc;
pub mod choose;
pub mod constrain;
pub mod forecast;
pub mod level1;
pub mod price;
pub mod reset;
pub mod state;

pub use choose::{Choice, POLICY, Policy, Terms, Tier, choose};
pub use price::{Price, price};
