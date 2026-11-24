//! The 2026-11-24 14:30Z snapshot, row by row.
//!
//! The tables these rows come from were worked out with a Fable window
//! worth 0.57 of a week, so the rows are checked at that value.

use super::choose::{HELPER_LAST_H, MUST_LAST_H, Mover, loads_of, pick};
use super::plan::{Plan, plan};
use super::rest::{Resting, RestMove, rest_move};
use super::sim::world_at;
use super::sim_policies::{old_helper_pick, old_pick, old_rest};
use super::{Class, Load, Quant, shortage};

const KAPPA_SNAPSHOT: f64 = 0.57;

fn world() -> Vec<Quant> {
    world_at(KAPPA_SNAPSHOT)
}

const F_RATE: f64 = 0.035 / 3.0;
const N_RATE: f64 = 0.035 / 9.0;

fn screenshot() -> Vec<Quant> {
    let mut a = world();
    let p6 = a.iter_mut().find(|q| q.id == 6).unwrap();
    p6.r = 0.96;
    p6.f = KAPPA_SNAPSHOT * 0.92;
    a
}

fn twelve() -> Vec<Class> {
    let mut c = vec![Class::Restricted; 3];
    c.extend([Class::Plain; 9]);
    c
}

fn rate(c: Class) -> f64 {
    if c == Class::Restricted { F_RATE } else { N_RATE }
}

/// Place panes one after another with the must-move rule, the plan
/// counting all of them.
fn place(a: &[Quant], classes: &[Class], horizon: f64) -> Vec<u8> {
    let p = plan(a, 0.035, 0.035, Some(KAPPA_SNAPSHOT));
    let mut placed: Vec<(Option<u8>, Class, f64)> = Vec::new();
    for &c in classes {
        let loads = loads_of(a, &placed, None);
        let to = pick(a, &p, &loads, Mover { class: c, rate: rate(c) }, None, horizon).unwrap();
        placed.push((Some(to), c, rate(c)));
    }
    placed.iter().map(|(on, _, _)| on.unwrap()).collect()
}

fn first_seen(order: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for id in order {
        if !out.contains(id) {
            out.push(*id);
        }
    }
    out
}

#[test]
fn allocation_snapshot_three_quantities() {
    // (account, R, Φ, N, T, R/T in points an hour or None when spent)
    let rows: [(u8, f64, f64, f64, f64, Option<f64>); 8] = [
        (1, 0.0, 0.0, 0.0, 60.5, None),
        (2, 0.26, 0.0, 0.26, 126.5, Some(0.21)),
        (3, 0.50, 0.50, 0.0, 110.5, Some(0.45)),
        (4, 1.00, 0.57, 0.43, 72.5, Some(1.38)),
        (5, 0.0, 0.0, 0.0, 35.5, None),
        (6, 0.95, 0.52, 0.43, 62.5, Some(1.52)),
        (7, 0.44, 0.0, 0.44, 157.5, Some(0.28)),
        (8, 0.0, 0.0, 0.0, 14.5, None),
    ];
    let a = world();
    for (id, r, phi, n, t, cap) in rows {
        let q = a.iter().find(|q| q.id == id).unwrap();
        let close = |x: f64, y: f64, tol: f64| assert!((x - y).abs() <= tol, "P{id}: {x} vs {y}");
        close(q.r, r, 0.005);
        close(q.phi(), phi, 0.005);
        close(q.n(), n, 0.005);
        close(q.t, t, 0.05);
        match cap {
            None => assert!(!q.accepts(Class::Plain), "P{id} is spent"),
            Some(c) => close(q.rate_cap() * 100.0, c, 0.005),
        }
    }
    let total: f64 = a.iter().map(Quant::rate_cap).sum();
    assert!((total * 100.0 - 3.84).abs() < 0.01, "{total}");
    assert!(shortage(&a, 0.07), "seven points an hour is more than 3.84");
}

#[test]
fn allocation_snapshot_placement_new_rule() {
    for a in [world(), screenshot()] {
        let got = place(&a, &twelve(), MUST_LAST_H);
        assert_eq!(&got[..3], &[6, 4, 3], "one Fable pane each on P6, P4, P3");
        let mut plain = got[3..].to_vec();
        plain.sort();
        assert_eq!(plain, vec![2, 4, 4, 4, 6, 6, 6, 7, 7], "plain: P6 3, P4 3, P7 2, P2 1");
    }
}

#[test]
fn allocation_snapshot_placement_old_rule() {
    for a in [world(), screenshot()] {
        let mut fable_on = Vec::new();
        for _ in 0..3 {
            fable_on.push(old_pick(&a, Class::Restricted, None).unwrap());
        }
        assert_eq!(fable_on, vec![6, 6, 6]);
        assert!((0..9).all(|_| old_pick(&a, Class::Plain, None) == Some(2)), "all nine plain on P2");
    }
}

fn base() -> (Vec<Quant>, Plan) {
    let a = world();
    let p = plan(&a, 0.035, 0.035, Some(KAPPA_SNAPSHOT));
    (a, p)
}

#[test]
fn allocation_snapshot_must_move_rows() {
    for a in [world(), screenshot()] {
        // (a) plain refused on P1, P5, P8: P6 first, then P4, P2, P7 as each fills
        let plain = place(&a, &[Class::Plain; 9], MUST_LAST_H);
        assert_eq!(first_seen(&plain), vec![6, 4, 2, 7]);
        // (a) Fable refused: P6, then P4, then P3
        let fable = place(&a, &[Class::Restricted; 3], MUST_LAST_H);
        assert_eq!(first_seen(&fable), vec![6, 4, 3]);
        let p = plan(&a, 0.035, 0.035, Some(KAPPA_SNAPSHOT));
        let none = vec![Load::default(); a.len()];
        for from in [1, 5, 8] {
            assert_eq!(pick(&a, &p, &none, Mover { class: Class::Plain, rate: N_RATE }, Some(from), MUST_LAST_H), Some(6));
        }
        // (c) new helpers: plain P6 then P4; Fable P6
        let helpers = place(&a, &[Class::Plain; 6], HELPER_LAST_H);
        assert_eq!(first_seen(&helpers)[..2], [6, 4]);
        assert_eq!(pick(&a, &p, &none, Mover { class: Class::Restricted, rate: F_RATE }, None, HELPER_LAST_H), Some(6));
        // (d) a pane marked as needing Fable on P7, where Fable is spent
        assert!(!a.iter().find(|q| q.id == 7).unwrap().accepts(Class::Restricted));
        assert_eq!(pick(&a, &p, &none, Mover { class: Class::Restricted, rate: F_RATE }, Some(7), MUST_LAST_H), Some(6));
        // today's rule for the same rows
        assert_eq!(old_pick(&a, Class::Plain, Some(1)), Some(2));
        assert_eq!(old_pick(&a, Class::Restricted, Some(1)), Some(6));
        assert_eq!(old_helper_pick(&a, Class::Plain, &[]), Some(4));
        assert_eq!(old_helper_pick(&a, Class::Restricted, &[]), Some(4));
        assert_eq!(old_pick(&a, Class::Restricted, Some(7)), Some(6));
    }
}

/// The 5.2 placement as loads, with one resting plain pane added on `on`.
fn placed_loads(a: &[Quant], on: u8) -> Vec<Load> {
    let mut tab: Vec<(Option<u8>, Class, f64)> = [6, 4, 3].iter().map(|id| (Some(*id), Class::Restricted, F_RATE)).collect();
    tab.extend([6, 6, 6, 4, 4, 4, 2, 7, 7].iter().map(|id| (Some(*id), Class::Plain, N_RATE)));
    tab.push((Some(on), Class::Plain, N_RATE));
    loads_of(a, &tab, None)
}

fn resting<'a>(here: u8, loads: &'a [Load], demand: f64, keeper: bool) -> Resting<'a> {
    Resting {
        here,
        mover: Mover { class: Class::Plain, rate: N_RATE },
        loads,
        keeper,
        since_move_h: None,
        left: &[],
        band_last_hour: 0,
        demand,
        stale_feed: false,
        speed_known: true,
        keeps_restricted: false,
        moves_week: 0,
    }
}

#[test]
fn allocation_snapshot_resting_rows() {
    let (a, p) = base();
    // (b) resting plain pane on P3 in a shortage: P3's U is nothing (all
    // of its R is Fable's), so it leaves for P7, where only plain panes
    // can use what is left
    let on3 = placed_loads(&a, 3);
    assert_eq!(rest_move(&a, &p, &resting(3, &on3, 0.07, false)), Some(RestMove::UOut(7)));
    // ... and on P7: stays
    let on7 = placed_loads(&a, 7);
    assert_eq!(rest_move(&a, &p, &resting(7, &on7, 0.07, false)), None);
    // with supply above demand the one on P3 goes to P6 or P4
    let light = plan(&a, 0.005, 0.01, Some(KAPPA_SNAPSHOT));
    let few = loads_of(&a, &[(Some(3), Class::Plain, N_RATE), (Some(3), Class::Plain, N_RATE)], None);
    let mv = rest_move(&a, &light, &resting(3, &few, 0.008, false));
    assert!(matches!(mv, Some(RestMove::Band(6 | 4))), "{mv:?}");
    // today's rule moves both to P2
    assert_eq!(old_rest(&a, Class::Plain, 3, None), Some(2), "KeepsFable");
    assert_eq!(old_rest(&a, Class::Plain, 7, None), Some(2), "RenewsSooner");
}

#[test]
fn allocation_snapshot_busy_and_low_rows() {
    let (mut a, p) = base();
    // P3 nearly spent: lasts under two hours with its panes on it, and
    // what is left is not Fable's (that a plain pane leaves at once)
    a.iter_mut().find(|q| q.id == 3).unwrap().r = 0.0005;
    a.iter_mut().find(|q| q.id == 3).unwrap().f = 0.0;
    let on3 = placed_loads(&a, 3);
    let mv = rest_move(&a, &p, &resting(3, &on3, 0.07, false));
    assert!(matches!(mv, Some(RestMove::LeaveEarly(_))), "{mv:?}");
    // the slowest pane stays to use the rest
    assert_eq!(rest_move(&a, &p, &resting(3, &on3, 0.07, true)), None);
    // the cooldown and the hourly cap do not hold the leave back
    let mut r = resting(3, &on3, 0.07, false);
    r.since_move_h = Some(0.1);
    r.band_last_hour = 9;
    assert!(matches!(rest_move(&a, &p, &r), Some(RestMove::LeaveEarly(_))));
}
