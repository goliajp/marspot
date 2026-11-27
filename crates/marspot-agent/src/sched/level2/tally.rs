//! Simulations a pass ran, by what each was run for.

use std::fmt;

/// What a simulation was run for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// The start costed, the active set, the ranking of a round.
    Base,
    /// The exhaustive pass over joint choices.
    Exact,
    /// One pane's proposals tried.
    Try,
    /// Pair swaps and two-step chains.
    Moves,
    /// Gains per pane to fit the fleet's day, and trimming.
    Fit,
    Prune,
    /// Doing nothing, the plan at both bands, each switch's gain.
    Finish,
}

const STAGES: [Stage; 7] = [Stage::Base, Stage::Exact, Stage::Try, Stage::Moves, Stage::Fit, Stage::Prune, Stage::Finish];

impl Stage {
    pub fn word(self) -> &'static str {
        match self {
            Stage::Base => "base",
            Stage::Exact => "exact",
            Stage::Try => "try",
            Stage::Moves => "moves",
            Stage::Fit => "fit",
            Stage::Prune => "prune",
            Stage::Finish => "finish",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    by: [u32; 7],
    /// Simulations already counted.
    seen: usize,
}

impl Tally {
    /// Counts the simulations run since the last mark, `runs` in all so
    /// far, toward `stage`.
    pub fn mark(&mut self, stage: Stage, runs: usize) {
        self.by[stage as usize] += (runs - self.seen) as u32;
        self.seen = runs;
    }

    pub fn get(&self, stage: Stage) -> usize {
        self.by[stage as usize] as usize
    }

    pub fn total(&self) -> usize {
        self.by.iter().map(|&n| n as usize).sum()
    }
}

/// The stages that ran any, `try:300,moves:150`.
impl fmt::Display for Tally {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut sep = "";
        for s in STAGES {
            if self.get(s) > 0 {
                write!(f, "{sep}{}:{}", s.word(), self.get(s))?;
                sep = ",";
            }
        }
        Ok(())
    }
}
