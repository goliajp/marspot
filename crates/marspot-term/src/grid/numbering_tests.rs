use crate::grid::{Cell, Grid};
use crate::terminal::Terminal;

fn next(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

/// Every cell a different true colour, then a cleared screen with
/// plain text on it.
fn every_cell_its_own_colour(cols: u16, rows: u16) {
    let mut t = Terminal::new(cols, rows);
    let n = cols as usize * rows as usize;
    let mut bytes = Vec::with_capacity(n * 24);
    for i in 0..n {
        bytes.extend_from_slice(
            format!("\x1b[48;2;{};{};{}m{}", i >> 16 & 0xFF, i >> 8 & 0xFF, i & 0xFF, (b'a' + (i % 26) as u8) as char)
                .as_bytes(),
        );
    }
    // the last cell's write wraps; stop short of scrolling
    bytes.extend_from_slice(b"\x1b[0m");
    t.feed(&bytes[..bytes.len()]);
    let g = t.grid();
    g.check_styles().unwrap();
    assert!(g.styles().is_positional(), "{cols}x{rows}: {} numbers", g.styles().used());
    assert!(g.styles().bytes() <= 8 * (n + 1) + 64, "{cols}x{rows}: {} bytes", g.styles().bytes());
    for (i, at) in [(0usize, (0u16, 0u16)), (n / 2, ((n / 2 % cols as usize) as u16, (n / 2 / cols as usize) as u16))] {
        let bg = g.cell(at.0, at.1).attrs.bg;
        assert_eq!(bg, crate::grid::Color::rgb((i >> 16) as u8, (i >> 8) as u8, i as u8));
    }
    // rows moved inside a margin carry their colours to their new
    // positions: row 1 goes down by one
    t.feed(b"\x1b[2;5r\x1b[2;1H\x1b[1L\x1b[r");
    let g = t.grid();
    g.check_styles().unwrap();
    let i = cols as usize;
    assert_eq!(g.cell(0, 2).attrs.bg, crate::grid::Color::rgb((i >> 16) as u8, (i >> 8) as u8, i as u8));
    t.feed(b"\x1b[2J\x1b[Hplain text again\r\n\x1b[31mred\x1b[0m");
    let g = t.grid();
    g.check_styles().unwrap();
    assert!(!g.styles().is_positional());
    assert_eq!(g.styles().used(), 2, "the default and red");
    assert!(g.styles().bytes() < 64 * 1024, "{} bytes after the clear", g.styles().bytes());
}

#[test]
fn a_screen_of_distinct_colours_numbers_by_position_and_comes_back() {
    if cfg!(miri) {
        every_cell_its_own_colour(12, 6);
    } else {
        every_cell_its_own_colour(122, 39);
    }
}

#[test]
fn the_largest_grid_of_distinct_colours_stays_in_bounds() {
    if cfg!(miri) {
        return;
    }
    every_cell_its_own_colour(512, 512);
}

/// Colours that keep changing for as long as output runs: the table
/// stays within its bound, sweeps keep happening, and every cell's
/// number names its attributes after every chunk.
#[test]
fn colours_that_never_repeat_keep_the_table_bounded() {
    let mut t = Terminal::new(80, 24);
    let mut generations = 0;
    let mut last = t.grid().styles().generation();
    for k in 0..if cfg!(miri) { 50u32 } else { 5000 } {
        let line = format!("\x1b[38;2;{};{};{}m{k:08}\x1b[0m line\r\n", k >> 16 & 0xFF, k >> 8 & 0xFF, k & 0xFF);
        t.feed(line.as_bytes());
        let g = t.grid();
        g.check_styles().unwrap();
        assert!(g.styles().len() <= crate::grid::style::limit_for(80 * 24) as usize);
        if g.styles().generation() != last {
            generations += 1;
            last = g.styles().generation();
        }
    }
    if !cfg!(miri) {
        assert!(generations > 10, "only {generations} sweeps freed anything");
    }
}

/// Random coloured output with every operation that moves cells
/// between rows, on a grid large enough that a sweep takes several
/// steps: no cell is ever left with a number that names other
/// attributes.
#[test]
fn sweeps_survive_rows_moving_under_them() {
    let mut s = 0x0123_4567_89AB_CDEFu64;
    let ops: [&[u8]; 10] = [
        b"\x1b[5;50r", b"\x1b[r", b"\x1b[3S", b"\x1b[2T", b"\x1b[4L", b"\x1b[3M", b"\x1b[5@", b"\x1b[7P", b"\x1b[K",
        b"\x1b[1J",
    ];
    let mut t = if cfg!(miri) { Terminal::new(40, 12) } else { Terminal::new(200, 60) };
    let steps = if cfg!(miri) { 20 } else { 6000 };
    for step in 0..steps {
        let mut chunk = Vec::new();
        for _ in 0..(next(&mut s) % 6) {
            let r = next(&mut s);
            match r % 8 {
                0..=3 => {
                    let palette = 1 + r % 3000;
                    let c = next(&mut s) % palette;
                    chunk.extend_from_slice(format!("\x1b[38;5;{};48;2;{};{};3m", c & 0xFF, c >> 8, c & 7).as_bytes());
                    chunk.extend(std::iter::repeat_n(b'x', (r >> 20) as usize % 90));
                }
                4 => chunk.extend_from_slice(b"\x1b[0m\r\n"),
                5 => chunk.extend_from_slice(format!("\x1b[{};{}H", r % 60 + 1, (r >> 8) % 200 + 1).as_bytes()),
                _ => chunk.extend_from_slice(ops[(r >> 4) as usize % ops.len()]),
            }
        }
        t.feed(&chunk);
        t.grid().check_styles().unwrap_or_else(|e| panic!("step {step}: {e}"));
    }
}

/// A grid that grew and shrank keeps its numbers right.
#[test]
fn numbers_survive_resizes() {
    let mut g = Grid::new(30, 10);
    let mut s = 7u64;
    for step in 0..if cfg!(miri) { 40 } else { 3000 } {
        let r = next(&mut s);
        let (cols, rows) = (g.cols(), g.rows());
        let mut c = Cell::from('y');
        c.attrs.fg = crate::grid::Color::indexed((r >> 8) as u8);
        match r % 5 {
            0 => g.resize(5 + (r >> 16) as u16 % 60, rows),
            1 => g.resize(cols, 2 + (r >> 16) as u16 % 30),
            _ => g.set_cell((r >> 24) as u16 % cols, (r >> 32) as u16 % rows, c),
        }
        g.check_styles().unwrap_or_else(|e| panic!("step {step}: {e}"));
    }
}

/// A run of coloured text that wraps at the bottom scrolls a row in
/// filled with its background; when that fill turns the grid
/// positional, the rest of the run must not be written under the
/// number the lane took before it.
#[test]
fn a_run_that_renumbers_the_grid_as_it_wraps_writes_the_rest_right() {
    let mut t = Terminal::new(4, 2);
    // three colours fill the table of a four-cell-pair grid
    t.feed(b"\x1b[31;41mA\x1b[32;42mB\x1b[33;43mC\r\n\x1b[31;41mWXYZPQ");
    let g = t.grid();
    g.check_styles().unwrap();
    assert!(g.styles().is_positional(), "the background fill needed a fourth number");
    for c in 0..2 {
        let cell = g.cell(c, 1);
        assert_eq!(cell.attrs.fg, crate::grid::Color::indexed(1), "{c}: {cell:?}");
        assert_eq!(cell.attrs.bg, crate::grid::Color::indexed(1));
    }
}
