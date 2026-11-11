//! How a settings value is read from the file and written back.

/// A float, clamped to a range the rest of the program can survive.
///
/// Out of range is treated as a typo and ignored rather than clamped:
/// `scroll_factor = 100` is far more likely a slip than a wish, and
/// silently honouring a tenth of it teaches nothing.
pub(super) fn parse_f32(v: &str, fallback: f32, lo: f32, hi: f32) -> f32 {
    match v.parse::<f32>() {
        Ok(f) if f.is_finite() && f >= lo && f <= hi => f,
        _ => fallback,
    }
}

/// Trailing-zero-free, so `1.0` writes as `1` and a hand-edited file
/// does not grow noise every time the panel rewrites it.
pub(super) fn fmt_f32(f: f32) -> String {
    let s = format!("{f:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

pub(super) fn parse_bool(v: &str, fallback: bool) -> bool {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => true,
        "false" | "no" | "off" | "0" => false,
        _ => fallback,
    }
}
