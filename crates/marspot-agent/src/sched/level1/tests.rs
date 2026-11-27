use super::*;

pub(super) const KAPPA: f64 = 0.528;

pub(super) fn q(id: u8, r: f64, f: f64, t: f64) -> Quant {
    Quant { id, r, f, t, tf: t, open: true, stale: false }
}

pub(super) fn world<'a>(accounts: &'a [Quant], demand: [f64; 2], prudent: f64) -> World<'a> {
    World { accounts, window_full: KAPPA, demand, prudent, buckets: &[], offers: &[], closures: &[], last_pi: None }
}

/// splitmix64.
pub(super) struct Rng(pub u64);

impl Rng {
    pub(super) fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A whole number of `step`-hour steps in `1..=n`.
    pub(super) fn steps(&mut self, step: f64, n: usize) -> f64 {
        step * (1 + (self.next() * n as f64) as usize).min(n) as f64
    }
}

/// A world of `n` accounts with renewals on `step`-hour boundaries.
pub(super) fn random_accounts(rng: &mut Rng, n: usize, step: f64) -> Vec<Quant> {
    (0..n)
        .map(|k| {
            let r = if rng.next() < 0.2 { 0.0 } else { rng.next() };
            let f = if rng.next() < 0.2 { 0.0 } else { KAPPA * rng.next() };
            q(k as u8 + 1, r, f, rng.steps(step, (H1 / step) as usize))
        })
        .collect()
}

#[test]
fn sched_level1_short_restricted_demand_prices_the_reserve_at_one() {
    // half a point of restricted quota against a week of a point an hour
    let a = [q(1, 0.5, 0.005, 100.0)];
    let p = plan(&world(&a, [0.01, 0.0], 0.01));
    assert_eq!(p.pi, 1.0);
    assert!(p.unserved_until(0, f64::INFINITY) > 0.5);
}

#[test]
fn sched_level1_pi_is_a_half_until_eight_buckets_then_their_share() {
    let a = [q(1, 1.0, KAPPA, 100.0), q(2, 1.0, KAPPA, 150.0)];
    let mut w = world(&a, [0.002, 0.0], 0.002);
    assert_eq!(plan(&w).pi, 0.5, "no buckets yet");
    // supply over the week: 1.056 now plus a renewed window at 100 h and
    // at 150 h, so about 0.01 an hour goes short; two of eight buckets do
    let buckets = [0.001, 0.002, 0.003, 0.002, 0.03, 0.04, 0.001, 0.002];
    w.buckets = &buckets;
    assert_eq!(plan(&w).pi, 0.25);
    // within the hysteresis the last price stays
    w.last_pi = Some(0.3);
    assert_eq!(plan(&w).pi, 0.3);
    w.last_pi = Some(0.6);
    assert_eq!(plan(&w).pi, 0.25);
}

#[test]
fn sched_level1_reserve_and_spare_follow_the_prudent_rate() {
    // P1 renews first: the prudent restricted rate empties its Phi first
    let a = [q(1, 0.4, 0.3, 20.0), q(2, 1.0, KAPPA, 160.0)];
    let p = plan(&world(&a, [0.005, 0.01], 0.02));
    // 0.02 × 20 h = 0.4 wanted before P1 renews; P1's Phi is 0.3
    assert!((p.reserve[0] - 0.3).abs() < 1e-9, "{:?}", p.reserve);
    assert!((p.spare[0] - 0.1).abs() < 1e-9, "{:?}", p.spare);
    // then P2's Phi, which expires before P1's renewed window: all of it
    assert!((p.reserve[1] - KAPPA).abs() < 1e-9, "{:?}", p.reserve);
    assert!((p.spare[1] - (1.0 - KAPPA)).abs() < 1e-9, "{:?}", p.spare);
}

#[test]
fn sched_level1_a_point_where_demand_waits_is_worth_a_point_and_where_none_waits_nothing() {
    // P1 has nothing; everything waits for P2's renewal at 100 h
    let a = [q(1, 0.0, 0.0, 120.0), q(2, 0.0, 0.0, 100.0)];
    let p = plan(&world(&a, [0.0, 0.01], 0.0));
    let price = p.price[0].expect("open");
    assert!((price[0] - 1.0).abs() < 1e-6 && (price[1] - 1.0).abs() < 1e-6, "{price:?}");
    // plenty: a point more serves nothing
    let a = [q(1, 1.0, KAPPA, 120.0)];
    let p = plan(&world(&a, [0.0, 0.001], 0.0));
    assert_eq!(p.price[0], Some([0.0, 0.0]));
    // a closed account has no price
    let mut closed = a;
    closed[0].open = false;
    assert_eq!(plan(&world(&closed, [0.0, 0.001], 0.0)).price[0], None);
}

#[test]
fn sched_level1_a_rate_window_closes_an_account_for_its_interval() {
    let a = [q(1, 1.0, KAPPA, 150.0)];
    let open = plan(&world(&a, [0.0, 0.005], 0.0));
    let closures = [Closure { account: 1, from_h: 0.0, to_h: 3.0 }];
    let w = World { closures: &closures, ..world(&a, [0.0, 0.005], 0.0) };
    let shut = plan(&w);
    assert!((open.served[1] - shut.served[1] - 0.015).abs() < 1e-9, "{} {}", open.served[1], shut.served[1]);
    assert!((shut.unserved_until(1, 3.0) - 0.015).abs() < 1e-9);
}

#[test]
fn sched_level1_the_bound_prices_reserve_taken_by_pi() {
    // the plain class takes the restricted reserve only where it waits
    // otherwise; the bound counts it at pi
    let a = [q(1, 0.2, 0.2, 50.0)];
    let p = plan(&world(&a, [0.001, 0.01], 0.004));
    assert!(p.money.reserve_taken > 0.0, "{:?}", p.money);
    assert!((p.money.total - (p.money.lapsed + p.pi * p.money.reserve_taken + p.money.offers_lost)).abs() < 1e-12);
}

#[test]
fn sched_level1_line_has_every_part_in_points() {
    let a = [q(3, 0.07, 0.07, 58.0), q(7, 0.5, 0.0, 105.0)];
    let p = plan(&world(&a, [0.002, 0.03], 0.0339));
    let l = p.line(&[3, 7]);
    for part in ["served=", " unserved_h2=", " unserved=", " pi=", " bound=", " lapsed=", " reserve_taken=", " offers_lost=", " combos=1", " claims=-", " P3=7.00/", " P7=0.00/50.00/"] {
        assert!(l.contains(part), "{part} missing: {l}");
    }
}

/// The cost of one plan at 16 accounts with four resets, reported; it
/// runs only when an input changes.
#[test]
fn sched_level1_plan_cost_at_sixteen_accounts_is_reported() {
    let mut rng = Rng(5);
    let a = random_accounts(&mut rng, 16, 1.0);
    let reset = Supply { week: false, window: true, gives_back_week: true, moves_week: false, requires_limit: true, starts_h: 0.0, ends_h: None, left: 1, usable_below_left: None };
    let offers: Vec<Offer> = a.iter().take(4).map(|q| Offer { account: q.id, supply: reset }).collect();
    let buckets = [0.01, 0.02, 0.03, 0.04, 0.05, 0.01, 0.02, 0.03];
    let w = World { offers: &offers, buckets: &buckets, ..world(&a, [0.03, 0.06], 0.05) };
    let t = std::time::Instant::now();
    let p = plan(&w);
    let took = t.elapsed();
    eprintln!("one plan: {took:?}, {} segments, {} combinations", p.cuts.len() - 1, p.combos);
    assert!(p.cuts.len() - 1 <= 80 && p.combos <= 16);
}
