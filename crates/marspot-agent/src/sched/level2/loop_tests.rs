//! The planner in a closed loop against today's rule, on the same draws,
//! each week's money printed in its parts (rewrites of switches,
//! rewrites at refusals, week lapsed) with the refusals and switches,
//! the restricted panes' in brackets.
//!
//! The bars held:
//! - with the forecast right, over all the weeks of a run together the
//!   planner refuses no more panes and spends no more than today's rule
//!   (a count of weeks lost is not held: the planner refuses more than
//!   today in about three weeks in ten on 638 weeks, so over twenty it
//!   moves by several weeks with the draw), and no single week refuses
//!   more panes than today's rule by more than two or a tenth, whichever
//!   is more;
//! - with the forecast three times low, the planner refuses at most 5%
//!   more panes than today's rule; three times high, it spends at most
//!   5% more;
//! - no pane is switched twice by plan inside six hours.
//!
//! Twenty weeks on the seeds the planner was tuned on and twenty it
//! never was are held to the same bars.  With the forecast wrong, the
//! planner is held against today's rule reading the speeds it reads:
//! wrong by the same factor, or true where the planner's three-hour
//! speed is true.  The weeks of a run go on threads of their own.

use super::loop_world::{Policy, Week, hours, run};

fn sum(v: &[Week], f: impl Fn(&Week) -> f64) -> f64 {
    v.iter().map(f).sum()
}

fn refusals(v: &[Week]) -> usize {
    v.iter().map(|w| w.refusals).sum()
}

/// Today's rule and the planner on each seed, one thread per week and policy.
fn both(seeds: std::ops::Range<u64>, today: Policy, planner: Policy) -> (Vec<Week>, Vec<Week>) {
    std::thread::scope(|s| {
        let t: Vec<_> = seeds.clone().map(|k| s.spawn(move || run(k, today))).collect();
        let p: Vec<_> = seeds.map(|k| s.spawn(move || run(k, planner))).collect();
        let join = |v: Vec<std::thread::ScopedJoinHandle<'_, Week>>| v.into_iter().map(|h| h.join().expect("a week ran")).collect::<Vec<_>>();
        (join(t), join(p))
    })
}

/// The run's weeks, printed with their totals; asserts no double switch.
fn closed(what: &str, seeds: std::ops::Range<u64>, bias: f64, short: f64, shell: bool) -> (Vec<Week>, Vec<Week>) {
    assert_eq!(hours().len(), 49, "the recorded hours read wrong");
    // today's rule reads the speeds the planner's three-hour speed has
    let today_bias = if short == 1.0 { 1.0 } else { bias };
    let (today, planner) = both(seeds.clone(), Policy::Today { bias: today_bias }, Policy::Planner { bias, short, shell });
    for (seed, (t, p)) in seeds.zip(today.iter().zip(&planner)) {
        println!(
            "{what} seed {seed}: today money {:.2} = switches {:.2} + refusals {:.2} + lapsed {:.2}, refusals {} ({}), switches {} ({}); planner money {:.2} = switches {:.2} + refusals {:.2} + lapsed {:.2}, refusals {} ({}), switches {} ({})",
            100.0 * t.money, 100.0 * t.switch_rw, 100.0 * t.refusal_rw, 100.0 * t.lapsed, t.refusals, t.refusals_r, t.switches, t.switches_r,
            100.0 * p.money, 100.0 * p.switch_rw, 100.0 * p.refusal_rw, 100.0 * p.lapsed, p.refusals, p.refusals_r, p.switches, p.switches_r,
        );
    }
    let money_ok = today.iter().zip(&planner).filter(|(t, p)| p.money <= t.money + 1e-9).count();
    let refusals_ok = today.iter().zip(&planner).filter(|(t, p)| p.refusals <= t.refusals).count();
    let worst = today.iter().zip(&planner).map(|(t, p)| p.refusals as i64 - t.refusals as i64).max().unwrap_or(0);
    println!(
        "closed loop {what}, {} weeks, today's rule at forecast × {today_bias:.3}: money today {:.2} (switches {:.2} refusals {:.2} lapsed {:.2}) planner {:.2} (switches {:.2} refusals {:.2} lapsed {:.2}) points; unmet today {:.2} planner {:.2}; refusals today {} planner {}; switches today {} planner {}; weeks with the planner's money no higher {money_ok}, refusals no higher {refusals_ok}; most refusals over today's in a week {worst}",
        today.len(),
        100.0 * sum(&today, |w| w.money),
        100.0 * sum(&today, |w| w.switch_rw),
        100.0 * sum(&today, |w| w.refusal_rw),
        100.0 * sum(&today, |w| w.lapsed),
        100.0 * sum(&planner, |w| w.money),
        100.0 * sum(&planner, |w| w.switch_rw),
        100.0 * sum(&planner, |w| w.refusal_rw),
        100.0 * sum(&planner, |w| w.lapsed),
        100.0 * sum(&today, |w| w.unmet),
        100.0 * sum(&planner, |w| w.unmet),
        refusals(&today),
        refusals(&planner),
        today.iter().map(|w| w.switches).sum::<usize>(),
        planner.iter().map(|w| w.switches).sum::<usize>(),
    );
    assert!(planner.iter().all(|w| w.double == 0), "{what}: a pane switched twice by plan inside six hours");
    (today, planner)
}

/// The bar with the forecast right (see the module's text).
fn no_worse(what: &str, today: &[Week], planner: &[Week]) {
    let (t, p) = (sum(today, |w| w.money), sum(planner, |w| w.money));
    assert!(p <= t + 1e-9, "{what}: the planner spent {:.2} points against today's {:.2}", 100.0 * p, 100.0 * t);
    assert!(refusals(planner) <= refusals(today), "{what}: the planner refused {} panes against today's {}", refusals(planner), refusals(today));
    let over: Vec<String> = today
        .iter()
        .zip(planner)
        .enumerate()
        .filter(|(_, (t, p))| p.refusals as f64 > t.refusals as f64 + (0.1 * t.refusals as f64).max(2.0))
        .map(|(k, (t, p))| format!("week {} {} against {}", k + 1, p.refusals, t.refusals))
        .collect();
    println!("closed loop {what}: weeks over today's refusals by more than max(2, 10%): {} {over:?}", over.len());
    assert!(over.is_empty(), "{what}: weeks over today's refusals by more than max(2, 10%): {over:?}");
}

/// The forecast three times low: at most 5% more refusals than today.
fn low_held(what: &str, today: &[Week], planner: &[Week]) {
    let (t, p) = (refusals(today), refusals(planner));
    assert!(p as f64 <= 1.05 * t as f64, "{what}: the planner refused {p} panes against today's {t}, over 5% more");
}

/// The forecast three times high: at most 5% more money than today.
fn high_held(what: &str, today: &[Week], planner: &[Week]) {
    let (t, p) = (sum(today, |w| w.money), sum(planner, |w| w.money));
    assert!(p <= 1.05 * t + 1e-9, "{what}: the planner spent {:.2} points against today's {:.2}, over 5% more", 100.0 * p, 100.0 * t);
}

fn right(what: &str, seeds: std::ops::Range<u64>, shell: bool) {
    let (t, p) = closed(what, seeds, 1.0, 1.0, shell);
    no_worse(what, &t, &p);
}

fn wrong(what: &str, seeds: std::ops::Range<u64>, bias: f64, short: f64, shell: bool) {
    let (t, p) = closed(what, seeds, bias, short, shell);
    if bias < 1.0 { low_held(what, &t, &p) } else { high_held(what, &t, &p) }
}

#[test]
fn sched_level2_closed_loop_against_today_twenty_weeks() {
    right("weeks 1-20", 1000..1020, false);
}

#[test]
fn sched_level2_closed_loop_hold_out_twenty_weeks() {
    right("hold-out weeks 1-20", 7000..7020, false);
}

#[test]
fn sched_level2_closed_loop_with_the_forecast_three_times_high() {
    wrong("forecast × 3, weeks 1-8", 1000..1008, 3.0, 3.0, false);
}

#[test]
fn sched_level2_closed_loop_with_the_forecast_three_times_low() {
    wrong("forecast ÷ 3, weeks 1-8", 1000..1008, 1.0 / 3.0, 1.0 / 3.0, false);
}

#[test]
#[ignore = "eight weeks driven tick by tick as the shell: minutes in a debug build; run with --release -- --ignored"]
fn sched_level2_closed_loop_driven_as_the_shell() {
    right("as the shell drives it, weeks 1-8", 1000..1008, true);
}

#[test]
#[ignore = "eight weeks driven tick by tick as the shell: minutes in a debug build; run with --release -- --ignored"]
fn sched_level2_closed_loop_driven_as_the_shell_forecast_three_times_high() {
    wrong("as the shell drives it, forecast × 3, weeks 1-8, three-hour speed true", 1000..1008, 3.0, 1.0, true);
}

#[test]
#[ignore = "eight weeks driven tick by tick as the shell: minutes in a debug build; run with --release -- --ignored"]
fn sched_level2_closed_loop_driven_as_the_shell_forecast_three_times_high_three_hour_speed_as_wrong() {
    wrong("as the shell drives it, forecast × 3, weeks 1-8, three-hour speed as wrong", 1000..1008, 3.0, 3.0, true);
}

#[test]
#[ignore = "eight weeks driven tick by tick as the shell: minutes in a debug build; run with --release -- --ignored"]
fn sched_level2_closed_loop_driven_as_the_shell_forecast_three_times_low() {
    wrong("as the shell drives it, forecast ÷ 3, weeks 1-8, three-hour speed true", 1000..1008, 1.0 / 3.0, 1.0, true);
}

#[test]
#[ignore = "eight weeks driven tick by tick as the shell: minutes in a debug build; run with --release -- --ignored"]
fn sched_level2_closed_loop_driven_as_the_shell_forecast_three_times_low_three_hour_speed_as_wrong() {
    wrong("as the shell drives it, forecast ÷ 3, weeks 1-8, three-hour speed as wrong", 1000..1008, 1.0 / 3.0, 1.0 / 3.0, true);
}
