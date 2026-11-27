//! What every run of the schedules reads besides the schedules.

use super::grid::Grid;
use super::sim::Sim;
use super::{H2, Input, Sched};
use crate::quota::EPS;

/// What every run reads besides the schedules.
pub(super) struct Ctx<'a, 'b> {
    pub input: &'a Input<'b>,
    pub grid: &'a Grid,
    /// Per account, when it empties at the high band if nobody moves,
    /// capped at the horizon: the deadline a switch off it is taken by.
    pub deadline: Vec<f64>,
    pub young_on: [Option<usize>; 2],
    /// Per account, it is open; it is open and fresh.
    pub open: Vec<bool>,
    pub fresh: Vec<bool>,
}

impl<'a, 'b> Ctx<'a, 'b> {
    /// The context on `grid`, whose deadlines it takes.
    pub fn new(input: &'a Input<'b>, grid: &'a Grid) -> Self {
        let fresh = |j: &usize| input.accounts[*j].q.open && !input.accounts[*j].q.stale;
        let best = |room: &dyn Fn(usize) -> f64| (0..input.accounts.len()).filter(fresh).filter(|&j| room(j) > EPS).max_by(|&a, &b| room(a).total_cmp(&room(b)).then(b.cmp(&a)));
        let young_on = [best(&|j| input.accounts[j].q.phi()), best(&|j| input.accounts[j].q.r)];
        let open = input.accounts.iter().map(|a| a.q.open).collect();
        let fresh = input.accounts.iter().map(|a| a.q.open && !a.q.stale).collect();
        Ctx { input, grid, deadline: grid.deadline.clone(), young_on, open, fresh }
    }

    /// When each account empties at the high band if nobody moves,
    /// capped at the horizon.  Cut points do not change a run in which
    /// nobody moves, so any grid of the same input gives the same.
    pub fn deadlines(input: &'a Input<'b>, grid: &'a Grid) -> Vec<f64> {
        let c = Ctx::new(input, grid);
        let mut s = Sim::new(&c);
        let stay = vec![Sched::STAY; input.panes.len()];
        s.run(&c, &stay, 1);
        s.empty.iter().map(|e| e.unwrap_or(H2)).collect()
    }
}
