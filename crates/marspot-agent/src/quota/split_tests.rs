//! A pane's two speeds: what it draws from the restricted window is its
//! restricted speed `a`, not its whole speed, wherever it may sit.

use super::choose::{Mover, loads_of, loads_split};
use super::replay::Inputs;
use super::{Class, Load, Quant, lasts, lasts_of};
use crate::sched::price::pool;

fn q(id: u8, r: f64, f: f64, t: f64) -> Quant {
    Quant { id, r, f, t, tf: t, open: true, stale: false }
}

/// The open accounts of 2026-11-26T18:24:37Z.
fn accounts_1126() -> Vec<Quant> {
    vec![q(3, 0.07, 0.45408, 58.589), q(5, 0.92, 0.46992, 151.59), q(7, 0.2, 0.0, 105.59), q(8, 0.59, 0.4488, 130.59)]
}

/// The panes of that hour (CA 1.3), points an hour / 100: account,
/// class, Fable speed, other speed.
fn panes_1126() -> Vec<(u8, Class, f64, f64)> {
    vec![
        (3, Class::Restricted, 0.0021, 0.0082), // 428: a Fable session, its subagents on Opus
        (3, Class::Plain, 0.0, 0.0060),         // 441
        (3, Class::Plain, 0.0, 0.0033),         // 430
        (3, Class::Plain, 0.0, 0.0031),         // 481
        (5, Class::Restricted, 0.0017, 0.0126), // 482, marked as needing Fable
        (8, Class::Plain, 0.0, 0.0072),         // 488
        (7, Class::Plain, 0.0, 0.0025),         // 483 and 487
    ]
}

#[test]
fn split_a_pane_with_428s_speeds_puts_fable_demand_at_0_21_not_1_03() {
    let acc = accounts_1126();
    let one = [(Some(3), 0.0021, 0.0082)];
    let split = loads_split(&acc, &one, None);
    assert!((split[0].restricted * 100.0 - 0.21).abs() < 1e-9, "{split:?}");
    assert!((split[0].plain * 100.0 - 0.82).abs() < 1e-9);
    let whole = loads_of(&acc, &[(Some(3), Class::Restricted, 0.0103)], None);
    assert!((whole[0].restricted * 100.0 - 1.03).abs() < 1e-9, "the whole speed under its class, as before");
}

/// Every account's Fable window priced at 1 on 2026-11-26 because the
/// restricted panes' whole speed was taken for Fable demand.
#[test]
fn split_price_pool_reads_true_fable_demand_against_what_fable_can_use() {
    let acc = accounts_1126();
    let panes = panes_1126();
    let whole: Vec<(Option<u8>, Class, f64)> = panes.iter().map(|(a, c, f, n)| (Some(*a), *c, f + n)).collect();
    let split: Vec<(Option<u8>, f64, f64)> = panes.iter().map(|(a, _, f, n)| (Some(*a), *f, *n)).collect();
    assert!(pool(&acc, &loads_of(&acc, &whole, None)).restricted, "as logged: price=1.00/1.00");
    assert!(!pool(&acc, &loads_split(&acc, &split, None)).restricted, "0.38 points an hour of Fable is not short");
    // what Fable can use is min(F, R): P3's window holds 45 points, its week 7
    let cap: f64 = acc.iter().map(|x| x.phi() / x.tf).sum();
    assert!(cap * 100.0 > 0.38 && cap * 100.0 < 1.0, "{cap}");
}

#[test]
fn split_a_mover_without_two_speeds_lasts_as_its_whole_speed_under_its_class() {
    let acc = q(3, 0.07, 0.03, 58.6);
    let load = Load { restricted: 0.004, plain: 0.012 };
    for class in [Class::Restricted, Class::Plain] {
        let m = Mover { class, rate: 0.01, tokens: None, fable: None };
        assert_eq!(lasts_of(&acc, &load, m), lasts(&acc, &load, class, 0.01));
        assert_eq!(m.a() + m.b(), 0.01);
    }
    // 428 judged where its window runs out: by its Fable, not its Opus
    let mixed = Mover { class: Class::Restricted, rate: 0.0103, tokens: None, fable: Some(0.0021) };
    let by_window: f64 = 0.03 / (0.004 + 0.0021);
    let by_week = 0.07 / (0.016 + 0.0103);
    assert!((lasts_of(&acc, &load, mixed) - by_window.min(by_week)).abs() < 1e-12);
    assert!(lasts_of(&acc, &load, mixed) > lasts(&acc, &load, Class::Restricted, 0.0103));
    let mut l = load;
    l.add_mover(mixed, 1.0);
    l.add_mover(mixed, -1.0);
    assert!((l.restricted - load.restricted).abs() < 1e-15 && (l.plain - load.plain).abs() < 1e-15);
}

#[test]
fn split_rows_carry_a_and_b_and_a_row_without_them_reads_as_before() {
    let line = "here=3 class=restricted rate=0.0103 tokens=789497 kappa=0.528 bf=0.0096 bn=0.0446 acc=3:0.07:0.45408:58.589:58.589:1:0;7:0.2:0:105.59:105.59:1:0 loads=0.0021:0.0082;0:0.0025 kind=out horizon=6";
    let old = Inputs::decode(line).unwrap();
    assert_eq!(old.mover.fable, None);
    assert_eq!((old.mover.a(), old.mover.b()), (0.0103, 0.0));
    let mut new = old.clone();
    new.mover.fable = Some(0.0021);
    let text = new.encode();
    assert!(text.contains(" rate=0.0103 a=0.0021 b=0.0082"), "{text}");
    let back = Inputs::decode(&text).unwrap();
    assert_eq!(back.mover.fable, Some(0.0021));
    assert_eq!(back.encode(), text);
    assert!(!old.encode().contains(" a="), "a row with one speed stays one");
}
