use super::{cell_grid, STRIP_MIN_TOTAL_ROWS};
use crate::layout::Layout;

/// A cell that is not a whole number of pixels, as a font's is.
const W: f64 = 8.43;
const H: f64 = 16.6;

fn one(h: f64) -> Layout {
    Layout::build(900.0, h, 0.0, 40.0, 0.0, 1, 1, W, H)
}

#[test]
fn the_rows_are_taken_from_the_bottom() {
    let plain = one(800.0);
    let (rows, before) = (plain.cells[0].rows, plain.generation);
    let l = plain.with_strips(&[5], W, H);
    let c = l.cells[0];
    assert_eq!((c.rows, c.strip_rows), (rows - 5, 5));
    assert_ne!(l.generation, before);
    let g = cell_grid(&c, 0.0, l.padding, W, H);
    assert_eq!(c.term_bottom, g.row_top(rows - 5));
    // given back in one call, and taken again from the full height
    let l = l.with_strips(&[3], W, H);
    assert_eq!((l.cells[0].rows, l.cells[0].strip_rows), (rows - 3, 3));
    let l = l.with_strips(&[0], W, H);
    assert_eq!(l.cells[0], one(800.0).cells[0]);
}

#[test]
fn a_pane_of_fewer_than_twelve_rows_gives_up_none() {
    let of = |rows: f64| one(40.0 + 2.0 * 8.0 * crate::layout::density() + rows * H + 1.0).with_strips(&[4], W, H).cells[0];
    let c = of(12.0);
    assert_eq!((c.rows, c.strip_rows), (STRIP_MIN_TOTAL_ROWS - 4, 4));
    let c = of(11.0);
    assert_eq!((c.rows, c.strip_rows), (11, 0));
    // nor one that would be left with nothing
    let l = one(40.0 + 16.0 + 13.0 * H).with_strips(&[20], W, H);
    assert_eq!(l.cells[0].strip_rows, 0);
}

#[test]
fn a_point_on_a_drawn_cell_is_that_cell_all_the_way_down() {
    let l = one(1400.0).with_strips(&[4], W, H);
    let c = l.cells[0];
    let g = cell_grid(&c, 0.0, l.padding, W, H);
    assert!(c.rows > 60);
    for row in 0..c.rows {
        for col in [0, c.cols - 1] {
            let (x, y) = (g.x + (col as f64 + 0.5) * g.w, g.row_top(row) + 0.5 * g.h);
            assert_eq!(l.hit_test_cell_pos(x, y, W, H), Some((0, col, row)));
            assert_eq!(l.caret_view_phys_rect(0, col, row, W, H), Some((g.x + col as f64 * g.w, g.row_top(row), g.w, g.h)));
        }
    }
}

#[test]
fn below_the_terminals_rows_is_no_cell() {
    let l = one(800.0).with_strips(&[4], W, H);
    let c = l.cells[0];
    let x = c.x + 30.0;
    assert!(l.hit_test_cell_pos(x, c.term_bottom - 1.0, W, H).is_some());
    for dy in [0.0, 10.0, 4.0 * H, 1e3] {
        assert_eq!(l.hit_test_cell_pos(x, c.term_bottom + dy, W, H), None);
    }
    assert_eq!(l.hit_test(x, c.term_bottom + 10.0), Some(0), "still the pane's");
    // without rows given up, the edge under the last row is still its
    let plain = one(800.0);
    let last = plain.cells[0].rows - 1;
    assert_eq!(plain.hit_test_cell_pos(x, 799.0, W, H).map(|p| p.2), Some(last));
}

#[test]
fn a_strip_is_hit_where_it_is_drawn() {
    let l = Layout::build(1200.0, 800.0, 0.0, 40.0, 20.0, 2, 1, W, H).with_strips(&[3, 0], W, H);
    let c = l.cells[0];
    let g = cell_grid(&c, 20.0, l.padding, W, H);
    for top in [2, c.rows] {
        for row in 0..3 {
            let y = g.row_top(top + row) + 1.0;
            assert_eq!(l.hit_test_strip(c.x + 1.0, y, W, H, |_| Some(top)), Some((0, row)), "to the pane's edge");
        }
        let hit = |y: f64| l.hit_test_strip(c.x + 30.0, y, W, H, |_| Some(top));
        assert_eq!(hit(g.row_top(top) - 1.0), None);
        assert_eq!(hit(g.row_top(top + 3) + 1.0), None);
    }
    let y = g.row_top(3);
    assert_eq!(l.hit_test_strip(c.x + 30.0, y, W, H, |_| None), None, "not drawn");
    assert_eq!(l.hit_test_strip(l.cells[1].x + 30.0, y, W, H, |_| Some(2)), None, "a pane without one");
}
