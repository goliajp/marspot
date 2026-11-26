//! What the accounts panel shows for a reading, and how its text fits.

use super::*;

/// No reading is a dash on the card and in the timeline's tag; a
/// reading of nothing used is `0%`.  The two are never the same
/// text.
#[test]
fn a_window_without_a_reading_prints_a_dash_and_zero_prints_zero() {
    let window = |util| CcUsageWindowRender {
        label: "7d".into(),
        util,
        reset_unix: Some(1_794_538_799),
        span_secs: 7 * 86_400,
        reset_hm: "12:00".into(),
    };
    assert_eq!((window(None).percent(), window(None).tag()), ("—".to_string(), "7d — 12:00".to_string()));
    assert_eq!((window(Some(0.0)).percent(), window(Some(0.0)).tag()), ("0%".to_string(), "7d 0% 12:00".to_string()));
    assert_eq!(window(Some(0.424)).tag(), "7d 42% 12:00");
    assert_eq!(crate::ui::core::text_fit::cells(&window(None).percent()), 1, "one cell in the terminal font");
}

#[test]
fn fit_ellipsis_bounds_every_case() {
    // Fits untouched.
    assert_eq!(fit_ellipsis("ada@example.jp", 20.0), "ada@example.jp");
    assert_eq!(fit_ellipsis("abc", 3.0), "abc");
    // Too long: cut to exactly the budget, last column is the mark.
    let cut = fit_ellipsis("ada@example.jp", 8.0);
    assert_eq!(cut, "ada@exa…");
    assert_eq!(cut.chars().count(), 8);
    // Never exceeds the budget for any prefix length.
    for cols in 3..30 {
        let out = fit_ellipsis("some.very.long.address@example.com", cols as f64);
        assert!(out.chars().count() <= cols, "cols={cols} out={out:?}");
    }
    // Below the useful minimum the slot is dropped, not filled with
    // a bare ellipsis.
    assert_eq!(fit_ellipsis("abcdef", 2.0), "");
    assert_eq!(fit_ellipsis("abcdef", 0.0), "");
}

/// Uneven widths, like a proportional font: `m` and `w` are wide,
/// `i`, `l` and `.` narrow.
fn uneven(s: &str) -> f64 {
    s.chars()
        .map(|c| match c {
            'm' | 'w' | '@' => 11.0,
            'i' | 'l' | '.' => 3.0,
            _ => 7.0,
        })
        .sum()
}

#[test]
fn fit_measured_keeps_the_longest_prefix_that_fits() {
    let email = "some.very.long.address@example.com";
    assert_eq!(fit_measured(email, uneven(email), &mut uneven), email, "fits: untouched");
    for max_w in [20.0, 60.0, 100.0, 150.0, uneven(email) - 0.5] {
        let out = fit_measured(email, max_w, &mut uneven);
        assert!(uneven(&out) <= max_w, "max_w={max_w} out={out:?}");
        assert!(out.ends_with('…'), "a cut has to show: {out:?}");
        let kept = out.trim_end_matches('…');
        assert!(email.starts_with(kept));
        assert!(!kept.ends_with('.'), "the mark must not follow a full stop: {out:?}");
        // One more character would not have fitted — one more
        // that is not a full stop, which is never the last kept.
        let more = email[kept.len()..].find(|c| c != '.').map_or(email.len(), |i| kept.len() + i + 1);
        let longer = format!("{}…", &email[..more]);
        assert!(uneven(&longer) > max_w, "max_w={max_w}: {longer:?} also fits");
    }
    // "some.very." would fit; the cut steps back over the stop.
    assert_eq!(fit_measured(email, uneven("some.very.…"), &mut uneven), "some.very…");
    assert_eq!(fit_measured(email, 7.0, &mut uneven), "…");
    assert_eq!(fit_measured(email, 5.0, &mut uneven), "", "no room for the mark either");
}

/// Every bar must fit: the plot exists to show where each window
/// ends, and a bar clamped at the axis edge says the opposite of
/// what its own label says.
///
/// Numbers are the live feed of 2026-08-01 (four accounts, both
/// windows each).  Under the old fixed ±6 d span, Claude 1's and
/// Claude 4's 7-day bars ended at 1.014 and 1.056 of the span.
#[test]
fn every_window_fits_inside_the_range() {
    let now = 1785538544i64;
    let resets_7d = [1786071600i64, 1785704400, 1785646800, 1786114800];
    let resets_5h = [1785543600i64, 1785544800, 1785543600, 1785546000];
    let extents: Vec<(i64, i64)> = resets_5h
        .iter()
        .map(|r| (r - 5 * 3_600, *r))
        .chain(resets_7d.iter().map(|r| (r - 7 * 86_400, *r)))
        .collect();
    let (t0, t1) = timeline_range(now, extents.iter().copied());
    for (start, end) in extents {
        let f0 = (start as f64 - t0) / (t1 - t0);
        let f1 = (end as f64 - t0) / (t1 - t0);
        assert!(f0 >= 0.0 && f1 <= 1.0, "window {start}..{end} escapes the plot: {f0}..{f1}");
    }
    // Now is inside it too — the marker has to land somewhere real.
    assert!((now as f64) > t0 && (now as f64) < t1);
}

/// A feed whose windows all reset within the hour would otherwise
/// be drawn at a scale where clock skew looks like a day.
#[test]
fn a_degenerate_feed_falls_back_to_the_fixed_span() {
    let now = 1785538544i64;
    let (t0, t1) = timeline_range(now, [(now - 60, now + 60)]);
    assert!((t1 - t0 - TIMELINE_SPAN_SECS).abs() < 1.0);
}

/// A fixed offset reads the same instants as the same dates on any
/// machine.
#[test]
fn a_fixed_clock_does_not_ask_the_machine_what_day_it_is() {
    // 2026-11-10 09:00 UTC.
    let now = 1_794_301_200;
    let utc = Clock::fixed(now, 0);
    assert_eq!(utc.month_day(now), (11, 10));
    assert_eq!(utc.day_start(now), now - 9 * 3_600);
    assert_eq!(utc.next_day_start(now), now + 15 * 3_600);
    assert_eq!(utc.day_start(utc.day_start(now)), utc.day_start(now), "midnight is its own day's start");
    // Nine hours east it is 18:00 the same day, and midnight came
    // nine hours sooner.
    let east = Clock::fixed(now, 9 * 3_600);
    assert_eq!(east.month_day(now), (11, 10));
    assert_eq!(east.day_start(now), now - 18 * 3_600);
    assert_eq!(east.month_day(now + 6 * 3_600), (11, 11));
    // Month and year ends, and a leap day.
    assert_eq!(utc.month_day(1_798_761_599), (12, 31)); // 2026-12-31 23:59:59
    assert_eq!(utc.month_day(1_798_761_600), (1, 1));
    assert_eq!(utc.month_day(1_709_164_800), (2, 29)); // 2024-02-29
    assert_eq!(utc.month_day(0), (1, 1));
    let (t0, t1) = utc.snap_to_days(now as f64, (now + 3 * 86_400) as f64);
    assert_eq!((t0, t1), ((now - 9 * 3_600) as f64, (now + 3 * 86_400 + 15 * 3_600) as f64));
    assert_eq!(utc.snap_to_days(t0, t1), (t0, t1), "already on day boundaries: left alone");
}

/// With no offset given the days are the machine's, as before.
#[test]
fn a_local_clock_agrees_with_the_machines_calendar() {
    let now = 1_794_301_200;
    let local = Clock::local(now);
    assert_eq!(local.day_start(now), crate::cc_usage::local_day_start(now));
    assert_eq!(local.next_day_start(now), crate::cc_usage::next_local_day_start(now));
    let (mo, d, _, _) = crate::cc_usage::local_mdhm(now);
    assert_eq!(local.month_day(now), (mo, d));
    assert_eq!(
        local.snap_to_days(now as f64, (now + 86_400) as f64),
        crate::cc_usage::snap_range_to_local_days(now as f64, (now + 86_400) as f64),
    );
}

/// The panel takes the widest step a panel has and never leaves
/// the window, whatever it is asked to hold.
#[test]
fn panel_rect_stays_inside_the_window() {
    for n in 0..=12 {
        let r = panel_rect(n, 3, 1600.0, 1000.0, 8.0, 18.0, 60.0);
        assert_eq!(r.w, 1600.0 * spec::panel::MAX_W_FRAC, "n={n}");
        assert!(r.x >= 0.0 && r.x + r.w <= 1600.0, "n={n} overflows width");
        assert!(r.y_top >= 60.0, "n={n} must clear the top inset");
        assert!(r.h <= 1000.0 * spec::panel::MAX_H_FRAC + 0.001, "n={n} too tall");
    }
    // No accounts: the header and the band the sentence sits in.
    let empty = panel_rect(0, 0, 1600.0, 1000.0, 8.0, 18.0, 60.0);
    assert_eq!(empty.h, panel_chrome::frame_height(spec::px(metric::EMPTY_H)));
}
