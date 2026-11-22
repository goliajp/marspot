//! The print path below `ParserCallbacks::print`: recognising `<u>`
//! spans, and building grapheme clusters into the cell at their anchor.

use super::lane::fast_width;
use super::{CLUSTER_POOL_SWEEP_AT, Handler};

/// How much text a `<u>` may hold before we give up on its close.
///
/// A sentence is far short of this; a span that runs longer has almost
/// certainly lost its closing tag, and holding output hostage waiting
/// for one is worse than printing the tag.
const U_SPAN_CAP: usize = 1024;

/// What a buffer starting with `<` has turned out to be.
enum UTag {
    /// `<u>` — start collecting what it wraps.
    Open,
    /// Still a prefix of one of them.
    Maybe,
    /// It is not, and never will be.
    No,
}

fn u_tag_verdict(buf: &str) -> UTag {
    match buf {
        "<u>" => UTag::Open,
        "<" | "<u" => UTag::Maybe,
        // A closing tag with nothing open is just text.
        _ => UTag::No,
    }
}

impl<'a> Handler<'a> {
    /// One character, while a `<u>` is being recognised or collected.
    pub(super) fn u_step(&mut self, ch: char) {
        if *self.u_open {
            // Collecting the span.  `</u>` closes it; anything else
            // just accumulates until it does or the cap says stop.
            self.u_buf.push(ch);
            if self.u_buf.ends_with("</u>") {
                let span: String = self.u_buf[..self.u_buf.len() - "</u>".len()].to_string();
                self.u_buf.clear();
                *self.u_open = false;
                // The text before the tag keeps the attributes it
                // arrived with; flush it before changing them.
                self.flush_cluster_for_break();
                let was = self.attrs.underline;
                self.attrs.underline = true;
                for c in span.chars() {
                    self.print_glyph(c);
                }
                self.flush_cluster_for_break();
                self.attrs.underline = was;
                return;
            }
            if self.u_buf.len() > U_SPAN_CAP {
                self.u_bail();
            }
            return;
        }
        self.u_buf.push(ch);
        match u_tag_verdict(self.u_buf) {
            UTag::Open => {
                self.u_buf.clear();
                *self.u_open = true;
            }
            UTag::Maybe => {}
            UTag::No => {
                let held = std::mem::take(self.u_buf);
                for c in held.chars() {
                    self.print_glyph(c);
                }
            }
        }
    }

    /// Give up on a span: print the opening tag and everything after
    /// it exactly as it arrived.
    fn u_bail(&mut self) {
        *self.u_open = false;
        let held = std::mem::take(self.u_buf);
        for c in "<u>".chars() {
            self.print_glyph(c);
        }
        for c in held.chars() {
            self.print_glyph(c);
        }
    }

    /// Print anything still held as a possible `<u>`, unchanged.
    ///
    /// Called wherever the run of printable text ends — a control
    /// byte, an escape sequence — because a tag cannot span one.
    #[inline(always)]
    pub(super) fn flush_u_buf(&mut self) {
        if *self.u_open || !self.u_buf.is_empty() {
            self.flush_held_u_buf();
        }
    }

    #[cold]
    fn flush_held_u_buf(&mut self) {
        if *self.u_open {
            self.u_bail();
            return;
        }
        let held = std::mem::take(self.u_buf);
        for c in held.chars() {
            self.print_glyph(c);
        }
    }

    pub(super) fn print_glyph(&mut self, ch: char) {
        // FAST CLASS — the boundary BEFORE one of these is
        // unconditional (UAX #29 has no rule joining Other+Other, and
        // the class is chosen so LV/LVT and emoji-presentation
        // pictographs behave the same way), so no segmenter is needed
        // to know this starts a new cluster.  Commit it now.
        // …but only while the cluster currently open is itself of
        // that class, or none is.  After a ZWJ or any other extender
        // the boundary is NOT unconditional — GB11 joins ZWJ to the
        // pictograph after it — so `👨 ZWJ 👩` has to reach the
        // segmenter even though `👩` is fast class on its own.
        if let Some(w) = fast_width(ch)
            && (self.cluster_buf.is_empty() || self.cluster_fast.is_some())
        {
            self.commit_cluster_head(ch, w, true);
            return;
        }
        // SLOW PATH — this codepoint may EXTEND what was just drawn
        // (`e` + ́ , ⚠ + VS16, 👨 + ZWJ + 👩, LV + jamo, क + virama),
        // and only the segmenter knows.
        self.resync_segmenter();
        if self.grapheme_cursor.step(ch) {
            // A boundary: its own cluster, committed at its own width.
            // Anything that widens it arrives later and amends it.
            let w = crate::grapheme::cluster_width(ch.encode_utf8(&mut [0u8; 4]));
            if w > 0 {
                self.commit_cluster_head(ch, w, false);
            } else {
                // A zero-width codepoint with a boundary before it has
                // nothing to attach to — it is not drawn, but it does
                // open a cluster the next codepoint may extend.
                self.cluster_buf.clear();
                self.cluster_buf.push(ch);
                *self.cluster_fast = None;
                *self.cluster_anchor = None;
            }
            return;
        }
        // No boundary: `ch` belongs to the cluster already on screen.
        self.cluster_buf.push(ch);
        *self.cluster_fast = None;
        let w = crate::grapheme::cluster_width(self.cluster_buf);
        self.widen_anchor(w);
        self.store_cluster_at_anchor();
    }

    /// The cluster on screen is now more than one codepoint, so the
    /// cell holding it has to hold all of it.
    ///
    /// Called every time a codepoint joins, which rewrites the same
    /// cell with a longer string each time — `e` then `é` then `é` with
    /// a second mark.  Each rewrite leaves the previous pool entry
    /// unreferenced, which is what the sweep is for; that is cheaper
    /// than trying to extend an entry in place, and clusters are rare
    /// enough that it does not matter (55 in forty million, measured).
    fn store_cluster_at_anchor(&mut self) {
        let Some((col, row, _)) = *self.cluster_anchor else {
            return;
        };
        if self.cluster_buf.chars().count() < 2 {
            return;
        }
        // Sweep before growing rather than on a timer: the pool is
        // only allowed to hold what the screen points at, and this is
        // the one place it grows.
        if self.grid.cluster_pool_len() > CLUSTER_POOL_SWEEP_AT {
            self.grid.sweep_clusters();
        }
        let attrs = if self.link == 0 { *self.attrs } else { self.attrs.with_link_id(self.link) };
        // The cell being replaced may already point at the newest pool
        // entry — this cluster, one codepoint shorter.  Say so, and it
        // is rewritten in place rather than pushed again.
        let prev = self.grid.cluster_index_at(col, row);
        let cell = {
            let buf = std::mem::take(self.cluster_buf);
            let cell = self.grid.cluster_cell_reusing(prev, &buf, attrs);
            *self.cluster_buf = buf;
            cell
        };
        if attrs.is_default() {
            self.grid.set_plain_cluster(col, row, cell);
        } else {
            self.grid.set_cell(col, row, cell);
        }
    }

    /// Draw `ch` as the first codepoint of a new cluster and remember
    /// where it went.
    /// `fast` says the head is of the class whose successors need no
    /// segmenter.  A head that reached here through the segmenter is
    /// NOT that, even when it would have qualified alone: what follows
    /// it may still join.
    fn commit_cluster_head(&mut self, ch: char, w: u8, fast: bool) {
        let at = self.write_glyph(ch, w);
        self.cluster_buf.clear();
        self.cluster_buf.push(ch);
        *self.cluster_fast = if fast { Some((ch, w)) } else { None };
        *self.cluster_anchor = Some((at.0, at.1, w));
        *self.seg_synced = false;
    }

    /// The cluster turned out to be wider than its first codepoint —
    /// `⚠` is one cell, `⚠️` is two.  Claim the cell after it and push
    /// the cursor along, but only when the glyph is still where it was
    /// left: anything that moved the cursor clears the anchor, and a
    /// glyph already at the right edge has nowhere to grow into (it
    /// keeps the width it was drawn at, which is what it looked like
    /// before this codepoint arrived).
    fn widen_anchor(&mut self, w: u8) {
        let Some((col, row, cur)) = *self.cluster_anchor else {
            return;
        };
        if w <= cur || w != 2 {
            return;
        }
        let cols = self.grid.cols();
        if col + 1 >= cols {
            return;
        }
        let linked = self.attrs.with_link_id(self.link);
        self.grid.set_char(col + 1, row, '\0', if self.link == 0 { self.attrs } else { &linked });
        if self.grid.cursor() == (col + 1, row) {
            self.grid.set_cursor(col + 2, row);
        }
        *self.cluster_anchor = Some((col, row, w));
    }
}
