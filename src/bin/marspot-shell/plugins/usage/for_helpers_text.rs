//! When the accounts that refuse a helper take work again, as text.

use super::Recovery;

/// `P1 at 2026-11-23T14:00:00+00:00, P2 at an unknown time`.
pub(in crate::plugins) fn recoveries_text(r: &[Recovery]) -> String {
    let one = |x: &Recovery| {
        let name = if x.number == 0 { "default".to_string() } else { format!("P{}", x.number) };
        x.at.map_or_else(|| format!("{name} at an unknown time"), |t| format!("{name} at {}", super::super::feed::iso_utc(t)))
    };
    r.iter().map(one).collect::<Vec<_>>().join(", ")
}

/// The soonest known recovery among `r`, as text.
pub(in crate::plugins) fn earliest_text(r: &[Recovery]) -> Option<String> {
    r.iter().filter_map(|x| x.at).min().map(super::super::feed::iso_utc)
}
