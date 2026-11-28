//! The terminal scene of `chrome_snapshot_terminal`.

use crate::grid::{Cell, CellAttrs, Color, Grid};

/// A screen that uses every terminal colour: the sixteen as text and
/// as backgrounds, the SGR attributes, and a line each for a
/// selection, a search hit and a link to land on.
pub(super) fn ansi_scene(cols: u16, rows: u16) -> Grid {
    const NAMES: [&str; 8] = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
    let mut g = Grid::new(cols, rows);
    let mut put = |row: u16, col: u16, text: &str, attrs: CellAttrs| {
        for (i, ch) in text.chars().enumerate() {
            g.set_cell(col + i as u16, row, Cell { ch, attrs });
        }
        col + text.chars().count() as u16
    };
    let plain = CellAttrs::default();
    put(0, 0, "$ colours", plain);
    for bright in [0u8, 8] {
        let row = 1 + bright as u16 / 8;
        let mut col = 0;
        for (i, name) in NAMES.iter().enumerate() {
            let fg = Color::indexed(bright + i as u8);
            col = put(row, col, name, CellAttrs { fg, ..plain }) + 1;
        }
    }
    for i in 0..16u8 {
        put(3, i as u16 * 3, "   ", CellAttrs { bg: Color::indexed(i), ..plain });
    }
    let mut col = put(4, 0, "bold", CellAttrs { bold: true, ..plain }) + 1;
    col = put(4, col, "dim", CellAttrs { dim: true, ..plain }) + 1;
    col = put(4, col, "underline", CellAttrs { underline: true, ..plain }) + 1;
    col = put(4, col, "reverse", CellAttrs { reverse: true, ..plain }) + 1;
    put(4, col, "red on yellow", CellAttrs { fg: Color::indexed(1), bg: Color::indexed(3), ..plain });
    put(5, 0, "select: the quick brown fox", plain);
    put(6, 0, "search: a needle in a haystack", plain);
    put(7, 0, "link: https://example.com/docs/themes", plain);
    put(8, 0, "$ ", plain);
    g.set_cursor(2, 8);
    g
}
