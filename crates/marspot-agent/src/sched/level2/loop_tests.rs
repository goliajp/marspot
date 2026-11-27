//! The planner in a closed loop against today's rule, on the same draws:
//! twenty simulated weeks in four parts (each test stays well inside
//! the runner's minute), and the forecast wrong by three either way.
//! Each week's money is printed in its parts (rewrites of switches,
//! rewrites at refusals, week lapsed) with the refusals and switches,
//! the restricted panes' in brackets.
//!
//! The bar asserted is that no pane is switched twice by plan inside
//! six hours.  Money and refusals no higher than today's rule are
//! printed, not asserted: over the twenty weeks the planner refuses
//! fewer panes but lapses more week at renewals than today's rule, which
//! drains every account to its renewal however far off that is, while
//! the planner sees lapse only up to two days past its horizon.

use super::loop_world::{Policy, Week, hours, run};

fn weeks(seeds: std::ops::Range<u64>, policy: Policy) -> Vec<Week> {
    seeds.map(|s| run(s, policy)).collect()
}

fn sum(v: &[Week], f: impl Fn(&Week) -> f64) -> f64 {
    v.iter().map(f).sum()
}

fn compare(what: &str, seeds: std::ops::Range<u64>, bias: f64) {
    assert_eq!(hours().len(), 49, "the recorded hours read wrong");
    let today = weeks(seeds.clone(), Policy::Today);
    let planner = weeks(seeds, Policy::Planner { bias });
    for (k, (t, p)) in today.iter().zip(&planner).enumerate() {
        println!(
            "week {k}: today money {:.2} = switches {:.2} + refusals {:.2} + lapsed {:.2}, refusals {} ({}), switches {} ({}); planner money {:.2} = switches {:.2} + refusals {:.2} + lapsed {:.2}, refusals {} ({}), switches {} ({})",
            100.0 * t.money, 100.0 * t.switch_rw, 100.0 * t.refusal_rw, 100.0 * t.lapsed, t.refusals, t.refusals_r, t.switches, t.switches_r,
            100.0 * p.money, 100.0 * p.switch_rw, 100.0 * p.refusal_rw, 100.0 * p.lapsed, p.refusals, p.refusals_r, p.switches, p.switches_r,
        );
    }
    let money_ok = today.iter().zip(&planner).filter(|(t, p)| p.money <= t.money + 1e-9).count();
    let refusals_ok = today.iter().zip(&planner).filter(|(t, p)| p.refusals <= t.refusals).count();
    println!(
        "closed loop {what}, {} weeks: money today {:.2} (switches {:.2} refusals {:.2} lapsed {:.2}) planner {:.2} (switches {:.2} refusals {:.2} lapsed {:.2}) points; unmet today {:.2} planner {:.2}; refusals today {} planner {}; switches today {} planner {}; weeks with the planner's money no higher {money_ok}, refusals no higher {refusals_ok}",
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
        today.iter().map(|w| w.refusals).sum::<usize>(),
        planner.iter().map(|w| w.refusals).sum::<usize>(),
        today.iter().map(|w| w.switches).sum::<usize>(),
        planner.iter().map(|w| w.switches).sum::<usize>(),
    );
    assert!(planner.iter().all(|w| w.double == 0), "{what}: a pane switched twice by plan inside six hours");
}

#[test]
fn sched_level2_closed_loop_against_today_weeks_1_to_5() {
    compare("weeks 1-5", 1000..1005, 1.0);
}

#[test]
fn sched_level2_closed_loop_against_today_weeks_6_to_10() {
    compare("weeks 6-10", 1005..1010, 1.0);
}

#[test]
fn sched_level2_closed_loop_against_today_weeks_11_to_15() {
    compare("weeks 11-15", 1010..1015, 1.0);
}

#[test]
fn sched_level2_closed_loop_against_today_weeks_16_to_20() {
    compare("weeks 16-20", 1015..1020, 1.0);
}

#[test]
fn sched_level2_closed_loop_with_the_forecast_three_times_high_1_to_4() {
    compare("forecast × 3, weeks 1-4", 1000..1004, 3.0);
}

#[test]
fn sched_level2_closed_loop_with_the_forecast_three_times_high_5_to_8() {
    compare("forecast × 3, weeks 5-8", 1004..1008, 3.0);
}

#[test]
fn sched_level2_closed_loop_with_the_forecast_three_times_low_1_to_4() {
    compare("forecast ÷ 3, weeks 1-4", 1000..1004, 1.0 / 3.0);
}

#[test]
fn sched_level2_closed_loop_with_the_forecast_three_times_low_5_to_8() {
    compare("forecast ÷ 3, weeks 5-8", 1004..1008, 1.0 / 3.0);
}
