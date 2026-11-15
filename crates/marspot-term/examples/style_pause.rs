//! How long one cell write can take on the largest grid, when the
//! write is the one that gives attribute numbers back (a sweep step, or
//! a whole sweep when the table is full) or that changes how the grid
//! numbers its cells.
//!
//!     cargo run --release -p marspot-term --example style_pause -- [COLS ROWS]
//!
//! Every write is timed on its own; the report is the slowest writes
//! of each phase, and what the phase's total came to.  One process,
//! nothing left behind.

use std::time::{Duration, Instant};

use marspot_term::grid::{Cell, Color, Grid};

fn coloured(i: u64) -> Cell {
    let mut c = Cell::from('x');
    c.attrs.fg = Color::rgb((i >> 16) as u8, (i >> 8) as u8, i as u8);
    c
}

/// Write `n` cells, the `k`th in `style(k)`, at positions walking the
/// grid; the slowest few single writes and the total.
fn phase(g: &mut Grid, name: &str, n: u64, style: impl Fn(u64) -> u64) {
    let (cols, rows) = (g.cols() as u64, g.rows() as u64);
    let mut times: Vec<(Duration, u64)> = Vec::with_capacity(n as usize);
    let start = Instant::now();
    let (gen0, positional0) = (g.styles().generation(), g.styles().is_positional());
    for k in 0..n {
        let at = k % (cols * rows);
        let cell = coloured(style(k));
        let t = Instant::now();
        g.set_cell((at % cols) as u16, (at / cols) as u16, cell);
        times.push((t.elapsed(), k));
    }
    let total = start.elapsed();
    times.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let p999 = times[times.len() / 1000].0;
    println!(
        "{name}: {n} writes in {:.1} ms; slowest {:.1} us (write {}), then {:.1}, {:.1}; 99.9th {:.2} us; \
         numbers given back {} times; positional {} -> {}; table {} values, {} bytes",
        total.as_secs_f64() * 1e3,
        us(times[0].0),
        times[0].1,
        us(times[1].0),
        us(times[2].0),
        us(p999),
        g.styles().generation() - gen0,
        positional0,
        g.styles().is_positional(),
        g.styles().len(),
        g.styles().bytes(),
    );
}

fn main() {
    let args: Vec<u16> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let (cols, rows) = (*args.first().unwrap_or(&512), *args.get(1).unwrap_or(&512));
    let n = cols as u64 * rows as u64;
    println!("grid {cols}x{rows} = {n} cells");
    let mut g = Grid::with_scrollback(cols, rows, 0);
    // a few hundred colours, over and over: sweeps in steps
    phase(&mut g, "300 colours", 4 * n, |k| k % 300);
    // colours that never repeat: every new one a number, sweeps keep up
    phase(&mut g, "never repeating", 4 * n, |k| 1_000 + k);
    // every cell its own colour at once: the table fills, a whole sweep
    // frees nothing, the grid goes positional
    g.clear_all(Cell::default());
    phase(&mut g, "one colour a cell", n, |k| 10_000_000 + k);
    // back to the shared way: the moment it happens (a resize here)
    let t = Instant::now();
    g.clear_all(Cell::default());
    println!("clear back to shared: {:.1} us", t.elapsed().as_secs_f64() * 1e6);
    phase(&mut g, "one colour a cell again", n, |k| 20_000_000 + k);
    let t = Instant::now();
    g.resize(cols, rows - 1);
    g.resize(cols, rows);
    println!(
        "resize of a positional grid: {:.1} ms, positional after {}",
        t.elapsed().as_secs_f64() * 1e3,
        g.styles().is_positional()
    );
}
