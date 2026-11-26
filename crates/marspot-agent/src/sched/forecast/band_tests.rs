use super::band::{Band, BandFrom, covered, measure, quantile, widen};

#[test]
fn forecast_band_default_that_covers_is_not_widened() {
    let mut r = vec![0.9, 1.0, 1.1, 1.2, 0.7];
    let b = measure(&mut r, (0.6, 1.5), false);
    assert_eq!(b, Band { lo: 0.6, hi: 1.5, from: BandFrom::Default, widened: false, pairs: 5, covered: 5 });
    assert_eq!(b.coverage(), Some(1.0));
}

/// Each step takes in the nearest ratio outside, by distance, until
/// 80% are inside; ties go below.
#[test]
fn forecast_band_widens_to_the_nearest_ratio_until_80_percent() {
    // 10 ratios, 4 outside [0.6, 1.5]: two must come in
    let mut r = vec![0.0, 0.5, 0.8, 0.9, 1.0, 1.0, 1.1, 1.2, 1.7, 4.0];
    let b = measure(&mut r, (0.6, 1.5), false);
    // 0.5 is 0.1 below, 1.7 is 0.2 above: 0.5 first, then 1.7 (0.0 is 0.5 below)
    assert_eq!((b.lo, b.hi, b.widened, b.covered), (0.5, 1.7, true, 8));
    // a quiet pane (ratio 0) is taken in before a large burst
    let s = [0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 9.0];
    assert_eq!(widen(&s, 0.6, 1.5), (0.0, 1.5, true));
    // a tie goes below
    assert_eq!(widen(&[0.5, 1.0, 1.0, 1.0, 1.6], 0.6, 1.5), (0.5, 1.5, true));
    // with nothing measured the default stands
    let b = measure(&mut [], (0.3, 2.5), true);
    assert_eq!((b.lo, b.hi, b.from, b.coverage()), (0.3, 2.5, BandFrom::Default, None));
}

#[test]
fn forecast_band_measured_starts_from_the_quantiles() {
    let mut r: Vec<f64> = (0..=100).rev().map(|k| k as f64 / 50.0).collect();
    let b = measure(&mut r, (0.6, 1.5), true);
    assert_eq!(b.from, BandFrom::Measured);
    assert!((b.lo - 0.2).abs() < 1e-12 && (b.hi - 1.8).abs() < 1e-12, "{b:?}");
    assert!(b.covered * 5 >= b.pairs * 4);
    // the quantiles of a small sample can cover under 80%: widened then
    let mut small = vec![0.1, 0.5, 0.9, 1.0, 1.1, 1.5, 3.0];
    let b = measure(&mut small, (0.6, 1.5), true);
    assert!(b.covered * 5 >= b.pairs * 4, "{b:?}");
    assert!(b.widened);
}

#[test]
fn forecast_band_helpers() {
    let s = [1.0, 2.0, 3.0, 4.0];
    assert_eq!(quantile(&s, 0.5), 2.5);
    assert_eq!(covered(&s, 2.0, 3.0), 2);
    assert_eq!(covered(&s, 5.0, 6.0), 0);
}
