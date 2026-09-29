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

use marspot_term::layout::{Rect, Alignment};
use super::modal_frame::{ModalFrame, ModalLayoutSpec};

/// Caps for cols / rows.
///
/// The data path never capped this — `Layout::build` takes any shape —
/// so the number here is only how far the modal's steppers will go.
/// It was 6, chosen against a 1920×1080 window where 6×6 leaves cells
/// around 300×180 px.  Raised to 9 by request: the displays this runs
/// on are 4K and larger, where 9 columns still leaves a cell wider
/// than most terminals ever get.
///
/// `SESSION_COUNT_HARD_CAP` is kept at `GRID_MAX²` so the modal can
/// never offer a shape the app would refuse to fill.  Note that a
/// grid is *slots*, not sessions: a 9×9 window with three panes has
/// three processes and 78 empty seats.
pub const GRID_MIN: usize = 1;
pub const GRID_MAX: usize = 9;

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

// F3+3.7 — bumped modal width + min card sizes so the preview can
// host cwd-basename titles (which may run 10–20 chars: `claudecode`,
// `lab31-luna`, `marspot`-style).  Old 320 pt × 44 pt cards clipped
// anything past ~6 chars at 1× scale.
const MODAL_W_LOGICAL: f64 = 440.0;
/// Modal min height when card grid is tiny.  Modal stretches
/// vertically beyond this when the card grid needs more room.
const MODAL_MIN_H_LOGICAL: f64 = 380.0;
const TITLE_BAR_H_LOGICAL: f64 = 28.0;
/// Title bar height as a multiple of the title's own line — the
/// breathing room a bar needs around the text it holds.
const TITLE_BAR_LEADING: f64 = 1.9;
const STEPPER_BTN_LOGICAL: f64 = 28.0;
const STEPPER_VALUE_W_LOGICAL: f64 = 56.0;
const APPLY_BTN_H_LOGICAL: f64 = 32.0;
const ROW_GAP_LOGICAL: f64 = 12.0;
const SIDE_PAD_LOGICAL: f64 = 16.0;
/// Card grid dims.  Cards run flat (wider than tall) so the
/// preview reads as "list of slots" rather than "miniature
/// terminals" — a tall 1:1 card invited the comparison and
/// always lost it.  Min card edge sized for ~9–10 chars at 1×
/// scale (typical workdir basename).
const CARD_GAP_LOGICAL: f64 = 8.0;
const CARD_MIN_W_LOGICAL: f64 = 80.0;
/// How much of the window the modal may take when its card block
/// needs more than the ideal width.
const MODAL_MAX_W_RATIO: f64 = 0.92;
/// Space between a card's label and its edge, each side.
///
/// Public because the painter's ellipsis budget must use the *same*
/// number: sizing the card with one padding and cutting the text with
/// another means a name that was measured to fit still gets an
/// ellipsis.
pub const CARD_LABEL_PAD_LOGICAL: f64 = 10.0;
const CARD_MIN_H_LOGICAL: f64 = 44.0;
/// Aspect-ratio target for one card (width / height).  2.4 ≈
/// 12:5 keeps cards readably flat across the cols range
/// (3-col → ~55 pt tall, 1-col → ~170 pt tall — still capped
/// by `CARD_MAX_H_LOGICAL` below so the 1-col case doesn't
/// turn into a huge banner).
const CARD_ASPECT: f64 = 2.4;
/// Upper bound on card height — keeps the 1-col / 2-col
/// preview from blowing up into a wall of card.  At default
/// density the body width / 2 (≈ 200 pt) × 1/CARD_ASPECT
/// already ≈ 83 pt, so this cap only kicks in for cols=1.
const CARD_MAX_H_LOGICAL: f64 = 96.0;

impl LayoutModal {
    /// Build modal geometry centered in the window.  `scale` is
    /// device pixel ratio; sizes above are logical pt and get
    /// multiplied here so the modal stays the same physical size
    /// regardless of display density.  `cols/rows` carry the
    /// PENDING grid shape (modal state) — the preview card grid
    /// matches those dims, not the committed ones.
    /// `label_w` is the width the widest card label needs, in
    /// physical px — pass `0.0` when the caller has no labels (the
    /// hit-test paths, which only need the rects).
    ///
    /// A card is a button with a project name on it, so the name is
    /// what decides how wide it wants to be.  Sizing to a constant and
    /// letting the name run over was the 2026-08-11 report; sizing to
    /// the name means `lab38-golialab` is readable whenever the window
    /// can hold it, and only gets an ellipsis when it truly cannot.
    pub fn layout(
        window_w: f64,
        window_h: f64,
        scale: f64,
        top_obstruction: f64,
        cols: usize,
        rows: usize,
        label_w: f64,
    ) -> Self {
        // The bar has to hold its own title.  28 pt was picked when
        // the title was drawn in the terminal cell font; a panel title
        // is now set in `PanelText::Title`, whose cap alone is 19 px
        // on a display where the bar is 28 — so the title filled the
        // bar edge to edge and read as a banner (2026-08-11).  Chrome
        // that holds text is sized from the text.
        let title_type = crate::ui::theme::PanelText::Title;
        let title_line_px = (title_type.cap() + title_type.descent())
            * crate::ui::core::ViewPainter::px_per_pt();
        let title_h = (TITLE_BAR_H_LOGICAL * scale).max(title_line_px * TITLE_BAR_LEADING);
        let side_pad = SIDE_PAD_LOGICAL * scale;
        let row_gap = ROW_GAP_LOGICAL * scale;
        let stepper_btn = STEPPER_BTN_LOGICAL * scale;
        let value_w = STEPPER_VALUE_W_LOGICAL * scale;
        let apply_h = APPLY_BTN_H_LOGICAL * scale;
        let card_gap = CARD_GAP_LOGICAL * scale;
        // F3+3.7 — modal sizes itself from the card block intrinsic
        // dims, then over-rounds up to MODAL_MIN.  Any leftover
        // vertical space lands as flex around the card block (the
        // block sits Center within the region between rows-stepper
        // and total-label), so a small grid (e.g. 1×1) doesn't push
        // the steppers + Apply to opposite ends of a near-empty
        // modal.  Width: cards fill body width by default; min card
        // size is the lower bound so tiny grids still get readable
        // cells.
        let cards_in = cols.max(1);
        let rows_in = rows.max(1);
        // Width: the modal is as wide as its widest row of cards, not
        // a constant the cards then overflow.
        //
        // It used to take `MODAL_W_LOGICAL` and divide what was left
        // among the columns — but with a floor of `CARD_MIN_W`, so at
        // six columns the block came out 520 pt inside a 408 pt body
        // and simply drew past the modal's own edge (2026-08-11
        // screenshot: the last column half outside the frame).  A
        // minimum that a container does not grow to honour is not a
        // minimum, it is an overflow.
        let ideal_w = MODAL_W_LOGICAL * scale;
        // What one card wants: never below the readable minimum, and
        // enough for the longest name plus its padding.
        let want_card_w = (CARD_MIN_W_LOGICAL * scale)
            .max(label_w + 2.0 * CARD_LABEL_PAD_LOGICAL * scale);
        let min_block_w = cards_in as f64 * want_card_w
            + (cards_in - 1) as f64 * card_gap;
        let modal_w = (min_block_w + 2.0 * side_pad)
            .max(ideal_w)
            // …but never wider than the window will hold.
            .min(window_w * MODAL_MAX_W_RATIO);
        let body_w = modal_w - 2.0 * side_pad;
        let natural_card_w = (body_w - (cards_in - 1) as f64 * card_gap)
            / cards_in as f64;
        // No floor here on purpose.  The modal already grew to honour
        // `CARD_MIN_W` whenever the window allowed it, so on a window
        // with room `natural_card_w` is *already* at or above the
        // minimum.  When the window did not allow it the modal was
        // clamped — and re-asserting the minimum then would put the
        // block back outside the frame it was just fitted into, which
        // is the overflow this was supposed to end (caught at 6×6 on a
        // 900 pt window at scale 2 by the density test below).  A
        // window too narrow for readable cards gets narrow cards; the
        // labels ellipsise.
        let card_w = natural_card_w.max(1.0);
        let aspect_card_h = card_w / CARD_ASPECT;
        let ideal_card_h = aspect_card_h
            .max(CARD_MIN_H_LOGICAL * scale)
            .min(CARD_MAX_H_LOGICAL * scale);
        let card_block_w = cards_in as f64 * card_w
            + (cards_in - 1) as f64 * card_gap;
        let ideal_card_block_h = rows_in as f64 * ideal_card_h
            + (rows_in - 1) as f64 * card_gap;
        // Body content: top pad + cols row + gap + rows row + gap
        //              + card block + gap + total label + gap + apply.
        // Split into fixed (non-card) part + card block — we'll need
        // the fixed part again after `ModalFrame` clamps the frame.
        let fixed_body_h = 2.0 * side_pad
            + 3.0 * stepper_btn
            + 4.0 * row_gap
            + apply_h;
        let body_h = fixed_body_h + ideal_card_block_h;
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
        // F3+3.7-fix — `ModalFrame::layout` clamps default_h to
        // `window_h * 0.95`, so the frame we actually got back may
        // be shorter than what we asked for.  All the squish lands
        // on the card region (apply is bottom-anchored, steppers are
        // top-anchored).  Re-derive card_h from the frame's real
        // body.h so the card block always fits — bottom border stays
        // visible instead of overflowing into the total/apply band.
        let actual_card_block_h = (frame.body.h - fixed_body_h).max(0.0);
        let card_h = if actual_card_block_h < ideal_card_block_h {
            ((actual_card_block_h - (rows_in - 1) as f64 * card_gap)
                / rows_in as f64)
                .max(1.0)
        } else {
            ideal_card_h
        };
        let card_block_h = rows_in as f64 * card_h
            + (rows_in - 1) as f64 * card_gap;
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
        // Footer first (pinned to body bottom) so the card region
        // can be computed against it.
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
        // F3+3.7 — card region = vertical band between the rows
        // stepper bottom and the total label top, full body width.
        // The card block (cards_in × rows_in cards with gaps) sits
        // Center-aligned inside this region so any flex space lands
        // evenly around the cards rather than stacking at the
        // bottom.
        let region_top = row2_top + stepper_btn + row_gap;
        let region_bottom = total_label.y_top - row_gap;
        let card_region = Rect {
            x: body.x + side_pad,
            y_top: region_top,
            w: body_w,
            h: (region_bottom - region_top).max(0.0),
        };
        let card_grid = card_region.place(
            card_block_w, card_block_h, Alignment::Center,
        );
        let mut cards: Vec<Rect> = Vec::with_capacity(cards_in * rows_in);
        for r in 0..rows_in {
            for c in 0..cards_in {
                let cx = card_grid.x + c as f64 * (card_w + card_gap);
                let cy = card_grid.y_top + r as f64 * (card_h + card_gap);
                cards.push(Rect {
                    x: cx, y_top: cy, w: card_w, h: card_h,
                });
            }
        }
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
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3, 0.0);
        // Frame should be roughly centered.
        let cx = m.frame.x + m.frame.w * 0.5;
        let cy = m.frame.y_top + m.frame.h * 0.5;
        assert!((cx - 960.0).abs() < 1.0);
        assert!((cy - 540.0).abs() < 1.0);
    }

    #[test]
    fn close_btn_is_inside_title_bar() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3, 0.0);
        assert!(m.title_bar.contains(
            m.close_btn.x + 1.0,
            m.close_btn.y_top + 1.0,
        ));
    }

    #[test]
    fn hit_test_dispatches_close() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3, 0.0);
        let cx = m.close_btn.x + m.close_btn.w * 0.5;
        let cy = m.close_btn.y_top + m.close_btn.h * 0.5;
        assert_eq!(m.hit_test(cx, cy), Some(LayoutModalHit::Close));
    }

    #[test]
    fn hit_test_dispatches_apply() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3, 0.0);
        let cx = m.apply_btn.x + m.apply_btn.w * 0.5;
        let cy = m.apply_btn.y_top + m.apply_btn.h * 0.5;
        assert_eq!(m.hit_test(cx, cy), Some(LayoutModalHit::Apply));
    }

    #[test]
    fn outside_returns_none() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 3, 0.0);
        assert_eq!(m.hit_test(0.0, 0.0), None);
    }

    /// Regression — F3+3.7 bug: rows=4 in a short window made the
    /// modal_h request exceed `window_h * 0.95`, ModalFrame clamped,
    /// and the card block overflowed past the total label so the
    /// last row's bottom border was hidden.  After fix card_h
    /// shrinks to fit; last card's bottom must stay clear of the
    /// total label rect.
    #[test]
    fn last_row_bottom_clears_total_label_when_window_squishes_modal() {
        // 660pt is a typical-ish short window; 3×4 modal at scale=1
        // wants ~640pt, which gets clamped to 660 * 0.95 = 627.
        let m = LayoutModal::layout(800.0, 660.0, 1.0, 0.0, 3, 4, 0.0);
        let last = m.cards.last().expect("4 rows × 3 cols → 12 cards");
        let last_bottom = last.y_top + last.h;
        assert!(
            last_bottom <= m.total_label.y_top + 0.5,
            "last card bottom {} must not overflow into total label \
             (y_top={})",
            last_bottom, m.total_label.y_top,
        );
    }

    /// And the inverse case — when the modal has plenty of room
    /// (1080pt window), card_h must NOT shrink, so it keeps the
    /// nice 4:3 aspect.
    #[test]
    fn card_h_keeps_ideal_size_when_window_has_room() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 3, 4, 0.0);
        let card = m.cards.first().expect("≥ 1 card");
        // Ideal: card_w / CARD_ASPECT, card_w ≈ (440-32-16)/3 ≈
        // 130.67, CARD_ASPECT 2.4 → card_h ≈ 54.4.  Within [MIN=44,
        // MAX=96] so neither clamp kicks in.
        assert!(
            (card.h - 54.44).abs() < 0.5,
            "expected ideal card_h ≈ 54pt (flat), got {}", card.h,
        );
    }

    /// 1-col case used to be a tall banner (body_w / CARD_ASPECT
    /// ≈ 170pt).  The MAX cap clamps it to 96pt.
    /// Cards live **inside** the modal, at every grid shape and every
    /// label length.
    ///
    /// 2026-08-11 report: at six columns the block came out 520 pt in
    /// a 408 pt body and drew straight over the frame's right edge,
    /// and long project names (`lab38-golialab`) ran past their own
    /// cards on top of that.  A minimum a container does not grow to
    /// honour is not a minimum.
    #[test]
    fn the_card_block_never_escapes_the_modal() {
        for (win_w, win_h) in [(2160.0, 1300.0), (900.0, 700.0), (600.0, 500.0)] {
            for cols in GRID_MIN..=GRID_MAX {
                for rows in GRID_MIN..=GRID_MAX {
                    // 0 pt (no labels) through a long project name.
                    for label_w in [0.0, 60.0, 101.0, 400.0] {
                        let m = LayoutModal::layout(
                            win_w, win_h, 1.0, 0.0, cols, rows, label_w,
                        );
                        let f = m.frame;
                        assert!(
                            f.w <= win_w + 1e-6 && f.h <= win_h + 1e-6,
                            "{cols}x{rows} @{win_w}x{win_h} label={label_w}: \
                             modal {}x{} is bigger than the window",
                            f.w, f.h,
                        );
                        for (i, c) in m.cards.iter().enumerate() {
                            assert!(
                                c.x >= f.x - 1e-6
                                    && c.x + c.w <= f.x + f.w + 1e-6,
                                "{cols}x{rows} @{win_w}x{win_h} label={label_w}: \
                                 card {i} spans {}..{} outside {}..{}",
                                c.x, c.x + c.w, f.x, f.x + f.w,
                            );
                            assert!(c.w > 0.0 && c.h > 0.0, "card {i} has no area");
                        }
                    }
                }
            }
        }
    }

    /// A longer name buys a wider card — until the window says no.
    #[test]
    fn a_longer_label_widens_the_card_up_to_the_window() {
        let narrow = LayoutModal::layout(2160.0, 1300.0, 1.0, 0.0, 3, 2, 40.0);
        let wide = LayoutModal::layout(2160.0, 1300.0, 1.0, 0.0, 3, 2, 300.0);
        assert!(
            wide.cards[0].w > narrow.cards[0].w,
            "a 300 pt label must get a wider card than a 40 pt one",
        );
        // …and on a window that cannot hold it, the modal stops at the
        // window rather than the label.
        let capped = LayoutModal::layout(500.0, 700.0, 1.0, 0.0, 6, 2, 300.0);
        assert!(capped.frame.w <= 500.0 * MODAL_MAX_W_RATIO + 1e-6);
    }

    /// The card block stays inside its modal at **both** densities.
    ///
    /// `scale` reaches this module as a parameter, so a caller that
    /// passes the display's number and a caller that passes 1 both
    /// have to work — and the labels arrive in physical pixels, which
    /// scale independently of it.  That pairing is where a modal
    /// stops holding its own cards.
    #[test]
    fn the_modal_holds_its_cards_at_every_density() {
        for scale in [1.0f64, 2.0] {
            for (win_w, win_h) in [(2160.0 * scale, 1300.0 * scale), (900.0, 700.0)] {
                for (cols, rows) in [(1, 1), (3, 2), (GRID_MAX, GRID_MAX)] {
                    for label_w in [0.0, 101.0 * scale, 400.0] {
                        let m = LayoutModal::layout(
                            win_w, win_h, scale, 0.0, cols, rows, label_w,
                        );
                        let f = m.frame;
                        assert!(
                            f.w <= win_w + 1e-6 && f.h <= win_h + 1e-6,
                            "scale {scale} {cols}x{rows} @{win_w}x{win_h}: modal \
                             {:.0}x{:.0} bigger than the window",
                            f.w, f.h,
                        );
                        for (i, c) in m.cards.iter().enumerate() {
                            assert!(
                                c.x >= f.x - 1e-6 && c.x + c.w <= f.x + f.w + 1e-6,
                                "scale {scale} {cols}x{rows}: card {i} outside the modal",
                            );
                            assert!(c.w > 0.0 && c.h > 0.0, "card {i} has no area");
                        }
                        assert!(
                            m.apply_btn.y_top + m.apply_btn.h <= f.y_top + f.h + 1e-6,
                            "scale {scale}: Apply fell out of the modal",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn card_h_capped_at_max_for_single_column() {
        let m = LayoutModal::layout(1920.0, 1080.0, 1.0, 0.0, 1, 4, 0.0);
        let card = m.cards.first().expect("≥ 1 card");
        assert!(
            (card.h - 96.0).abs() < 0.5,
            "expected cap at 96pt, got {}", card.h,
        );
    }
}
