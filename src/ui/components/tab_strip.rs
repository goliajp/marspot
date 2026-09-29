//! Horizontal tab strip with overflow handling.
//!
//! Each tab is an equal-width slot inside a parent strip rect.  Labels
//! are truncated with an ellipsis when the slot is too narrow to fit
//! the full string at the current cell width.

use marspot_term::layout::Rect;

pub struct TabStrip {
    pub rect: Rect,
    pub tab_rects: Vec<Rect>,
    /// Display labels — may be ellipsis-truncated copies of caller's input.
    pub display_labels: Vec<String>,
    pub active: usize,
}

impl TabStrip {
    /// Lay out `labels.len()` equal-width tabs across `strip`.  Each
    /// label is truncated with "…" if its width exceeds the slot's
    /// available width (slot_w - padding).
    pub fn layout(
        strip: Rect,
        labels: &[String],
        active: usize,
        cell_w: f64,
        slot_h_pad: f64,
    ) -> Self {
        let n = labels.len();
        if n == 0 {
            return Self { rect: strip, tab_rects: Vec::new(), display_labels: Vec::new(), active: 0 };
        }
        let tab_w = strip.w / (n as f64);
        let avail_chars = ((tab_w - 2.0 * slot_h_pad) / cell_w).max(1.0) as usize;
        let mut tab_rects = Vec::with_capacity(n);
        let mut display_labels = Vec::with_capacity(n);
        for (i, label) in labels.iter().enumerate() {
            tab_rects.push(Rect {
                x: strip.x + (i as f64) * tab_w,
                y_top: strip.y_top,
                w: tab_w,
                h: strip.h,
            });
            display_labels.push(truncate_with_ellipsis(label, avail_chars));
        }
        Self {
            rect: strip,
            tab_rects,
            display_labels,
            active: active.min(n.saturating_sub(1)),
        }
    }

    pub fn hit_test(&self, x: f64, y: f64) -> Option<usize> {
        self.tab_rects
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains(x, y))
            .map(|(i, _)| i)
    }
}

/// Truncate a UTF-8 string to at most `max_chars` Unicode scalar values,
/// appending '…' when truncation happens.  Returns the original when
/// it already fits or `max_chars <= 1`.
fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    if max_chars <= 1 {
        return s.chars().take(max_chars).collect();
    }
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y_top: y, w, h }
    }

    #[test]
    fn empty_labels_no_rects() {
        let t = TabStrip::layout(rect(0.0, 0.0, 800.0, 30.0), &[], 0, 8.0, 4.0);
        assert!(t.tab_rects.is_empty());
        assert!(t.display_labels.is_empty());
    }

    #[test]
    fn equal_width_tabs() {
        let labels = vec!["A".to_string(), "B".to_string(), "C".to_string(), "D".to_string()];
        let t = TabStrip::layout(rect(0.0, 0.0, 800.0, 30.0), &labels, 1, 8.0, 4.0);
        for r in &t.tab_rects {
            assert_eq!(r.w, 200.0);
        }
        assert_eq!(t.active, 1);
    }

    #[test]
    fn label_truncates_with_ellipsis_when_too_wide() {
        // Tab width 80, padding 4 each side, cell 8 → avail = (80 - 8) / 8 = 9 chars.
        let labels = vec!["12345678901234".to_string(); 1]; // 14 chars
        let t = TabStrip::layout(rect(0.0, 0.0, 80.0, 30.0), &labels, 0, 8.0, 4.0);
        assert_eq!(t.display_labels[0].chars().count(), 9);
        assert!(t.display_labels[0].ends_with('…'));
    }

    #[test]
    fn label_fits_without_ellipsis() {
        let labels = vec!["abc".to_string()];
        let t = TabStrip::layout(rect(0.0, 0.0, 200.0, 30.0), &labels, 0, 8.0, 4.0);
        assert_eq!(t.display_labels[0], "abc");
    }

    #[test]
    fn active_clamped_to_last_when_out_of_range() {
        let labels = vec!["a".to_string(), "b".to_string()];
        let t = TabStrip::layout(rect(0.0, 0.0, 200.0, 30.0), &labels, 99, 8.0, 4.0);
        assert_eq!(t.active, 1);
    }
}
