//! The table, which the process monitor's two cards are built from
//! and which is the first component here that scrolls.
//!
//! Measured in terminal cells: a 10 x 20 cell here, so a header row is
//! 30 tall, a data row 27, a column gap 20.

use marspot::ui::components::table::{card_height, Align, Column, ColumnWidth, TableGeometry};
use marspot_term::layout::Rect;

const CELL_W: f64 = 10.0;
const CELL_H: f64 = 20.0;

fn col(header: &'static str, width: ColumnWidth, align: Align) -> Column {
    Column { header, width, align, sort: None }
}

fn r(x: &Rect) -> String {
    format!("{:.1},{:.1} {:.1}x{:.1}", x.x, x.y_top, x.w, x.h)
}

/// A card with room for four rows, holding `n_rows`.
fn table(cols: &[Column], n_rows: usize, scroll: f64) -> TableGeometry {
    let card = Rect { x: 100.0, y_top: 50.0, w: 400.0, h: card_height(4, CELL_H) };
    TableGeometry::new(card, cols, n_rows, scroll, CELL_W, CELL_H)
}

fn two_cols() -> Vec<Column> {
    vec![
        col("Name", ColumnWidth::Flex, Align::Left),
        col("CPU%", ColumnWidth::Cells(8.0), Align::Right),
    ]
}

fn dump(t: &TableGeometry) -> String {
    let mut out = format!(
        "card {}\nheader {}\nseparator {:.1}\nbody {}\n",
        r(&t.card), r(&t.header), t.separator_y, r(&t.body),
    );
    for (i, (x, w)) in t.columns.iter().enumerate() {
        out += &format!("col[{i}] x {x:.1} w {w:.1}\n");
    }
    for (i, row) in &t.rows {
        out += &format!("row[{i}] {}\n", r(row));
    }
    out
}

fn laid_out(t: &TableGeometry) -> Vec<usize> {
    t.rows.iter().map(|(i, _)| *i).collect()
}

/// The header is sticky, the line sits under it, and rows step by
/// the row height from the body's top.  Columns sit inside the
/// card's row padding; the flex column takes what the fixed one and
/// the gap leave.
#[test]
fn the_header_is_sticky_and_the_rows_step_below_it() {
    assert_eq!(
        dump(&table(&two_cols(), 6, 0.0)),
        "card 100.0,50.0 400.0x155.0\n\
         header 100.0,50.0 400.0x30.0\n\
         separator 80.0\n\
         body 100.0,89.0 400.0x108.0\n\
         col[0] x 124.0 w 252.0\n\
         col[1] x 396.0 w 80.0\n\
         row[0] 100.0,89.0 400.0x27.0\n\
         row[1] 100.0,116.0 400.0x27.0\n\
         row[2] 100.0,143.0 400.0x27.0\n\
         row[3] 100.0,170.0 400.0x27.0\n"
    );
}

/// Scrolling moves the rows and leaves the header where it is --
/// which is the whole reason the header has its own rect.
#[test]
fn scrolling_moves_the_rows_and_not_the_header() {
    let t = table(&two_cols(), 6, 30.0);
    assert_eq!(r(&t.header), "100.0,50.0 400.0x30.0");
    assert_eq!(t.scroll, 30.0);
    assert_eq!(t.max_scroll, 54.0);
    assert_eq!(laid_out(&t), vec![2, 3, 4]);
    assert_eq!(r(&t.rows[0].1), "100.0,113.0 400.0x27.0");
    // A row scrolled above the body is not clickable there.
    assert_eq!(t.row_at(200.0, 60.0), None);
    assert_eq!(t.row_at(200.0, 95.0), None);
    assert_eq!(t.row_at(200.0, 115.0), Some(2));
    // Asking for more than the rows allow stops at the last one.
    assert_eq!(table(&two_cols(), 6, 500.0).scroll, 54.0);
}

/// There is no scissor in the UI paint path, so a row straddling
/// the body's edge would be drawn whole, onto whatever the card
/// sits on.  Only rows that fit whole are laid out, and nothing
/// else is there to be painted or clicked.
#[test]
fn only_rows_that_fit_the_body_whole_are_laid_out() {
    let cols = two_cols();
    let t = table(&cols, 6, 0.0);
    assert_eq!(laid_out(&t), vec![0, 1, 2, 3]);
    for (i, rr) in &t.rows {
        assert!(
            rr.y_top >= t.body.y_top - 1e-9 && rr.y_top + rr.h <= t.body.y_top + t.body.h + 1e-9,
            "row {i} at {rr:?} is outside the body {:?}",
            t.body,
        );
    }
    // Scrolled by 8px the top row no longer fits, and the next one
    // down does not gain a place: a list mid-scroll shows one fewer
    // than a list at rest, which is what having no scissor costs.
    assert_eq!(laid_out(&table(&cols, 6, 8.0)), vec![1, 2, 3]);
    // At exactly one row down it is back to four.
    assert_eq!(laid_out(&table(&cols, 6, 27.0)), vec![1, 2, 3, 4]);
}

/// Fixed columns adding up to more than the card shrink in
/// proportion, gaps included, so every column keeps a share and
/// none escapes the card's padding.
#[test]
fn columns_wider_than_the_card_shrink_instead_of_overflowing() {
    let cols = vec![
        col("A", ColumnWidth::Cells(30.0), Align::Left),
        col("B", ColumnWidth::Cells(30.0), Align::Left),
        col("C", ColumnWidth::Flex, Align::Left),
    ];
    let t = table(&cols, 1, 0.0);
    assert_eq!(
        t.columns.iter().map(|(x, w)| format!("{x:.1}+{w:.1}")).collect::<Vec<_>>(),
        ["124.0+165.0", "300.0+165.0", "476.0+0.0"],
    );
}

/// The ordinary case: the columns end exactly at the card's row
/// padding, which is what the shrink must not change.
#[test]
fn columns_that_fit_end_at_the_row_padding() {
    let t = table(&two_cols(), 1, 0.0);
    let (x, w) = t.columns.last().copied().unwrap();
    assert_eq!(x + w, 476.0);
}
