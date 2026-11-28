//! The speed a pane is planned at, from its level and its recent use.

/// The speed a pane is planned at in one class: its 24-hour level, its
/// short-window speed when that lies above the level's band (`hi` times
/// the level), and the band's bottom (`lo` times the level) when the
/// short-window speed lies below it.  A level that is off by more than
/// its band is shown by the pane's own recent use, and planning on the
/// level then times every switch off its account by a wrong exhaustion;
/// a pane idle for the short window reads a speed of nothing there and
/// is not planned as if it would stay idle.
pub fn planned_speed(day: f64, short: f64, lo: f64, hi: f64) -> f64 {
    if day <= 0.0 {
        day
    } else if short > day * hi {
        short
    } else if short < day * lo {
        day * lo
    } else {
        day
    }
}
