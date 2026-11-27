use super::*;

fn q(id: u8, r: f64, t: f64) -> Quant {
    Quant { id, r, f: r, t, tf: t, open: true, stale: false }
}

/// Six hours of readings every five minutes: two accounts asked for
/// `asked` weeks an hour each and losing `used` an hour each.
fn window(c: &mut Calib, from: i64, asked: f64, used: f64) {
    for k in 0..=72 {
        let h = k as f64 / 12.0;
        let at = from + k * 300;
        let a = [q(1, 0.8 - used * h, 100.0 - h), q(2, 0.6 - used * h, 50.0 - h)];
        c.observe(at, &a, &[asked, asked]);
    }
}

#[test]
fn sched_calib_speeds_read_three_times_high_are_scaled_down() {
    let mut c = Calib::default();
    window(&mut c, 0, 0.03, 0.01);
    assert!((c.factor() - 1.0 / 3.0).abs() < 1e-6, "{}", c.factor());
}

#[test]
fn sched_calib_ordinary_error_leaves_the_speeds_alone() {
    let mut c = Calib::default();
    window(&mut c, 0, 0.012, 0.01);
    assert_eq!(c.factor(), 1.0);
    assert!(c.ratio.is_some_and(|r| (r - 1.0 / 1.2).abs() < 1e-6));
}

#[test]
fn sched_calib_too_little_use_keeps_the_factor_in_force() {
    let mut c = Calib::default();
    window(&mut c, 0, 0.03, 0.01);
    window(&mut c, 6 * 3600, 0.003, 0.003);
    assert!((c.factor() - 1.0 / 3.0).abs() < 1e-6, "{}", c.factor());
}

#[test]
fn sched_calib_an_account_that_renews_or_runs_dry_is_left_out() {
    let mut c = Calib::default();
    c.observe(0, &[q(1, 0.9, 1.0), q(2, 0.5, 50.0), q(3, 0.05, 30.0)], &[0.03, 0.03, 0.03]);
    // account 1 renewed, account 3 ran dry: only account 2 counts,
    // asked 0.18 over six hours and losing 0.06
    c.observe(6 * 3600, &[q(1, 1.0, 163.0), q(2, 0.44, 44.0), q(3, 0.0, 24.0)], &[0.03, 0.03, 0.03]);
    assert!(c.ratio.is_some_and(|r| (r - 1.0 / 3.0).abs() < 1e-6), "{:?}", c.ratio);
}
