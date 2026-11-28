use super::*;

fn account(r: f64, t: f64) -> Quant {
    Quant { id: 1, r, f: r, t, tf: t, open: true, stale: false }
}

fn plain(rate: f64) -> Load {
    Load { restricted: 0.0, plain: rate }
}

/// A reset that brings the week back any time and moves the week.
fn any_time(ends_h: f64, left: u32) -> Supply {
    Supply { week: true, window: false, gives_back_week: false, week_cut: WeekCut::Nothing, moves_week: true, requires_limit: false, starts_h: 0.0, ends_h: Some(ends_h), left, usable_below_left: None }
}

/// A `full` reset: both windows, only once at the limit.
fn full(ends_h: f64) -> Supply {
    Supply { week: true, window: true, gives_back_week: false, week_cut: WeekCut::Nothing, moves_week: false, requires_limit: true, starts_h: 0.0, ends_h: Some(ends_h), left: 1, usable_below_left: None }
}

#[test]
fn no_supply_leaves_the_account_exactly_as_it_was() {
    let q = account(0.4, 50.0);
    assert_eq!(with_supply(&q, &plain(0.01), 0.528, &[]), q);
}

#[test]
fn a_reset_used_at_the_limit_counts_a_whole_week_when_the_limit_comes_before_it_ends() {
    // 0.2 left at 0.01 an hour: at the limit in 20 h, the reset ends in 30 h
    let q = account(0.2, 100.0);
    let got = with_supply(&q, &plain(0.01), 0.528, &[full(30.0)]);
    assert!((got.r - 1.2).abs() < 1e-9, "{got:?}");
    // the window, used by no restricted pane, never reaches its limit
    assert_eq!(got.f, q.f);

    // the reset ends before the limit comes: it brings nothing
    let late = with_supply(&q, &plain(0.01), 0.528, &[full(10.0)]);
    assert_eq!(late.r, q.r);
    // the week renews by itself before the limit: nothing either
    let renews = with_supply(&account(0.2, 15.0), &plain(0.01), 0.528, &[full(30.0)]);
    assert_eq!(renews.r, 0.2);
    // nobody uses the account: nothing to bring back
    assert_eq!(with_supply(&q, &plain(0.0), 0.528, &[full(30.0)]).r, q.r);
}

#[test]
fn a_reset_usable_any_time_brings_back_what_was_used_by_its_end() {
    // 0.9 left, 0.01 an hour, ends in 30 h: used then, it brings back 0.4
    let q = account(0.9, 100.0);
    let got = with_supply(&q, &plain(0.01), 1.0, &[any_time(30.0, 1)]);
    assert!((got.r - 1.3).abs() < 1e-9, "{got:?}");
    assert!((got.t - (30.0 + WEEK_H)).abs() < 1e-9, "the week now renews a week after the use");
}

#[test]
fn several_resets_are_used_one_after_another_earliest_ending_first() {
    // 0.1 left at 0.05 an hour: empty at 2 h, again at 22 h, again at 42 h
    let q = account(0.1, 160.0);
    let got = with_supply(&q, &plain(0.05), 1.0, &[any_time(25.0, 1), any_time(10.0, 1), any_time(30.0, 1)]);
    // the one ending at 10 h used at 2 h, the one ending at 25 h at 22 h,
    // the one ending at 30 h at its end, with 0.6 of the week used by then
    assert!((got.r - (0.1 + 1.0 + 1.0 + 0.4)).abs() < 1e-9, "{got:?}");
    assert!((got.t - (30.0 + WEEK_H)).abs() < 1e-9);
}

#[test]
fn a_reset_not_yet_usable_waits_for_its_start() {
    let q = account(0.1, 160.0);
    let mut s = full(30.0);
    s.window = false;
    s.starts_h = 12.0;
    // at the limit at 2 h, usable from 12 h: a whole week then
    let got = with_supply(&q, &plain(0.05), 1.0, &[s]);
    assert!((got.r - 1.1).abs() < 1e-9);
    s.starts_h = 31.0;
    assert_eq!(with_supply(&q, &plain(0.05), 1.0, &[s]).r, 0.1, "it starts after it ends");
}

#[test]
fn a_fable_reset_refills_the_window_and_gives_its_use_back_to_the_week() {
    let q = Quant { id: 2, r: 0.5, f: 0.05, t: 100.0, tf: 100.0, open: true, stale: false };
    let fable = Supply { week: false, window: true, gives_back_week: true, week_cut: WeekCut::Nothing, moves_week: false, requires_limit: true, starts_h: 0.0, ends_h: Some(40.0), left: 1, usable_below_left: None };
    let load = Load { restricted: 0.01, plain: 0.0 };
    let got = with_supply(&q, &load, 0.528, &[fable]);
    assert!((got.f - (0.05 + 0.528)).abs() < 1e-9, "{got:?}");
    assert!((got.r - (0.5 + 0.528)).abs() < 1e-9);
}

#[test]
fn a_window_reset_with_a_week_cut_refills_the_window_and_takes_its_cut_off_the_weeks_usage() {
    // the window is spent at 5 h, when the week has used 0.65
    let q = Quant { id: 2, r: 0.4, f: 0.05, t: 100.0, tf: 100.0, open: true, stale: false };
    let fable = Supply { week: false, window: true, gives_back_week: false, week_cut: WeekCut::ShareOfUsed(0.5), moves_week: false, requires_limit: true, starts_h: 0.0, ends_h: Some(40.0), left: 1, usable_below_left: None };
    let load = Load { restricted: 0.01, plain: 0.0 };
    let got = with_supply(&q, &load, 0.528, &[fable]);
    assert!((got.f - (0.05 + 0.528)).abs() < 1e-9, "{got:?}");
    assert!((got.r - (0.4 + 0.5 * 0.65)).abs() < 1e-9, "{got:?}");
    let fixed = with_supply(&q, &load, 0.528, &[Supply { week_cut: WeekCut::Fixed(0.5), ..fable }]);
    assert!((fixed.r - (0.4 + 0.5)).abs() < 1e-9, "{fixed:?}");
    assert_eq!(WeekCut::Fixed(0.5).of(0.2), 0.2, "never more than was used");
}

#[test]
fn a_week_cut_counts_the_week_already_given_back_before_it() {
    let load = Load { restricted: 0.01, plain: 0.005 };
    let fable = Supply { week: false, window: true, gives_back_week: false, week_cut: WeekCut::ShareOfUsed(0.5), moves_week: false, requires_limit: true, starts_h: 0.0, ends_h: None, left: 1, usable_below_left: None };
    // the full reset refills both at 13.3 h, when the week runs out; the
    // window is spent again 52.8 h later, with 0.792 of the week used
    let q = Quant { id: 5, r: 0.2, f: 0.4, t: 100.0, tf: 100.0, open: true, stale: false };
    let got = with_supply(&q, &load, 0.528, &[full(30.0), fable]);
    assert!((got.f - (0.4 + 0.528 - (0.4 - 0.2 / 0.015 * 0.01) + 0.528)).abs() < 1e-9, "{got:?}");
    assert!((got.r - (0.2 + 1.0 + 0.5 * 0.792)).abs() < 1e-9, "{got:?}");

    // two in a row: the first cuts 0.325 of 0.65 used at 5 h, so at 57.8 h
    // the week has used 0.853, not all of it
    let q = Quant { id: 6, r: 0.4, f: 0.05, t: 200.0, tf: 200.0, open: true, stale: false };
    let load = Load { restricted: 0.01, plain: 0.0 };
    let got = with_supply(&q, &load, 0.528, &[Supply { left: 2, ..fable }]);
    assert!((got.f - (0.05 + 2.0 * 0.528)).abs() < 1e-9, "{got:?}");
    assert!((got.r - (0.4 + 0.325 + 0.5 * 0.853)).abs() < 1e-9, "{got:?}");
}

#[test]
fn a_reset_usable_only_below_a_share_left_counts_nothing_while_little_is_used() {
    // 2% used, slowly: at its end in 30 h the week still has over 80% left
    let mut s = any_time(30.0, 1);
    s.usable_below_left = Some(0.8);
    let q = account(0.98, 100.0);
    let got = with_supply(&q, &plain(0.001), 1.0, &[s]);
    assert_eq!((got.r, got.t), (q.r, q.t), "neither more quota nor a moved week");
    // used enough by its end: it counts as any other
    let busy = with_supply(&account(0.3, 100.0), &plain(0.005), 1.0, &[s]);
    assert!((busy.r - 1.15).abs() < 1e-9, "{busy:?}");
    assert!((busy.t - (30.0 + WEEK_H)).abs() < 1e-9);
}

#[test]
fn a_use_that_moves_the_week_lets_a_later_reset_count_past_the_old_renewal() {
    // empty at 2 h, the first used then moves the renewal from 10 h to
    // 170 h; empty again at 22 h, where the second is used
    let q = account(0.1, 10.0);
    let got = with_supply(&q, &plain(0.05), 1.0, &[any_time(5.0, 1), any_time(40.0, 1)]);
    assert!((got.r - (0.1 + 1.0 + 1.0)).abs() < 1e-9, "{got:?}");
    assert!((got.t - (22.0 + WEEK_H)).abs() < 1e-9);
}

#[test]
fn a_reset_bringing_back_both_windows_is_used_once_when_the_week_runs_out() {
    // the week is empty at 20 h; the window, used at 0.01 an hour, has
    // 0.2 left then and is refilled at that same use
    let q = Quant { id: 3, r: 0.2, f: 0.4, t: 100.0, tf: 100.0, open: true, stale: false };
    let load = Load { restricted: 0.01, plain: 0.0 };
    let got = with_supply(&q, &load, 0.528, &[full(30.0)]);
    assert!((got.r - 1.2).abs() < 1e-9, "{got:?}");
    assert!((got.f - (0.4 + 0.328)).abs() < 1e-9, "{got:?}");
}

#[test]
fn only_what_a_fable_reset_refills_goes_back_to_the_week() {
    // the full reset is too early for the week (empty at 50 h) and refills
    // the window at 5 h; the fable reset refills it again at 57.8 h
    let q = Quant { id: 4, r: 0.5, f: 0.05, t: 100.0, tf: 100.0, open: true, stale: false };
    let fable = Supply { week: false, window: true, gives_back_week: true, week_cut: WeekCut::Nothing, moves_week: false, requires_limit: true, starts_h: 0.0, ends_h: None, left: 1, usable_below_left: None };
    let load = Load { restricted: 0.01, plain: 0.0 };
    let got = with_supply(&q, &load, 0.528, &[full(30.0), fable]);
    assert!((got.f - (0.05 + 2.0 * 0.528)).abs() < 1e-9, "{got:?}");
    assert!((got.r - (0.5 + 0.528)).abs() < 1e-9, "{got:?}");
}

#[test]
fn the_earliest_ending_resets_are_the_ones_counted_when_there_are_too_many() {
    // a cap's worth that cannot be used before the week renews, listed first
    let mut late = any_time(1_000.0, MAX_UNITS as u32);
    late.starts_h = 500.0;
    let q = account(0.0, 160.0);
    let got = with_supply(&q, &plain(1.0), 1.0, &[late, any_time(1.0, 1)]);
    assert!((got.r - 1.0).abs() < 1e-9, "the one ending in 1 h is counted: {got:?}");
    assert!((got.t - WEEK_H).abs() < 1e-9);
}

#[test]
fn an_account_is_idle_only_when_every_pane_on_it_rests() {
    assert!(account_idle([]), "an account nobody is on is idle");
    assert!(account_idle([true, true]));
    assert!(!account_idle([true, false]));
}

#[test]
fn two_readings_agree_on_count_and_end_when_close_in_time() {
    let a = Count { observed_at: 1_000, left: 4, next_expiry: Some(1_796_070_363) };
    assert_eq!(agree(a, a), Agreement::Same);
    assert_eq!(agree(a, Count { left: 3, ..a }), Agreement::Differs);
    assert_eq!(agree(a, Count { next_expiry: Some(1), ..a }), Agreement::Differs);
    assert_eq!(agree(a, Count { next_expiry: None, ..a }), Agreement::Same, "an end only one gives is not compared");
    assert_eq!(agree(a, Count { observed_at: 1_000 + AGREE_WITHIN_S + 1, left: 0, ..a }), Agreement::Apart);
}
