use super::*;

fn q(id: u8, r: f64, t: f64) -> Quant {
    Quant { id, r, f: r, t, tf: t, open: true, stale: false }
}

/// Six hours of readings every five minutes from `from`: two accounts
/// asked for `asked` weeks an hour each and losing `used` an hour each.
fn window(c: &mut Calib, from: i64, asked: f64, used: f64) {
    readings(c, from, &mut 0.0, asked, used, |_, _| {});
}

/// The same after `lost` weeks lost before `from` by each account, which
/// it adds this window's loss to, so that windows in a row carry on.
fn window_on(c: &mut Calib, from: i64, lost: &mut f64, asked: f64, used: f64) {
    readings(c, from, lost, asked, used, |_, _| {});
}

/// The same, with `edit` changing each reading `k` before it is seen.
fn readings(c: &mut Calib, from: i64, lost: &mut f64, asked: f64, used: f64, mut edit: impl FnMut(i64, &mut [Quant; 2])) {
    let before = *lost;
    for k in 0..=72 {
        let at = from + k * 300;
        let h = at as f64 / 3600.0;
        *lost = before + used * k as f64 / 12.0;
        let mut a = [q(1, 0.8 - *lost, 100.0 - h), q(2, 0.6 - *lost, 50.0 - h)];
        edit(k, &mut a);
        c.observe(at, &a, &[asked, asked], false);
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
    let mut lost = 0.0;
    window_on(&mut c, 0, &mut lost, 0.03, 0.01);
    window_on(&mut c, 6 * 3600, &mut lost, 0.003, 0.003);
    assert!((c.factor() - 1.0 / 3.0).abs() < 1e-6, "{}", c.factor());
}

#[test]
fn sched_calib_an_account_that_renews_or_runs_dry_is_left_out() {
    let mut c = Calib::default();
    // account 1 renews at the end, account 3 runs dry: only account 2
    // counts, asked 0.18 over six hours and losing 0.06
    for k in 0..=72 {
        let h = k as f64 / 12.0;
        let one = if k < 72 { q(1, 0.9, 7.0 - h) } else { q(1, 1.0, 168.0) };
        let a = [one, q(2, 0.5 - 0.01 * h, 50.0 - h), q(3, (0.05 - 0.01 * h).max(0.0), 30.0 - h)];
        c.observe(k * 300, &a, &[0.03, 0.03, 0.03], false);
    }
    assert!(c.ratio.is_some_and(|r| (r - 1.0 / 3.0).abs() < 1e-6), "{:?}", c.ratio);
}

#[test]
fn sched_calib_a_closed_account_is_left_out() {
    let mut c = Calib::default();
    // account 2 is closed for one reading and loses nothing asked of it
    readings(&mut c, 0, &mut 0.0, 0.03, 0.01, |k, a| {
        a[1].r = 0.6;
        a[1].open = k != 30;
    });
    assert_eq!(c.closed.accounts.len(), 1, "{:?}", c.closed);
    assert!(c.ratio.is_some_and(|r| (r - 1.0 / 3.0).abs() < 1e-6), "{:?}", c.ratio);
}

#[test]
fn sched_calib_a_reading_error_upward_counts_as_negative_use() {
    let mut c = Calib::default();
    // account 2 reads a point more at the end than at the start, short of
    // a renewal: its loss is -0.01, summed with the others
    readings(&mut c, 0, &mut 0.0, 0.02, 0.02, |k, a| a[1].r = if k == 72 { 0.61 } else { 0.6 });
    let (used, want) = (c.closed.used, c.closed.want);
    assert!((used - (0.12 - 0.01)).abs() < 1e-9 && (want - 0.24).abs() < 1e-9, "{used} {want}");
}

#[test]
fn sched_calib_a_change_of_accounts_restarts_the_window() {
    let mut c = Calib::default();
    // three hours in, a third account appears: the window starts again
    // there and has not closed by the end of these readings
    let mut three = false;
    readings(&mut c, 0, &mut 0.0, 0.03, 0.01, |k, a| {
        if k >= 36 {
            a[1].id = 3;
            three = true;
        }
    });
    assert!(three && c.ratio.is_none() && c.factor() == 1.0, "{:?}", c.ratio);
}

#[test]
fn sched_calib_a_stale_account_is_left_out_and_a_stale_feed_drops_the_window() {
    let mut c = Calib::default();
    readings(&mut c, 0, &mut 0.0, 0.03, 0.01, |k, a| a[0].stale = k == 40);
    assert_eq!(c.closed.accounts.iter().map(|x| x.0).collect::<Vec<_>>(), vec![2]);
    let mut c = Calib::default();
    let a = [q(1, 0.8, 100.0), q(2, 0.6, 50.0)];
    c.observe(0, &a, &[0.03, 0.03], false);
    c.observe(300, &a, &[0.03, 0.03], true);
    // the window starts again at the next fresh reading and closes six
    // hours after it, not six hours after the first
    let closes: Vec<i64> = (2..=80).map(|k| k * 300).filter(|&t| c.observe(t, &a, &[0.03, 0.03], false)).collect();
    assert_eq!(closes, vec![600 + 6 * 3600]);
}

#[test]
fn sched_calib_a_long_stall_restarts_the_window() {
    let mut c = Calib::default();
    let a = [q(1, 0.8, 100.0), q(2, 0.6, 50.0)];
    c.observe(0, &a, &[0.03, 0.03], false);
    // asleep five hours with nothing used: the first reading after the
    // gap starts a window instead of counting five hours of asking
    let b = |h: f64| [q(1, 0.79 - 0.03 * (h - 5.0), 100.0 - h), q(2, 0.59 - 0.03 * (h - 5.0), 50.0 - h)];
    assert!(!c.observe(5 * 3600, &b(5.0), &[0.03, 0.03], false));
    for k in 1..=72 {
        c.observe(5 * 3600 + k * 300, &b(5.0 + k as f64 / 12.0), &[0.03, 0.03], false);
    }
    assert_eq!(c.factor(), 1.0);
    assert!(c.ratio.is_some_and(|r| (r - 1.0).abs() < 1e-6), "{:?}", c.ratio);
}

#[test]
fn sched_calib_a_factor_in_force_holds_near_the_band_edge() {
    let mut c = Calib::default();
    let (six, mut lost) = (6 * 3600, 0.0);
    window_on(&mut c, 0, &mut lost, 0.01, 0.0138);
    assert_eq!(c.factor(), 1.0, "1.38 is inside the band");
    window_on(&mut c, six, &mut lost, 0.01, 0.0142);
    assert!((c.factor() - 1.42).abs() < 1e-6, "{}", c.factor());
    // back inside the band but not inside the narrower one: the factor
    // follows the ratio instead of flipping to 1
    window_on(&mut c, 2 * six, &mut lost, 0.01, 0.0138);
    assert!((c.factor() - 1.38).abs() < 1e-6, "{}", c.factor());
    window_on(&mut c, 3 * six, &mut lost, 0.01, 0.012);
    assert_eq!(c.factor(), 1.0);
}

#[test]
fn sched_calib_use_lost_without_ask_counts_toward_the_least_use() {
    let mut c = Calib::default();
    // speeds that ask four points against twelve lost: the loss alone
    // reaches the least use
    window(&mut c, 0, 0.0033, 0.01);
    assert!(c.ratio.is_some_and(|r| r > BAND[1]), "{:?}", c.ratio);
}
