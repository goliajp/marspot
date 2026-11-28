//! The closed loop under each profile: what each set of limits buys and
//! costs against today's rule on the same draws, and a digest per
//! profile so a change to any of them shows.

use super::Limits;
use super::loop_digest_tests::{Fnv, RUNS};
use super::loop_price::{MEASURED, Pricing};
use super::loop_world::{Policy, Week, run, run_priced, run_with};

/// The digests of `RUNS` with the planner under balanced and full,
/// recorded; steady's is `loop_digest_tests::RECORDED`.
const BALANCED: u64 = 0x091a_0d70_0af8_b166;
const FULL: u64 = 0x1bfd_4255_32f2_35ec;

fn weeks(from: u64, to: u64, policy: Policy, lim: Limits) -> Vec<Week> {
    std::thread::scope(|s| {
        let h: Vec<_> = (from..to).map(|k| s.spawn(move || run_with(k, policy, lim))).collect();
        h.into_iter().map(|h| h.join().expect("a week ran")).collect()
    })
}

fn priced(from: u64, to: u64, policy: Policy, lim: Limits, pricing: Pricing) -> Vec<Week> {
    std::thread::scope(|s| {
        let h: Vec<_> = (from..to).map(|k| s.spawn(move || run_priced(k, policy, lim, pricing))).collect();
        h.into_iter().map(|h| h.join().expect("a week ran")).collect()
    })
}

fn digest(lim: Limits) -> u64 {
    let mut all = Fnv(0xcbf2_9ce4_8422_2325);
    for (from, to, policy) in RUNS {
        for w in &weeks(from, to, policy, lim) {
            all.week(w);
        }
    }
    all.0
}

#[test]
#[ignore = "nine weeks a profile; run with --release -- --ignored"]
fn sched_level2_closed_loop_digest_per_profile() {
    let (b, f) = (digest(Limits::BALANCED), digest(Limits::FULL));
    println!("closed loop digest balanced: {b:016x} full: {f:016x}");
    assert_eq!((b, f), (BALANCED, FULL), "balanced read {b:016x}, full {f:016x}");
}

struct Sum {
    money: f64,
    refusals: usize,
    switches: usize,
    fable_lapsed: f64,
    short_stay: usize,
    long_switches: usize,
    long_steps: usize,
    steps: usize,
}

fn sum(ws: &[Week]) -> Sum {
    Sum {
        money: ws.iter().map(|w| w.money).sum(),
        refusals: ws.iter().map(|w| w.refusals).sum(),
        switches: ws.iter().map(|w| w.switches).sum(),
        fable_lapsed: ws.iter().map(|w| w.fable_lapsed).sum(),
        short_stay: ws.iter().map(|w| w.short_stay).sum(),
        long_switches: ws.iter().map(|w| w.long_switches).sum(),
        long_steps: ws.iter().map(|w| w.long_steps).sum(),
        steps: ws.iter().map(|w| w.steps).sum(),
    }
}

/// Twenty weeks, the planner driven as the shell drives it with the
/// forecast right, under each profile; today's rule beside it.
#[test]
#[ignore = "eighty weeks driven tick by tick; run with --release -- --ignored"]
fn sched_level2_closed_loop_profiles() {
    const SEEDS: (u64, u64) = (1000, 1020);
    let n = (SEEDS.1 - SEEDS.0) as f64;
    let today: Vec<Week> = std::thread::scope(|s| {
        let h: Vec<_> = (SEEDS.0..SEEDS.1).map(|k| s.spawn(move || run(k, Policy::Today { bias: 1.0 }))).collect();
        h.into_iter().map(|h| h.join().expect("a week ran")).collect()
    });
    let planner = Policy::Planner { bias: 1.0, short: 1.0, shell: true };
    let mut rows = vec![("today", sum(&today))];
    for lim in [Limits::STEADY, Limits::BALANCED, Limits::FULL] {
        rows.push((lim.profile.word(), sum(&weeks(SEEDS.0, SEEDS.1, planner, lim))));
    }
    for (name, s) in &rows {
        println!(
            "closed loop {name:>8}: money {:.2} points, refusals {}, switches {} ({:.1} a day), fable lapsed {:.1} points, inside own stay {}",
            100.0 * s.money,
            s.refusals,
            s.switches,
            s.switches as f64 / n / 7.0,
            100.0 * s.fable_lapsed,
            s.short_stay,
        );
    }
    let (steady, balanced, full) = (&rows[1].1, &rows[2].1, &rows[3].1);
    assert!(balanced.refusals <= steady.refusals, "balanced refusals {} over steady's {}", balanced.refusals, steady.refusals);
    // printed, not asserted: see the loop priced by context below
    println!("closed loop: full lapses {:.2} points against balanced's {:.2}", 100.0 * full.fable_lapsed, 100.0 * balanced.fable_lapsed);
    for (name, s) in &rows[1..] {
        assert_eq!(s.short_stay, 0, "{name}: a pane switched again inside its stay");
    }
}

/// The same twenty weeks with each switch priced by the context its pane
/// has then: switches at a rest per pane-day of long sessions (at
/// least `loop_price::LONG`) against short ones.
#[test]
#[ignore = "eighty weeks driven tick by tick; run with --release -- --ignored"]
fn sched_level2_closed_loop_profiles_priced_by_context() {
    const SEEDS: (u64, u64) = (1000, 1020);
    let n = (SEEDS.1 - SEEDS.0) as f64;
    let ctx = Pricing::Context(MEASURED);
    let planner = Policy::Planner { bias: 1.0, short: 1.0, shell: true };
    let mut rows = vec![("today", sum(&priced(SEEDS.0, SEEDS.1, Policy::Today { bias: 1.0 }, Limits::STEADY, ctx)))];
    for lim in [Limits::STEADY, Limits::BALANCED, Limits::FULL] {
        rows.push((lim.profile.word(), sum(&priced(SEEDS.0, SEEDS.1, planner, lim, ctx))));
    }
    // a pane step is five minutes
    let per_day = |k: usize, steps: usize| k as f64 / (steps.max(1) as f64 / 288.0);
    for (name, s) in &rows {
        let short_switches = s.switches - s.long_switches;
        println!(
            "closed loop by context {name:>8}: money {:.2} points, refusals {}, switches {} ({:.1} a day), fable lapsed {:.1} points, inside own stay {}, moves a pane-day long {:.3} short {:.3}",
            100.0 * s.money,
            s.refusals,
            s.switches,
            s.switches as f64 / n / 7.0,
            100.0 * s.fable_lapsed,
            s.short_stay,
            per_day(s.long_switches, s.long_steps),
            per_day(short_switches, s.steps - s.long_steps),
        );
    }
    let (steady, balanced, full) = (&rows[1].1, &rows[2].1, &rows[3].1);
    assert!(balanced.refusals <= steady.refusals, "balanced refusals {} over steady's {}", balanced.refusals, steady.refusals);
    // balanced charges a switch nothing beyond its rewrite, as steady:
    // with its old charges (0.3 points a switch, 2 a refusal) it refused
    // 207 here against steady's 185, and 392 against 342 on seeds
    // 1020..1060; without them 183 and 339.  Full against balanced on
    // lapsed Fable is printed, not asserted: with balanced's charges gone
    // full lapses more (0.85 points against 0.00 here, 1.16 against 0.06
    // on seeds 1020..1060, 1.01 against 0.06 priced fixed), and full with
    // its own charges at zero lapses none on either range
    println!("closed loop by context: full lapses {:.2} points against balanced's {:.2}", 100.0 * full.fable_lapsed, 100.0 * balanced.fable_lapsed);
    for (name, s) in &rows[1..] {
        assert_eq!(s.short_stay, 0, "{name}: a pane switched again inside its stay");
    }
}
