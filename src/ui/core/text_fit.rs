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

/// How many monospace cells `s` takes: two for a wide character, as
/// the terminal gives it, and never none.
pub fn cells(s: &str) -> usize {
    s.chars().map(|ch| marspot_term::grid::char_width(ch).max(1) as usize).sum()
}

/// Fit `s` into `box_w` physical px of monospace, ellipsised.
///
/// `pad` is per-side, in physical px.
pub fn fit_mono(s: &str, box_w: f64, cell_w: f64, pad: f64) -> String {
    if cell_w <= 0.0 {
        return String::new();
    }
    let budget = ((box_w - 2.0 * pad) / cell_w).floor().max(0.0) as usize;
    if cells(s) <= budget {
        return s.to_string();
    }
    if budget <= 1 {
        return "…".to_string();
    }
    // whole characters only: a wide one that would straddle the mark is dropped
    let mut room = budget - 1;
    let kept = s.chars().take_while(|ch| {
        let w = marspot_term::grid::char_width(*ch).max(1) as usize;
        let fits = w <= room;
        room = room.saturating_sub(w);
        fits
    });
    kept.chain(std::iter::once('…')).collect()
}

/// What [`fit_mono`] keeps of `s` in a box with no padding: how many
/// of its bytes, and whether an ellipsis follows them.  Nothing is
/// allocated: the caller draws the two pieces.
pub fn fit_mono_len(s: &str, box_w: f64, cell_w: f64) -> (usize, bool) {
    if cell_w <= 0.0 {
        return (0, false);
    }
    let budget = (box_w / cell_w).floor().max(0.0) as usize;
    if cells(s) <= budget {
        return (s.len(), false);
    }
    if budget <= 1 {
        return (0, true);
    }
    let mut room = budget - 1;
    let mut len = 0;
    for ch in s.chars() {
        let w = marspot_term::grid::char_width(ch).max(1) as usize;
        if w > room {
            break;
        }
        room -= w;
        len += ch.len_utf8();
    }
    (len, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two ways of asking keep the same text.
    #[test]
    fn the_length_kept_is_the_string_kept() {
        for s in ["", "a", "abcdefghij", "日本語のテキスト", "a日b本c語", "rust-analyzer-proc-macro-srv"] {
            for box_w in [0.0, 9.0, 10.0, 19.0, 20.0, 35.0, 60.0, 61.0, 1000.0] {
                let (len, cut) = fit_mono_len(s, box_w, 10.0);
                let mut drawn = s[..len].to_string();
                if cut {
                    drawn.push('…');
                }
                assert_eq!(drawn, fit_mono(s, box_w, 10.0, 0.0), "{s:?} in {box_w}");
            }
        }
        assert_eq!(fit_mono_len("abc", 100.0, 0.0), (0, false));
    }

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

    /// Multi-byte text is cut by character, not by byte, and a wide
    /// character counts as the two cells it is drawn in: four cells
    /// hold one of them and the mark, not three.
    #[test]
    fn it_counts_cells_not_bytes_or_characters() {
        let out = fit_mono("回收空闲的窗格", 60.0, 10.0, 10.0);
        assert_eq!(out, "回…");
        assert_eq!(cells(&out), 3, "the next character would not fit whole");
        assert_eq!(fit_mono("回收", 60.0, 10.0, 10.0), "回收", "four cells of text in four cells");
        assert_eq!(fit_mono("a回b", 60.0, 10.0, 10.0), "a回b");
        assert_eq!(cells("pid数"), 5);
    }

    /// A degenerate cell width cannot divide by zero into a panic or a
    /// gigantic budget.
    #[test]
    fn a_zero_cell_width_is_not_a_crash() {
        assert_eq!(fit_mono("abc", 100.0, 0.0, 0.0), "");
    }
}
