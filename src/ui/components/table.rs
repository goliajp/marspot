//! A table of data on a card: a muted header row, a line under it,
//! and rows of text in the terminal's own font.
//!
//! Tabular data is measured in terminal cells (`spec::table`), so a
//! column is a whole number of characters wide and its numbers line up
//! without measuring anything.
//!
//! [`TableGeometry`] is where everything is — built with no painter,
//! so a click handler holds the same rects the painter draws — and its
//! `paint_*` methods draw what it describes.  The caller owns the data,
//! the selection and the scroll offset; the body scrolls and the header
//! does not.
//!
//! There is no scissor in the UI paint path, so a row is laid out only
//! if it fits the body whole: a row that straddles the card's edge
//! would be drawn onto whatever the card sits on.
//!
//! All rects are physical pixels.

use marspot_term::layout::{Alignment, Rect};

use crate::ui::components::panel_chrome;
use crate::ui::components::panel_palette as palette;
use crate::ui::core::text_fit::fit_mono;
use crate::ui::core::ViewPainter;
use crate::ui::theme::spec::{self, card as card_spec, space, table as t};

/// How wide a column is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    /// This many characters.
    Cells(f64),
    /// Physical pixels — a column that holds a control, not text.
    Px(f64),
    /// What the other columns leave, shared equally between the flex
    /// columns.
    Flex,
}

/// Names go left, numbers go right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub header: &'static str,
    pub width: ColumnWidth,
    pub align: Align,
    /// Set on the column the rows are ordered by.  The caller sorts;
    /// the header shows which way.
    pub sort: Option<SortDir>,
}

/// Clear space between the header's line and the first row, and under
/// the last row: the selected row's fill touches neither.
fn body_pad() -> f64 {
    spec::px(space::S4)
}

/// Height of a data row, on whole pixels: a fractional row height
/// puts every other row's text a pixel off the rhythm of the rest.
pub fn row_height(cell_h: f64) -> f64 {
    spec::snap(t::row_h(cell_h))
}

/// Height of the header row, on whole pixels.
pub fn header_height(cell_h: f64) -> f64 {
    spec::snap(t::header_h(cell_h))
}

/// Height of the card that shows `n_rows` rows without scrolling.
pub fn card_height(n_rows: usize, cell_h: f64) -> f64 {
    header_height(cell_h) + card_spec::SEPARATOR_PX + 2.0 * body_pad() + n_rows as f64 * row_height(cell_h)
}

/// Width of the card that gives every column its width, a flex column
/// counting as `flex_cells` characters.
pub fn natural_width(columns: &[Column], flex_cells: f64, cell_w: f64) -> f64 {
    let cols: f64 = columns
        .iter()
        .map(|c| match c.width {
            ColumnWidth::Cells(n) => cells_of(c, n) * cell_w,
            ColumnWidth::Px(w) => w,
            ColumnWidth::Flex => flex_cells * cell_w,
        })
        .sum();
    let gaps = columns.len().saturating_sub(1) as f64 * t::COLUMN_GAP_CELLS * cell_w;
    cols + gaps + 2.0 * spec::px(card_spec::ROW_PAD_X)
}

/// Where a table's parts are.
#[derive(Debug, Clone)]
pub struct TableGeometry {
    /// The card the table is drawn on.
    pub card: Rect,
    /// The header row, across the top of the card.
    pub header: Rect,
    /// Top edge of the line under the header.
    pub separator_y: f64,
    /// Where rows may be.
    pub body: Rect,
    /// `(x, width)` of each column's text.
    pub columns: Vec<(f64, f64)>,
    /// `(index, rect)` of every row that fits the body whole, top to
    /// bottom.  A row's rect spans the card's width.
    pub rows: Vec<(usize, Rect)>,
    /// The scroll offset in force: the one asked for, held to what the
    /// rows allow.
    pub scroll: f64,
    /// How far the rows can be pushed up before the last one is at the
    /// body's bottom.
    pub max_scroll: f64,
    pub cell_w: f64,
}

impl TableGeometry {
    pub fn new(
        card: Rect,
        columns: &[Column],
        n_rows: usize,
        scroll: f64,
        cell_w: f64,
        cell_h: f64,
    ) -> Self {
        let header = Rect { h: header_height(cell_h).min(card.h), ..card };
        let separator_y = header.y_top + header.h;
        let body_top = separator_y + card_spec::SEPARATOR_PX + body_pad();
        let body_bottom = card.y_top + card.h - body_pad();
        let body = Rect { y_top: body_top, h: (body_bottom - body_top).max(0.0), ..card };

        let row_h = row_height(cell_h);
        // A card sized for exactly its rows comes out a rounding error
        // short of them; that is not something to scroll by.
        let overflow = n_rows as f64 * row_h - body.h;
        let max_scroll = if overflow > 1e-6 { overflow } else { 0.0 };
        let scroll = scroll.clamp(0.0, max_scroll);
        let rows = (0..n_rows)
            .map(|i| (i, Rect { y_top: body.y_top + i as f64 * row_h - scroll, h: row_h, ..card }))
            .filter(|(_, r)| r.y_top >= body.y_top - 1e-9 && r.y_top + r.h <= body_bottom + 1e-9)
            .collect();

        Self {
            card,
            header,
            separator_y,
            body,
            columns: column_spans(card, columns, cell_w),
            rows,
            scroll,
            max_scroll,
            cell_w,
        }
    }

    /// The row `(x, y)` is on.  Rows that are not laid out are not
    /// there to be clicked.
    pub fn row_at(&self, x: f64, y: f64) -> Option<usize> {
        self.rows.iter().find(|(_, r)| r.contains(x, y)).map(|(i, _)| *i)
    }

    /// The fill behind a selected or hovered `row`: short of the
    /// card's edges on both sides.
    pub fn highlight_rect(&self, row: Rect) -> Rect {
        let inset = spec::px(t::SELECTED_INSET);
        Rect { x: row.x + inset, w: (row.w - 2.0 * inset).max(0.0), ..row }
    }

    /// Where `text` goes in column `col` of `row`, `indent` tree
    /// levels in, and what is left of it once it is cut to fit: a cell
    /// never runs into the next column.
    pub fn cell_text(&self, row: Rect, col: usize, indent: u8, text: &str) -> (Rect, String) {
        let (x, w) = self.columns[col];
        let shift = (indent as f64 * t::INDENT_CELLS * self.cell_w).min(w);
        let rect = Rect { x: x + shift, y_top: row.y_top, w: w - shift, h: row.h };
        (rect, fit_mono(text, rect.w, self.cell_w, 0.0))
    }

    /// The card, the header's labels and the line under them.
    pub fn paint_frame(&self, p: &mut ViewPainter, columns: &[Column]) {
        panel_chrome::card(p, self.card);
        for (col, c) in columns.iter().enumerate() {
            // The arrow goes on the side away from the edge the column
            // is aligned to, so the label still lines up with its data.
            let label = match (c.sort, c.align) {
                (None, _) => c.header.to_string(),
                (Some(dir), Align::Left) => format!("{} {}", c.header, arrow(dir)),
                (Some(dir), Align::Right) => format!("{} {}", arrow(dir), c.header),
            };
            self.paint_cell(p, self.header, col, c.align, 0, &label, palette::fg_muted());
        }
        panel_chrome::row_separator(p, self.card, self.separator_y);
    }

    /// The fill behind `row`, in `color`.
    pub fn paint_highlight(&self, p: &mut ViewPainter, row: Rect, color: [f32; 4]) {
        p.fill_rounded_rect(
            self.highlight_rect(row),
            color,
            spec::px(t::SELECTED_RADIUS) as f32,
            (palette::none(), 0.0),
        );
    }

    /// One cell's text, vertically centred in `row`.
    pub fn paint_cell(
        &self,
        p: &mut ViewPainter,
        row: Rect,
        col: usize,
        align: Align,
        indent: u8,
        text: &str,
        color: [f32; 4],
    ) {
        let (rect, text) = self.cell_text(row, col, indent, text);
        let anchor = match align {
            Align::Left => Alignment::CenterLeft,
            Align::Right => Alignment::CenterRight,
        };
        p.text_in(rect, &text, color, anchor);
    }
}

/// The cells a column asked for, or what its header needs when that
/// is more: a translated header is not cut to the width of the English
/// one.
fn cells_of(c: &Column, asked: f64) -> f64 {
    let header = crate::ui::core::text_fit::cells(c.header) + if c.sort.is_some() { 2 } else { 0 };
    asked.max(header as f64)
}

fn arrow(dir: SortDir) -> char {
    match dir {
        SortDir::Asc => '▲',
        SortDir::Desc => '▼',
    }
}

/// `(x, width)` of each column inside `card`'s row padding.  Columns
/// that ask for more than the card has shrink in proportion, gaps
/// included, so the last one still ends inside the card.
fn column_spans(card: Rect, columns: &[Column], cell_w: f64) -> Vec<(f64, f64)> {
    let pad = spec::px(card_spec::ROW_PAD_X);
    let inner_w = (card.w - 2.0 * pad).max(0.0);
    let gaps = columns.len().saturating_sub(1) as f64 * t::COLUMN_GAP_CELLS * cell_w;
    let fixed: f64 = columns
        .iter()
        .map(|c| match c.width {
            ColumnWidth::Cells(n) => cells_of(c, n) * cell_w,
            ColumnWidth::Px(w) => w,
            ColumnWidth::Flex => 0.0,
        })
        .sum();
    let n_flex = columns.iter().filter(|c| c.width == ColumnWidth::Flex).count();
    let wanted = fixed + gaps;
    let squeeze = if wanted > inner_w && wanted > 0.0 { inner_w / wanted } else { 1.0 };
    let flex_w = if n_flex > 0 { (inner_w - wanted).max(0.0) / n_flex as f64 } else { 0.0 };
    let gap = t::COLUMN_GAP_CELLS * cell_w * squeeze;

    let mut x = card.x + pad;
    columns
        .iter()
        .map(|c| {
            let w = match c.width {
                ColumnWidth::Cells(n) => cells_of(c, n) * cell_w * squeeze,
                ColumnWidth::Px(w) => w * squeeze,
                ColumnWidth::Flex => flex_w,
            };
            let span = (x, w);
            x += w + gap;
            span
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL_W: f64 = 10.0;
    const CELL_H: f64 = 20.0;

    fn columns() -> [Column; 3] {
        [
            Column { header: "Name", width: ColumnWidth::Flex, align: Align::Left, sort: None },
            Column { header: "pid", width: ColumnWidth::Cells(6.0), align: Align::Right, sort: None },
            Column {
                header: "CPU%",
                width: ColumnWidth::Cells(6.0),
                align: Align::Right,
                sort: Some(SortDir::Desc),
            },
        ]
    }

    fn table(w: f64, n_visible: usize, n_rows: usize, scroll: f64) -> TableGeometry {
        let card = Rect { x: 100.0, y_top: 50.0, w, h: card_height(n_visible, CELL_H) };
        TableGeometry::new(card, &columns(), n_rows, scroll, CELL_W, CELL_H)
    }

    #[test]
    fn table_header_and_rows_take_their_spec_heights() {
        let g = table(600.0, 4, 4, 0.0);
        assert_eq!(g.header.h, t::header_h(CELL_H));
        // A cell height that gives a fractional row is put on whole
        // pixels.
        assert_eq!((row_height(16.0), header_height(16.0)), (22.0, 24.0));
        assert_eq!(row_height(CELL_H), t::row_h(CELL_H));
        assert_eq!(g.separator_y, g.header.y_top + g.header.h);
        assert_eq!(g.rows.len(), 4);
        for (i, r) in &g.rows {
            assert_eq!(r.h, t::row_h(CELL_H));
            assert_eq!(r.y_top, g.body.y_top + *i as f64 * t::row_h(CELL_H));
            assert_eq!((r.x, r.w), (g.card.x, g.card.w));
        }
        // The card is exactly as tall as its rows need.
        let last = g.rows.last().unwrap().1;
        assert!((g.card.y_top + g.card.h - (last.y_top + last.h) - body_pad()).abs() < 1e-9);
        assert_eq!(g.max_scroll, 0.0);
    }

    #[test]
    fn table_columns_keep_the_gap_and_end_at_the_row_padding() {
        let g = table(600.0, 4, 4, 0.0);
        let pad = spec::px(card_spec::ROW_PAD_X);
        let gap = t::COLUMN_GAP_CELLS * CELL_W;
        assert_eq!(g.columns[0].0, g.card.x + pad);
        for w in g.columns.windows(2) {
            assert!((w[1].0 - (w[0].0 + w[0].1) - gap).abs() < 1e-9);
        }
        let (x, w) = g.columns[2];
        assert!((x + w - (g.card.x + g.card.w - pad)).abs() < 1e-9);
        assert_eq!((g.columns[1].1, g.columns[2].1), (6.0 * CELL_W, 6.0 * CELL_W));
        // A card of the natural width gives the flex column what was
        // asked for it.
        let natural = natural_width(&columns(), 20.0, CELL_W);
        let g = table(natural, 4, 4, 0.0);
        assert!((g.columns[0].1 - 20.0 * CELL_W).abs() < 1e-9);
    }

    #[test]
    fn table_columns_too_wide_for_the_card_shrink_inside_it() {
        let g = table(120.0, 4, 4, 0.0);
        let right = g.card.x + g.card.w - spec::px(card_spec::ROW_PAD_X);
        let (x, w) = *g.columns.last().unwrap();
        assert!(x + w <= right + 1e-9, "the last column ends at {} past {right}", x + w);
        assert_eq!(g.columns[0].1, 0.0);
    }

    #[test]
    fn table_rows_that_do_not_fit_are_not_laid_out() {
        let g = table(600.0, 4, 10, 0.0);
        assert_eq!(g.rows.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1, 2, 3]);
        let row_h = t::row_h(CELL_H);
        assert!((g.max_scroll - 6.0 * row_h).abs() < 1e-9);
        // Part of the way to the next row, the top one no longer fits
        // and the next one down has no room yet.
        let g = table(600.0, 4, 10, 8.0);
        assert_eq!(g.rows.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![1, 2, 3]);
        let g = table(600.0, 4, 10, row_h);
        assert_eq!(g.rows.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        // Asking for more than there is stops at the last row.
        let g = table(600.0, 4, 10, 1e9);
        assert_eq!(g.scroll, g.max_scroll);
        assert_eq!(g.rows.last().unwrap().0, 9);
        for (_, r) in &g.rows {
            assert!(r.y_top >= g.body.y_top - 1e-9);
            assert!(r.y_top + r.h <= g.body.y_top + g.body.h + 1e-9);
        }
        // The header does not move.
        assert_eq!(g.header.y_top, g.card.y_top);
    }

    #[test]
    fn table_row_at_finds_only_laid_out_rows() {
        let g = table(600.0, 4, 10, 0.0);
        let (_, r) = g.rows[2];
        assert_eq!(g.row_at(r.x + 5.0, r.y_top + 1.0), Some(2));
        assert_eq!(g.row_at(r.x + 5.0, g.header.y_top + 1.0), None, "the header is not a row");
        assert_eq!(g.row_at(r.x + 5.0, g.card.y_top + g.card.h + 30.0), None);
        assert_eq!(g.row_at(g.card.x - 1.0, r.y_top + 1.0), None);
    }

    #[test]
    fn table_cell_text_is_cut_to_its_column_and_indented_by_level() {
        let g = table(400.0, 4, 4, 0.0);
        let row = g.rows[0].1;
        let (x, w) = g.columns[0];
        let (rect, text) = g.cell_text(row, 0, 0, "short");
        assert_eq!((rect.x, rect.w, text.as_str()), (x, w, "short"));
        let (rect, text) = g.cell_text(row, 0, 2, "short");
        assert_eq!(rect.x, x + 2.0 * t::INDENT_CELLS * CELL_W);
        assert_eq!(text, "short");
        let long = "a-process-name-far-longer-than-the-column-it-is-shown-in";
        let (rect, text) = g.cell_text(row, 0, 1, long);
        assert!(text.ends_with('…'), "a cut has to show: {text:?}");
        let end = rect.x + text.chars().count() as f64 * CELL_W;
        assert!(end <= x + w + 1e-9, "the text ends at {end}, the column at {}", x + w);
        assert!(end <= g.columns[1].0 - t::COLUMN_GAP_CELLS * CELL_W + 1e-9);
    }

    #[test]
    fn table_highlight_stops_short_of_the_card_on_both_sides() {
        let g = table(600.0, 4, 4, 0.0);
        let row = g.rows[1].1;
        let h = g.highlight_rect(row);
        let inset = spec::px(t::SELECTED_INSET);
        assert_eq!(h.x - g.card.x, inset);
        assert_eq!(g.card.x + g.card.w - (h.x + h.w), inset);
        assert_eq!((h.y_top, h.h), (row.y_top, row.h));
    }
}
