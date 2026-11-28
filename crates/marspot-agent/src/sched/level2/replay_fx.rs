//! The recorded facts of 2026-11-24 20:00Z to 11-28 07:30Z, as the
//! replay of each profile reads them: the accounts at the start, every
//! pane's binds, rests and use, the turn boundaries recorded from
//! 11-26 21:45Z on, and the refusals today's rule moved panes for.
//!
//! The fixture was cut from the event log: use is each reply's tokens
//! at the price table, five-minute steps; a boundary's quiet is the
//! time to the pane's next turn.

use std::collections::BTreeMap;

use crate::quota::Class;

const FIXTURE: &str = include_str!("../../../tests/fixtures/replay_20261125_20261128.txt");

/// What a pane does at one recorded turn boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(in super::super) struct Bound {
    /// Background tasks or a process under the agent were live.
    pub live: bool,
    /// The user was at the pane.
    pub user: bool,
    /// Seconds until its next turn, at most an hour.
    pub quiet: i64,
    /// The context its last reply read, tokens; 0 unknown.
    pub ctx: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(in super::super) enum Ev {
    /// A pane there at the start, or one that changed account or class.
    Bind(u64, u8, Class),
    /// A pane opened in the window, on the account today's rule chose.
    New(u64, u8, Class),
    Gone(u64),
    Busy(u64, bool),
    Boundary(u64, Bound),
    /// Today's rule moved a refused pane off this account.
    Out(u64, u8),
}

#[derive(Clone, Debug, Default)]
pub(in super::super) struct Fx {
    pub t0: i64,
    pub len: i64,
    pub step: i64,
    /// Account, week left, Fable window left (weeks), when each renews.
    pub start: Vec<(u8, f64, f64, i64, i64)>,
    pub events: Vec<(i64, Ev)>,
    /// By step: pane, class of the model, weeks, context.
    pub usage: BTreeMap<i64, Vec<(u64, Class, f64, u64)>>,
    /// The first recorded boundary: before it, every rest counts as a chance.
    pub bounds_from: i64,
}

fn class_of(w: &str) -> Class {
    if w == "F" { Class::Restricted } else { Class::Plain }
}

pub(in super::super) fn fixture() -> Fx {
    let mut fx = Fx::default();
    for line in FIXTURE.lines().filter(|l| !l.starts_with('#')) {
        let w: Vec<&str> = line.split(' ').collect();
        let n = |i: usize| w[i].parse::<i64>().expect("a number");
        let x = |i: usize| w[i].parse::<f64>().expect("a number");
        let at = |i: usize| fx.t0 + n(i);
        let sid = |i: usize| n(i) as u64;
        match w[0] {
            "T" => (fx.t0, fx.len, fx.step) = (n(1), n(2), n(3)),
            "A" => fx.start.push((n(1) as u8, x(2), x(3), at(4), at(5))),
            "B" => fx.events.push((at(1), Ev::Bind(sid(2), n(3) as u8, class_of(w[4])))),
            "N" => fx.events.push((at(1), Ev::New(sid(2), n(3) as u8, class_of(w[4])))),
            "G" => fx.events.push((at(1), Ev::Gone(sid(2)))),
            "X" | "R" => fx.events.push((at(1), Ev::Busy(sid(2), w[0] == "X"))),
            "C" => fx.events.push((at(1), Ev::Boundary(sid(2), Bound { live: w[3] == "1", user: w[4] == "1", quiet: n(5), ctx: n(6) as u64 }))),
            "O" => fx.events.push((at(1), Ev::Out(sid(2), n(3) as u8))),
            "U" => fx.usage.entry(n(1)).or_default().push((sid(2), class_of(w[3]), x(4), n(5) as u64)),
            _ => panic!("unknown fixture line {line}"),
        }
    }
    fx.events.sort_by_key(|(at, _)| *at);
    fx.bounds_from = fx.events.iter().find(|e| matches!(e.1, Ev::Boundary(..))).map_or(i64::MAX, |e| e.0);
    fx
}

/// The refusals today's rule moved panes for, in the window.
pub(in super::super) fn refused_today(fx: &Fx) -> usize {
    fx.events.iter().filter(|e| matches!(e.1, Ev::Out(..))).count()
}
