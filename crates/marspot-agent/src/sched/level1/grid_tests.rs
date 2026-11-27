//! The event sweep against the hour grid it replaces: an hourly
//! earliest-renewal-first pass of the restricted class over 168 hours
//! with renewals and no rate cap, which is what the reserve, the spare
//! and the unserved restricted demand were defined by.  The grid exists
//! only here, as the reference.  Renewals fall on whole hours, where
//! the grid is exact.

use super::tests::{KAPPA, Rng, q, random_accounts, world};
use super::*;
use crate::quota::replay::Inputs;

const ROWS: &str = include_str!("../../../tests/fixtures/alloc_decisions_20261126.tsv");
/// 0.01 points.
const TOL: f64 = 1e-4;
/// The highest six-hour restricted bucket of the week the rows come from.
const A_HI_ROWS: f64 = 0.0339;

/// Per account what the hourly pass at `rate` takes before the week
/// renews, and the restricted demand it could not place.
fn hour_grid(accounts: &[Quant], rate: f64) -> (Vec<f64>, f64) {
    let mut s: Vec<(f64, f64, f64, bool)> = accounts.iter().map(|a| (a.r, a.f, a.t, false)).collect();
    let mut reserve = vec![0.0; accounts.len()];
    let mut unserved = 0.0;
    for _ in 0..168 {
        let mut order: Vec<usize> = (0..s.len()).filter(|&i| s[i].1.min(s[i].0) > EPS).collect();
        order.sort_by(|&a, &b| s[a].2.total_cmp(&s[b].2).then(accounts[a].id.cmp(&accounts[b].id)));
        let mut need = rate;
        for i in order {
            let x = s[i].1.min(s[i].0).min(need);
            s[i].0 -= x;
            s[i].1 -= x;
            need -= x;
            if !s[i].3 {
                reserve[i] += x;
            }
        }
        unserved += need;
        for a in &mut s {
            a.2 -= 1.0;
            if a.2 <= 0.0 {
                (a.0, a.1, a.2, a.3) = (1.0, KAPPA, a.2 + 168.0, true);
            }
        }
    }
    (reserve, unserved)
}

fn compare(accounts: &[Quant], demand: [f64; 2], prudent: f64, what: &str) -> f64 {
    let p = plan(&world(accounts, demand, prudent));
    let (reserve, _) = hour_grid(accounts, prudent);
    let (_, unserved) = hour_grid(accounts, demand[0]);
    for (k, a) in accounts.iter().enumerate() {
        assert!((p.reserve[k] - reserve[k]).abs() < TOL, "{what}: P{} reserve {} against the grid's {}", a.id, p.reserve[k], reserve[k]);
        let spare = (a.r - reserve[k]).max(a.n());
        assert!((p.spare[k] - spare).abs() < TOL, "{what}: P{} spare {} against {spare}", a.id, p.spare[k]);
    }
    let got = p.unserved_until(0, f64::INFINITY);
    assert!((got - unserved).abs() < TOL, "{what}: unserved {got} against the grid's {unserved}");
    reserve.iter().sum()
}

/// Every logged decision's accounts, renewals rounded to whole hours.
#[test]
fn sched_level1_equals_the_hour_grid_on_the_logged_accounts() {
    let (mut n, mut reserved) = (0, 0.0);
    for line in ROWS.lines().filter(|l| !l.starts_with('#')) {
        let Some(i) = line.split('\t').nth(5).and_then(Inputs::decode) else { continue };
        let accounts: Vec<Quant> = i.accounts.iter().map(|a| Quant { t: a.t.round().max(1.0), tf: a.t.round().max(1.0), ..*a }).collect();
        reserved += compare(&accounts, [i.b_f, i.b_n], A_HI_ROWS.max(i.b_f), &format!("row {n}"));
        n += 1;
    }
    assert!(n >= 90 && reserved > 1.0, "{n} rows decoded, {reserved} reserved");
}

#[test]
fn sched_level1_equals_the_hour_grid_on_generated_worlds() {
    let mut rng = Rng(7);
    for w in 0..200 {
        let n = 2 + (rng.next() * 15.0) as usize;
        let accounts = random_accounts(&mut rng, n, 1.0);
        let a = rng.next() * 0.05;
        let _ = compare(&accounts, [a, rng.next() * 0.06], a * (1.0 + rng.next() * 2.0), &format!("world {w}"));
    }
}

/// CA's case of 11-26 18:24: P3 with 7 points to renewal at 58 h; at the
/// prudent rate all of P3's Phi is reserve and P3 has no spare.
#[test]
fn sched_level1_reserves_all_of_a_small_restricted_remainder() {
    let a = [q(3, 0.07, 0.07, 58.0), q(5, 0.92, 0.47, 120.0), q(7, 0.2, 0.0, 105.0), q(8, 0.59, 0.449, 140.0)];
    let _ = compare(&a, [0.012, 0.03], 0.0339, "18:24");
    let p = plan(&world(&a, [0.012, 0.03], 0.0339));
    assert!((p.reserve[0] - 0.07).abs() < TOL && p.spare[0].abs() < TOL, "{:?} {:?}", p.reserve, p.spare);
}
