//! The band around a forecast: the low and high multipliers of a level
//! that the actual six-hour mean fell between.
//!
//! From the 10% and 90% quantiles of the measured ratio (actual ÷
//! forecast) once seven days of facts exist, from a default before
//! that; either way widened, one measured ratio at a time, until it
//! covers 80% of the ratios measured.

/// The share of measured ratios a band must cover.
pub const COVER_NUM: usize = 4;
pub const COVER_DEN: usize = 5;

/// The band of each class until seven days of facts exist: (low, high).
pub const DEFAULT_RESTRICTED: (f64, f64) = (0.3, 2.5);
pub const DEFAULT_PLAIN: (f64, f64) = (0.6, 1.5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandFrom {
    /// Fewer than seven days of facts: the class's default.
    Default,
    /// The 10% and 90% quantiles of the measured ratios.
    Measured,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub lo: f64,
    pub hi: f64,
    pub from: BandFrom,
    /// It was widened to cover 80% of the ratios.
    pub widened: bool,
    /// Ratios measured, and how many of them the band covers.
    pub pairs: usize,
    pub covered: usize,
}

impl Band {
    /// The share of measured ratios inside the band; `None` with none measured.
    pub fn coverage(&self) -> Option<f64> {
        (self.pairs > 0).then(|| self.covered as f64 / self.pairs as f64)
    }
}

/// The band over `ratios` (in any order), starting from the measured
/// quantiles when `enough` facts exist, else from `default`.
pub fn measure(ratios: &mut [f64], default: (f64, f64), enough: bool) -> Band {
    ratios.sort_by(f64::total_cmp);
    let (lo, hi, from) = if enough && !ratios.is_empty() {
        (quantile(ratios, 0.1), quantile(ratios, 0.9), BandFrom::Measured)
    } else {
        (default.0, default.1, BandFrom::Default)
    };
    let (lo, hi, widened) = widen(ratios, lo, hi);
    Band { lo, hi, from, widened, pairs: ratios.len(), covered: covered(ratios, lo, hi) }
}

/// Widen `[lo, hi]` over the sorted `ratios` until it covers 80% of
/// them: each step takes in the nearest ratio outside, below or above,
/// by its distance in ratio units (below first on a tie).  A pane that
/// went quiet (a ratio of 0) costs at most `lo` to take in, a burst its
/// full excess over `hi`.
pub fn widen(sorted: &[f64], mut lo: f64, mut hi: f64) -> (f64, f64, bool) {
    let need = (COVER_NUM * sorted.len()).div_ceil(COVER_DEN);
    let mut widened = false;
    while covered(sorted, lo, hi) < need {
        let below = sorted.partition_point(|r| *r < lo).checked_sub(1).map(|i| sorted[i]);
        let above = sorted.get(sorted.partition_point(|r| *r <= hi)).copied();
        match (below, above) {
            (Some(b), Some(a)) if lo - b <= a - hi => lo = b,
            (Some(b), None) => lo = b,
            (_, Some(a)) => hi = a,
            (None, None) => break,
        }
        widened = true;
    }
    (lo, hi, widened)
}

/// How many of the sorted `ratios` lie in `[lo, hi]`.
pub fn covered(sorted: &[f64], lo: f64, hi: f64) -> usize {
    sorted.partition_point(|r| *r <= hi).saturating_sub(sorted.partition_point(|r| *r < lo))
}

/// The `q` quantile of sorted `xs`, interpolated between neighbours.
pub fn quantile(sorted: &[f64], q: f64) -> f64 {
    let k = (sorted.len() - 1) as f64 * q;
    let (a, b) = (k.floor() as usize, k.ceil() as usize);
    sorted[a] + (sorted[b] - sorted[a]) * (k - a as f64)
}
