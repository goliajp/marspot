//! `LayoutModal` — small centered modal where the user picks an
//! arbitrary `(cols, rows)` grid shape for the main-area session
//! grid.  Replaces the F3+1.x popup picker (which had 7 fixed
//! options: Single / SplitH / SplitV / Quad / SixH / SixV / Nine).
//!
//! V1 contents:
//! - title bar with close [×]
//! - "Columns" row: [-] N [+]
//! - "Rows"    row: [-] M [+]
//! - footer: "Total: N×M = X panes" + Apply button
//!
//! Modal carries no internal state — it's a frame layouter that
//! computes rects from `pending_cols` / `pending_rows` each frame.
//! Caller (CoreState / Marspot) owns the pending values, the
//! `layout_modal_open` flag, and applies on click.

use marspot_term::layout::Rect;
use super::modal_frame::{ModalFrame, ModalLayoutSpec};

/// Caps for cols / rows.  Practical maximum tied to readability —
/// a 6×6 grid at 1920×1080 leaves cells ~300×180 px which is
/// roughly the lower bound where a terminal stays usable.  Going
/// higher is allowed by the data path (Layout::build doesn't cap),
/// the modal just gates the UI affordance.
pub const GRID_MIN: usize = 1;
pub const GRID_MAX: usize = 6;

/// Modal hit-test result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutModalHit {
    /// Close [×] in title bar.
    Close,
    /// Inside the title bar (non-close) — caller may treat as
    /// drag-start (V2; V1 just swallows).
    TitleBar,
    ColsDec,
    ColsInc,
    RowsDec,
    RowsInc,
    /// Apply button — commit pending → grid_cols/grid_rows.
    Apply,
    /// Inside the modal frame but outside any control.  Swallow
    /// the click (so it doesn't fall through to the grid below).
    Frame,
}

pub struct LayoutModal {
    pub frame: Rect,
    pub title_bar: Rect,
    pub close_btn: Rect,
    pub cols_dec: Rect,
    pub cols_inc: Rect,
    pub cols_value: Rect,
    pub rows_dec: Rect,
    pub rows_inc: Rect,
    pub rows_value: Rect,
    pub apply_btn: Rect,
    /// Total = cols × rows footer text region.
    pub total_label: Rect,
    /// F3+3.3 — preview / drag area.  Holds the per-slot card
    /// rects in row-major order.  Length == pending_cols *
    /// pending_rows.  Caller drags cards to reorder which pane
    /// shows in which slot.
    pub cards: Vec<Rect>,
    /// Bounding rect of the card grid area.  Used to clip drag
    /// overlay + as the hit-test bounds.
    pub card_grid: Rect,
}

const MODAL_W_LOGICAL: f64 = 320.0;
/// Modal min height when card grid is tiny.  Modal stretches
/// vertically beyond this when the card grid needs more room.
const MODAL_MIN_H_LOGICAL: f64 = 280.0;
const TITLE_BAR_H_LOGICAL: f64 = 28.0;
const STEPPER_BTN_LOGICAL: f64 = 28.0;
const STEPPER_VALUE_W_LOGICAL: f64 = 56.0;
const APPLY_BTN_H_LOGICAL: f64 = 32.0;
const ROW_GAP_LOGICAL: f64 = 12.0;
const SIDE_PAD_LOGICAL: f64 = 16.0;
/// Card grid dims.  Card aspect ratio kept close to 4:3 so a
/// "wide" 5×2 grid reads differently from a "tall" 2×5.  Min
/// card edge picked so 3-char titles ("245") read at 1× scale.
const CARD_GAP_LOGICAL: f64 = 6.0;
const CARD_MIN_W_LOGICAL: f64 = 44.0;
const CARD_MIN_H_LOGICAL: f64 = 32.0;
const CARD_GRID_PAD_LOGICAL: f64 = 14.0;
/// Aspect-ratio target for one card (width / height) — picked
/// so the preview at default density matches the real grid's
/// roughly 4:3 cells.  Both dimensions still floor at min above.
const CARD_ASPECT: f64 = 4.0 / 3.0;

impl LayoutModal {
    /// Build modal geometry centered in the window.  `scale` is
    /// device pixel ratio; sizes above are logical pt and get
    /// multiplied here so the modal stays the same physical size
    /// regardless of display density.  `cols/rows` carry the
    /// PENDING grid shape (modal state) — the preview card grid
    /// matches those dims, not the committed ones.
    pub fn layout(
        window_w: f64,
        window_h: f64,
        scale: f64,
        top_obstruction: f64,
        cols: usize,
        rows: usize,
    ) -> Self {
        let title_h = TITLE_BAR_H_LOGICAL * scale;
        let side_pad = SIDE_PAD_LOGICAL * scale;
        let row_gap = ROW_GAP_LOGICAL * scale;
        let stepper_btn = STEPPER_BTN_LOGICAL * scale;
        let value_w = STEPPER_VALUE_W_LOGICAL * scale;
        let apply_h = APPLY_BTN_H_LOGICAL * scale;
        let card_gap = CARD_GAP_LOGICAL * scale;
        let card_grid_pad = CARD_GRID_PAD_LOGICAL * scale;
        // Compute the card grid first so the modal height can
        // grow to fit it.  Width comes from MODAL_W minus paddings;
        // card cell size = floor of (w - gaps) / cols, with a
        // CARD_ASPECT-driven height (floored at min).
        let modal_w = MODAL_W_LOGICAL * scale;
        let card_grid_w = modal_w - 2.0 * side_pad;
        let cards_in = cols.max(1);
        let rows_in = rows.max(1);
        let inner_card_w = (card_grid_w - (cards_in - 1) as f64 * card_gap)
            / cards_in as f64;
        let card_w = inner_card_w.max(CARD_MIN_W_LOGICAL * scale);
        let aspect_card_h = card_w / CARD_ASPECT;
        let card_h = aspect_card_h.max(CARD_MIN_H_LOGICAL * scale);
        let card_grid_inner_h = card_h * rows_in as f64
            + (rows_in - 1) as f64 * card_gap;
        let card_grid_h = card_grid_inner_h + 2.0 * card_grid_pad;
        // Body content: top pad + cols row + gap + rows row + gap
        //              + card grid + gap + total label + gap + apply.
        let body_h = side_pad
            + stepper_btn
            + row_gap
            + stepper_btn
            + row_gap
            + card_grid_h
            + row_gap
            + stepper_btn  // total label
            + row_gap
            + apply_h
            + side_pad;
        let modal_h = (title_h + body_h).max(MODAL_MIN_H_LOGICAL * scale);
        let frame = ModalFrame::layout(
            window_w,
            window_h,
            ModalLayoutSpec {
                default_w: modal_w,
                default_h: modal_h,
                title_bar_h: title_h,
                tab_strip_h: 0.0,
                maximized: false,
                max_w_ratio: 1.0,
                max_h_ratio: 1.0,
                minimized: false,
                with_tab_strip: false,
                pos_offset: (0.0, 0.0),
                top_obstruction,
            },
        );
        // Close [×] sits flush against title bar right edge.
        let close_size = title_h - 8.0 * scale;
        let close_btn = Rect {
            x: frame.title_bar.x + frame.title_bar.w - close_size - 6.0 * scale,
            y_top: frame.title_bar.y_top + (frame.title_bar.h - close_size) * 0.5,
            w: close_size,
            h: close_size,
        };
        // Body rows: stack within the body area.
        let body = frame.body;
        // Right-anchor the stepper cluster so the row reads
        // "Columns        [-] 3 [+]".  Label fills the slack on
        // the left (caller paints the label text, modal owns
        // hit rects only).
        let cluster_w = stepper_btn * 2.0 + value_w;
        let cluster_x = body.x + body.w - side_pad - cluster_w;
        // Row 1: Cols
        let row1_top = body.y_top + side_pad;
        let cols_dec = Rect {
            x: cluster_x, y_top: row1_top, w: stepper_btn, h: stepper_btn,
        };
        let cols_value = Rect {
            x: cluster_x + stepper_btn, y_top: row1_top, w: value_w, h: stepper_btn,
        };
        let cols_inc = Rect {
            x: cluster_x + stepper_btn + value_w, y_top: row1_top, w: stepper_btn, h: stepper_btn,
        };
        // Row 2: Rows
        let row2_top = row1_top + stepper_btn + row_gap;
        let rows_dec = Rect {
            x: cluster_x, y_top: row2_top, w: stepper_btn, h: stepper_btn,
        };
        let rows_value = Rect {
            x: cluster_x + stepper_btn, y_top: row2_top, w: value_w, h: stepper_btn,
        };
        let rows_inc = Rect {
            x: cluster_x + stepper_btn + value_w, y_top: row2_top, w: stepper_btn, h: stepper_btn,
        };
        // Card grid: below row2, framed by a padded subgrid.
        let card_grid = Rect {
            x: body.x + side_pad,
            y_top: row2_top + stepper_btn + row_gap,
            w: card_grid_w,
            h: card_grid_h,
        };
        let mut cards: Vec<Rect> = Vec::with_capacity(cards_in * rows_in);
        for r in 0..rows_in {
            for c in 0..cards_in {
                let cx = card_grid.x + card_grid_pad
                    + c as f64 * (card_w + card_gap);
                let cy = card_grid.y_top + card_grid_pad
                    + r as f64 * (card_h + card_gap);
                cards.push(Rect {
                    x: cx, y_top: cy, w: card_w, h: card_h,
                });
            }
        }
        // Footer: total label + Apply button at the bottom.
        let apply_btn = Rect {
            x: body.x + side_pad,
            y_top: body.y_top + body.h - side_pad - apply_h,
            w: body.w - 2.0 * side_pad,
            h: apply_h,
        };
        // Total label band sits above the Apply button.
        let total_label = Rect {
            x: body.x + side_pad,
            y_top: apply_btn.y_top - row_gap - stepper_btn,
            w: body.w - 2.0 * side_pad,
            h: stepper_btn,
        };
        Self {
            frame: frame.frame,
            title_bar: frame.title_bar,
            cards,
            card_grid,
            close_btn,
            cols_dec,
            cols_inc,
            cols_value,
            rows_dec,
            rows_inc,
            rows_value,
            apply_btn,
            total_label,
        }
    }

    /// Index of the card whose rect contains `(px, py)`, or
    /// `None`.  Used by mouse_down to start a drag.
    pub fn hit_test_card(&self, px: f64, py: f64) -> Option<usize> {
        self.cards
            .iter()
            .position(|r| r.contains(px, py))
    }

    /// Card slot whose CENTER is closest to `(px, py)`.  Used
    /// during drag to highlight the drop target + on drop to
    /// settle the dragged card to its destination slot.  Never
    /// returns None when the modal has at least one card.
    pub fn nearest_card(&self, px: f64, py: f64) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (i, r) in self.cards.iter().enumerate() {
            let cx = r.x + r.w * 0.5;
            let cy = r.y_top + r.h * 0.5;
            let dx = cx - px;
            let dy = cy - py;
            let d2 = dx * dx + dy * dy;
            if best.map_or(true, |(_, bd)| d2 < bd) {
                best = Some((i, d2));
            }
        }
        best.map(|(i, _)| i)
    }

    pub fn hit_test(&self, px: f64, py: f64) -> Option<LayoutModalHit> {
        if !self.frame.contains(px, py) {
            return None;
        }
        if self.close_btn.contains(px, py) {
            return Some(LayoutModalHit::Close);
        }
        if self.apply_btn.contains(px, py) {
            return Some(LayoutModalHit::Apply);
        }
        if self.cols_dec.contains(px, py) {
            return Some(LayoutModalHit::ColsDec);
        }
        if self.cols_inc.contains(px, py) {
            return Some(LayoutModalHit::ColsInc);
        }
        if self.rows_dec.contains(px, py) {
            return Some(LayoutModalHit::RowsDec);
        }
        if self.rows_inc.contains(px, py) {
            return Some(LayoutModalHit::RowsInc);
        }
        if self.title_bar.contains(px, py) {
            return Some(LayoutModalHit::TitleBar);
        }
        Some(LayoutModalHit::Frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_centers_in_window() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3);
        // Frame should be roughly centered.
        let cx = m.frame.x + m.frame.w * 0.5;
        let cy = m.frame.y_top + m.frame.h * 0.5;
        assert!((cx - 960.0).abs() < 1.0);
        assert!((cy - 540.0).abs() < 1.0);
    }

    #[test]
    fn close_btn_is_inside_title_bar() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3);
        assert!(m.title_bar.contains(
            m.close_btn.x + 1.0,
            m.close_btn.y_top + 1.0,
        ));
    }

    #[test]
    fn hit_test_dispatches_close() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3);
        let cx = m.close_btn.x + m.close_btn.w * 0.5;
        let cy = m.close_btn.y_top + m.close_btn.h * 0.5;
        assert_eq!(m.hit_test(cx, cy), Some(LayoutModalHit::Close));
    }

    #[test]
    fn hit_test_dispatches_apply() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3);
        let cx = m.apply_btn.x + m.apply_btn.w * 0.5;
        let cy = m.apply_btn.y_top + m.apply_btn.h * 0.5;
        assert_eq!(m.hit_test(cx, cy), Some(LayoutModalHit::Apply));
    }

    #[test]
    fn outside_returns_none() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3);
        assert_eq!(m.hit_test(0.0, 0.0), None);
    }
}
