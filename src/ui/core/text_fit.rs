//! Cutting text to the box that holds it.
//!
//! One rule, in one place, because the two ways of getting it wrong
//! both shipped: the layout modal's cards drew their labels at full
//! length and let them run over the neighbouring card, and the search
//! overlay's result list cut its snippets with `chars().take(n)` — no
//! mark, so a path that ran out of room read as a path that ended.
//!
//! What the rule says:
//!
//! * the box keeps `pad` of clear space on **each** side, and it is
//!   the same `pad` the box was *sized* with — cut with a different
//!   one and a label measured to fit still loses its tail;
//! * truncated text ends in `…`, one cell wide, so "shortened"
//!   is visible rather than inferred;
//! * a box too small for even one character gets the ellipsis alone,
//!   never an empty row that reads as a blank result.

/// Fit `s` into `box_w` physical px of monospace, ellipsised.
///
/// `pad` is per-side, in physical px.
pub fn fit_mono(s: &str, box_w: f64, cell_w: f64, pad: f64) -> String {
    if cell_w <= 0.0 {
        return String::new();
    }
    let budget = ((box_w - 2.0 * pad) / cell_w).floor().max(0.0) as usize;
    let n = s.chars().count();
    if n <= budget {
        return s.to_string();
    }
    if budget <= 1 {
        return "…".to_string();
    }
    s.chars().take(budget - 1).chain(std::iter::once('…')).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three cases the callers actually hit.
    #[test]
    fn it_fits_marks_and_never_returns_nothing() {
        let cell = 10.0;
        // Fits: untouched, and no ellipsis appears where nothing was
        // dropped.
        assert_eq!(fit_mono("abcd", 60.0, cell, 10.0), "abcd");
        // Doesn't fit: cut, and the cut is visible.
        let cut = fit_mono("abcdefghij", 60.0, cell, 10.0);
        assert_eq!(cut.chars().count(), 4, "4 cells of room");
        assert!(cut.ends_with('…'), "a cut has to show: {cut:?}");
        assert!("abcdefghij".starts_with(&cut[..cut.len() - '…'.len_utf8()]));
        // No room at all: an ellipsis, not an empty row — a blank line
        // in a result list reads as a blank result.
        assert_eq!(fit_mono("abcdefghij", 12.0, cell, 5.0), "…");
        assert_eq!(fit_mono("abcdefghij", 0.0, cell, 0.0), "…");
    }

    /// Multi-byte text is cut by character, not by byte — a snippet of
    /// CJK cut mid-codepoint would not be text at all.
    #[test]
    fn it_counts_characters_not_bytes() {
        let out = fit_mono("回收空闲的窗格", 60.0, 10.0, 10.0);
        assert_eq!(out.chars().count(), 4);
        assert!(out.ends_with('…'));
        assert!(out.starts_with("回收空"));
    }

    /// A degenerate cell width cannot divide by zero into a panic or a
    /// gigantic budget.
    #[test]
    fn a_zero_cell_width_is_not_a_crash() {
        assert_eq!(fit_mono("abc", 100.0, 0.0, 0.0), "");
    }
}
