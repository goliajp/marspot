//! The planner in a closed loop against today's rule, on the same draws:
//! twenty simulated weeks in two halves (each test stays well inside
//! the runner's minute), and the forecast wrong by three either way.
//! The numbers are printed for the record; the bar asserted is that no
//! pane is switched twice by plan inside six hours.

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
    let money_ok = today.iter().zip(&planner).filter(|(t, p)| p.money <= t.money + 1e-9).count();
    let refusals_ok = today.iter().zip(&planner).filter(|(t, p)| p.refusals <= t.refusals).count();
    println!(
        "closed loop {what}, {} weeks: money today {:.2} planner {:.2} points; unmet today {:.2} planner {:.2}; refusals today {} planner {}; switches today {} planner {}; weeks with the planner's money no higher {money_ok}, refusals no higher {refusals_ok}",
        today.len(),
        100.0 * sum(&today, |w| w.money),
        100.0 * sum(&planner, |w| w.money),
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
fn sched_level2_closed_loop_against_today_weeks_1_to_10() {
    compare("weeks 1-10", 1000..1010, 1.0);
}

#[test]
fn sched_level2_closed_loop_against_today_weeks_11_to_20() {
    compare("weeks 11-20", 1010..1020, 1.0);
}

#[test]
fn sched_level2_closed_loop_with_the_forecast_three_times_high() {
    compare("forecast × 3", 1000..1008, 3.0);
}

#[test]
fn sched_level2_closed_loop_with_the_forecast_three_times_low() {
    compare("forecast ÷ 3", 1000..1008, 1.0 / 3.0);
}
