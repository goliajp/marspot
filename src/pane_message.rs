//! Text a pane with no session writes into its own grid.

use marspot_term::grid::{char_width, Cell, Grid};

/// How many cells `text` takes: two for each wide character.
pub fn width(text: &str) -> u16 {
    text.chars().map(|ch| char_width(ch) as u16).sum()
}

/// The width a line of `text` is centred by.  Cells, for text with a
/// wide character in it.  Text without one keeps the width it has
/// always been centred by, its length in bytes, so a line that was on
/// screen before wide text could be does not move.
pub fn centring_width(text: &str) -> u16 {
    let cells = width(text);
    if cells as usize == text.chars().count() { text.len() as u16 } else { cells }
}

/// Write `text` into `row` from `col`, stopping at the last column a
/// whole character fits in.  A wide character takes its cell and the
/// one after it, as it does when a program prints it.
pub fn put(grid: &mut Grid, col: u16, row: u16, text: &str) {
    let mut col = col;
    for ch in text.chars() {
        let w = char_width(ch) as u16;
        if col + w > grid.cols() {
            break;
        }
        grid.set_cell(col, row, Cell { ch, ..Cell::default() });
        if w == 2 {
            grid.set_cell(col + 1, row, Cell { ch: '\0', ..Cell::default() });
        }
        col += w;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_character_takes_two_cells_and_is_never_cut_in_half() {
        let mut grid = Grid::new(8, 2);
        assert_eq!(width("a窗格b"), 6);
        put(&mut grid, 1, 0, "a窗格b窗");
        let row: Vec<char> = (0..8).map(|c| grid.cell(c, 0).ch).collect();
        assert_eq!(row, [' ', 'a', '窗', '\0', '格', '\0', 'b', ' '], "the last 窗 has one cell left and is dropped");
    }
}
