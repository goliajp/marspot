//! The reference feed and a comparison of everything a feed can be
//! seen to have done — what the differential tests and the fuzz target
//! hold the bulk path to.

use super::Terminal;

impl Terminal {
    /// `feed`, with every byte through the state machine and every
    /// character through the per-character path.
    #[doc(hidden)]
    pub fn feed_reference(&mut self, bytes: &[u8]) {
        self.feed_with(bytes, false);
    }

    /// The first difference between two terminals that should have
    /// been driven to the same place, or `None`.
    ///
    /// Covers the screen (cells, cluster text, continuation flags,
    /// prompt marks), the cursor and its deferred wrap, the current
    /// attributes, the margins, every line of history with its flag
    /// and its clusters, the queued replies, and the snapshot bytes,
    /// which carry the modes.
    #[doc(hidden)]
    pub fn first_difference(&self, other: &Terminal) -> Option<String> {
        let (a, b) = (self.grid(), other.grid());
        if (a.cols(), a.rows()) != (b.cols(), b.rows()) {
            return Some("grid size".into());
        }
        if a.cursor() != b.cursor() {
            return Some(format!("cursor {:?} / {:?}", a.cursor(), b.cursor()));
        }
        if self.pending_wrap != other.pending_wrap {
            return Some("deferred wrap".into());
        }
        if self.attrs != other.attrs {
            return Some("current attributes".into());
        }
        if (self.scroll_top, self.scroll_bot) != (other.scroll_top, other.scroll_bot) {
            return Some("margins".into());
        }
        if self.in_alt_screen() != other.in_alt_screen() {
            return Some("alternate screen".into());
        }
        for r in 0..a.rows() {
            if a.row_wrapped(r) != b.row_wrapped(r) {
                return Some(format!("row {r} continuation flag"));
            }
            if a.row_prompt(r) != b.row_prompt(r) {
                return Some(format!("row {r} prompt mark"));
            }
            for c in 0..a.cols() {
                let (x, y) = (a.cell(c, r), b.cell(c, r));
                let same = match (a.cluster_text(&x), b.cluster_text(&y)) {
                    (Some(s), Some(t)) => s == t && x.attrs == y.attrs,
                    (None, None) => x == y,
                    _ => false,
                };
                if !same {
                    return Some(format!(
                        "cell ({c},{r}): {x:?} {:?} / {y:?} {:?}",
                        a.cluster_text(&x),
                        b.cluster_text(&y)
                    ));
                }
            }
        }
        if a.scrollback_len() != b.scrollback_len() {
            return Some(format!(
                "history length {} / {}",
                a.scrollback_len(),
                b.scrollback_len()
            ));
        }
        let (mut ca, mut cb) = (Vec::new(), Vec::new());
        for i in 0..a.scrollback_len() {
            if a.scrollback_line(i) != b.scrollback_line(i) {
                return Some(format!("history line {i}"));
            }
            if a.scrollback_wrapped(i) != b.scrollback_wrapped(i) {
                return Some(format!("history line {i} continuation flag"));
            }
            let back = a.scrollback_len() - i;
            if let Ok(view) = u16::try_from(back) {
                a.row_clusters_at_view(view, 0, &mut ca);
                b.row_clusters_at_view(view, 0, &mut cb);
                if ca != cb {
                    return Some(format!("history line {i} clusters"));
                }
            }
        }
        if self.pending_response != other.pending_response {
            return Some("queued replies".into());
        }
        if self.osc_title != other.osc_title {
            return Some("title".into());
        }
        if self.serialize_snapshot_live() != other.serialize_snapshot_live() {
            return Some("snapshot bytes".into());
        }
        None
    }
}
