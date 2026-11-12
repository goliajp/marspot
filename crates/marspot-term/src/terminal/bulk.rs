//! `Terminal::feed`: predictions first, then the bytes — in bulk where
//! a whole run can be taken at once, one at a time through the state
//! machine everywhere else.

use super::{Handler, Terminal};
use crate::parser::{Parser, ParserCallbacks};
use std::time::Instant;

impl Terminal {
    /// `lanes == false` is the reference path: every byte goes through
    /// the parser's state machine and every character through `print`.
    pub(super) fn feed_with(&mut self, bytes: &[u8], lanes: bool) {
        if bytes.is_empty() {
            return;
        }
        // Once per chunk, not once per character.  The pane's own
        // declaration is the normal route; the setting is a manual
        // override for someone who wants it everywhere.
        self.u_tags = self.render_markup || crate::settings::get().render_u_tags;
        // Bumped once per non-empty feed: the snapshot pump compares it
        // against the last generation it pushed.
        self.generation = self.generation.saturating_add(1);
        let mut i = 0;
        while i < bytes.len() {
            // Bulk program output has neither queue populated, and
            // nothing in a feed can populate them, so the rest of the
            // chunk goes through in one piece.
            if self.predictions.is_empty() && self.shadow.is_empty() {
                let (parser, mut handler) = self.parts();
                if lanes {
                    feed_bulk(parser, &mut handler, &bytes[i..]);
                } else {
                    for &b in &bytes[i..] {
                        parser.advance(&mut handler, b);
                    }
                }
                break;
            }
            // A byte owed to an expired prediction settles the
            // question that expiry deliberately left open.  Matching
            // means the program does echo, just later than we waited
            // — widen the window and keep predicting.  Anything else
            // means it was never going to, which is a real miss.  The
            // byte itself is fed normally either way: its cell was
            // unpainted when the prediction expired.
            let mut owed = false;
            if let Some(&(b, at)) = self.shadow.front() {
                if bytes[i] == b {
                    self.shadow.pop_front();
                    self.note_echo_rtt(Instant::now().duration_since(at));
                    self.predict_misses = 0;
                    owed = true;
                } else {
                    self.predict_misses = self.predict_misses.saturating_add(1);
                    self.shadow.clear();
                }
            }
            // Validate against pending predictions before the parser
            // sees the byte.
            if let Some(p) = self.predictions.front() {
                if !owed && bytes[i] == p.byte {
                    let at = p.at;
                    self.predictions.pop_front();
                    self.predictions_hit += 1;
                    self.note_echo_rtt(Instant::now().duration_since(at));
                    // The program echoes at the cursor after all.
                    self.predict_misses = 0;
                    i += 1;
                    continue;
                }
                if !owed {
                    self.predict_misses = self.predict_misses.saturating_add(1);
                    self.rollback_predictions();
                }
            }
            // While either queue holds something every byte is looked
            // at on its own: that comparison is what settles them.
            let (parser, mut handler) = self.parts();
            parser.advance(&mut handler, bytes[i]);
            i += 1;

            // Entering alt-screen mid-feed swaps the grid wholesale —
            // any predictions queued beforehand were anchored to the
            // old grid and are now garbage.  Drop them.
            if !self.predictions.is_empty() && self.saved_main.is_some() {
                let n = self.predictions.len();
                self.predictions.clear();
                self.shadow.clear();
                self.predictions_miss += n as u64;
            }
        }
        // Nothing is flushed at the end of a feed.  A glyph is
        // committed the moment its width is known, and the open
        // cluster and its anchor survive the boundary: a pty ends its
        // reads wherever the kernel had a break, and the codepoint
        // that extends a cluster may well be the first byte of the
        // next one (measured 2026-09-07: `a⚠️b` split after `⚠`).
    }

    /// The parser and everything else it drives, as two borrows.
    fn parts(&mut self) -> (&mut Parser, Handler<'_>) {
        let handler = Handler {
            grid: &mut self.grid,
            saved_main: &mut self.saved_main,
            attrs: &mut self.attrs,
            saved_cursor: &mut self.saved_cursor,
            parked_saved_cursor: &mut self.parked_saved_cursor,
            scroll_top: &mut self.scroll_top,
            scroll_bot: &mut self.scroll_bot,
            pending_response: &mut self.pending_response,
            response_window: &mut self.response_window,
            response_burst_since: &mut self.response_burst_since,
            response_burst_last_warn: &mut self.response_burst_last_warn,
            cursor_key_app_mode: &mut self.cursor_key_application_mode,
            bracketed_paste: &mut self.bracketed_paste_mode,
            sync_output: &mut self.sync_output,
            uses_sync_output: &mut self.uses_sync_output,
            focus_reporting: &mut self.focus_reporting,
            focus_reported: &mut self.focus_reported,
            osc_title: &mut self.osc_title,
            cursor_shape: &mut self.cursor_shape,
            alt_scroll: &mut self.alt_scroll,
            u_tags: self.u_tags,
            u_buf: &mut self.u_buf,
            u_open: &mut self.u_open,
            cursor_visible: &mut self.cursor_visible,
            kitty_keyboard: &mut self.kitty_keyboard,
            mouse_tracking_mode: &mut self.mouse_tracking_mode,
            mouse_sgr_encoding: &mut self.mouse_sgr_encoding,
            pending_wrap: &mut self.pending_wrap,
            tabs: &mut self.tabs,
            osc_clipboard: &mut self.osc_clipboard,
            g0_graphics: &mut self.g0_graphics,
            origin_mode: &mut self.origin_mode,
            insert_mode: &mut self.insert_mode,
            cluster_buf: &mut self.cluster_buf,
            cluster_fast: &mut self.cluster_fast,
            cluster_anchor: &mut self.cluster_anchor,
            grapheme_cursor: &mut self.grapheme_cursor,
            seg_synced: &mut self.seg_synced,
        };
        (&mut self.parser, handler)
    }
}

/// Feed `bytes` with no prediction pending.
///
/// In plain Ground three shapes are taken whole instead of a byte at a
/// time, each doing exactly what the state machine would have done
/// with the same bytes: a run of fast-class text, the parameter bytes
/// of a control sequence, and a C0 control.  Everything else, and
/// whatever those leave unconsumed, goes through `advance`.
fn feed_bulk(parser: &mut Parser, handler: &mut Handler<'_>, bytes: &[u8]) {
    let mut i = 0;
    while i < bytes.len() {
        let b0 = bytes[i];
        if parser.in_ground_plain() {
            if b0 >= 0x20 {
                let n = handler.text_lane(&bytes[i..]);
                if n > 0 {
                    i += n;
                    continue;
                }
            } else if b0 == 0x1B {
                if bytes.get(i + 1) == Some(&b'[') {
                    i += parser.csi_slice(handler, &bytes[i..]);
                    continue;
                }
            } else if b0 != 0x18 && b0 != 0x1A {
                handler.execute(b0);
                i += 1;
                continue;
            }
        }
        parser.advance(handler, b0);
        i += 1;
    }
}
