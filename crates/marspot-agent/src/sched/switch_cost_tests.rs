use super::*;

fn warm(at: i64, ctx: u64, out: u64) -> Turn {
    Turn { at, input: 2, write_1h: 1_000, read: ctx, output: out, ..Turn::default() }
}

#[test]
fn switch_cost_fit_order_does_not_matter_and_is_bounded() {
    let a = Sample { at: 1, ctx: 100, rewrite: 300, rewrite_1h: 0, regen: -50, hit_after: 0, hit_base: 990 };
    let b = Sample { at: 2, ctx: 900, rewrite: 900, rewrite_1h: 900, regen: 10, ..a };
    let (mut x, mut y) = (Fit::default(), Fit::default());
    for s in [a, b, a, b, a] {
        x.add(&s);
    }
    for s in [b, a, a, a, b] {
        y.add(&s);
    }
    assert_eq!(x, y);
    let sh = x.measured().unwrap();
    assert!(sh.rewrite <= 2.0 && sh.regen == 0.0);
    assert!((sh.one_hour - 1800.0 / 2700.0).abs() < 1e-12);
}

#[test]
fn switch_cost_needs_a_warm_baseline() {
    // cold replies before the switch are not a baseline
    let mut t: Vec<Turn> = (0..4).map(|i| Turn { at: i * 60, input: 5, write_5m: 100_000, ..Turn::default() }).collect();
    t.push(Turn { at: 600, input: 5, write_5m: 101_000, ..Turn::default() });
    assert_eq!(sample(&t, 300, None), None);
    let mut w: Vec<Turn> = (0..5).map(|i| warm(i * 60, 100_000, 200)).collect();
    w.push(Turn { at: 400, input: 3, write_5m: 101_000, output: 900, ..Turn::default() });
    let s = sample(&w, 300, None).unwrap();
    assert_eq!((s.ctx, s.rewrite, s.rewrite_1h, s.regen), (101_003, 101_003 - 1_002, 0, 700));
    assert_eq!(Sample::parse(0, "ctx=1 rw=2"), None);
}

#[test]
fn switch_cost_prices_the_context_and_falls_back_to_the_old_charge() {
    let near = |x: f64, y: f64| assert!((x - y).abs() < 1e-15, "{x} vs {y}");
    // unmeasured: exactly what a switch was charged before, by context
    for class in [Class::Plain, Class::Restricted] {
        near(weeks(None, class, 460_000), switch_cost(class, None));
        near(weeks(None, class, 920_000), 2.0 * switch_cost(class, None));
    }
    // measured: the whole context written once to the one-hour cache
    // instead of read, and a hundredth of it said again
    let sh = Shares { rewrite: 1.0, one_hour: 1.0, regen: 0.01 };
    near(weeks(Some(sh), Class::Plain, 1_000_000), (8.0 - 0.2 + 0.01 * 20.0) / 30.5 / 100.0);
    near(weeks(Some(sh), Class::Restricted, 1_000_000), (16.0 - 0.2 + 0.01 * 40.0) / 11.2 / 100.0);
    // a long session costs more to move, in proportion
    near(weeks(Some(sh), Class::Plain, 800_000), 4.0 * weeks(Some(sh), Class::Plain, 200_000));
    let five = Shares { one_hour: 0.0, regen: 0.0, ..sh };
    near(weeks(Some(five), Class::Plain, 1_000_000), (5.0 - 0.2) / 30.5 / 100.0);
}
