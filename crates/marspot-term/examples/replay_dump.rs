//! Replay a recorded byte stream into the terminal and print the grid.
//!
//! The byte stream is whatever a program actually wrote — captured from
//! a pty, or handcrafted.  What comes out is the screen that stream
//! produces, as JSON, so another tool can compare it against the same
//! stream replayed somewhere else.  That comparison is the point: a
//! difference is either a bug here or a decision worth writing down,
//! and neither is visible while the only judge of our output is us.
//!
//!     cargo run -p marspot-term --example replay_dump -- FILE COLS ROWS
//!
//! Colours come out named the way this engine stores them — `null` for
//! "whatever the theme says", a number for a palette entry, three for a
//! direct colour.  A cell holding a grapheme cluster prints the whole
//! cluster, not the stand-in character the grid keeps.

use std::io::{BufWriter, Write};

use marspot_term::grid::{Cell, ColorKind, Grid};
use marspot_term::terminal::Terminal;

fn main() {
    let mut args = std::env::args().skip(1);
    let (path, cols, rows) = match (args.next(), args.next(), args.next()) {
        (Some(p), Some(c), Some(r)) => (p, c, r),
        _ => {
            eprintln!("usage: replay_dump FILE COLS ROWS");
            std::process::exit(2);
        }
    };
    let cols: u16 = cols.parse().expect("COLS is a number");
    let rows: u16 = rows.parse().expect("ROWS is a number");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(2);
    });

    let mut term = Terminal::new(cols, rows);
    // In chunks, because a stream arrives in chunks and an escape
    // sequence split across two of them is a thing that happens.
    for chunk in bytes.chunks(4096) {
        term.feed(chunk);
    }

    let out = std::io::stdout();
    let mut w = BufWriter::new(out.lock());
    write!(w, "{{\"cols\":{cols},\"rows\":{rows},\"rowsData\":[").unwrap();
    let grid = term.grid();
    for row in 0..rows {
        if row > 0 {
            write!(w, ",").unwrap();
        }
        write!(w, "[").unwrap();
        for col in 0..cols {
            if col > 0 {
                write!(w, ",").unwrap();
            }
            let cell = grid.cell_at_view(0, col, row);
            write_cell(&mut w, grid, &cell);
        }
        write!(w, "]").unwrap();
    }
    writeln!(w, "]}}").unwrap();
}

fn write_cell(w: &mut impl Write, grid: &Grid, cell: &Cell) {
    write!(w, "{{\"c\":").unwrap();
    match grid.cluster_text(cell) {
        Some(text) => write_json_string(w, text),
        // The continuation half of a wide glyph holds NUL, and that is
        // what it should print: a comparison has to see that the cell
        // is spoken for rather than empty.
        None => write_json_string(w, &cell.ch.to_string()),
    }
    let a = &cell.attrs;
    write!(w, ",\"fg\":").unwrap();
    write_color(w, a.fg.kind());
    write!(w, ",\"bg\":").unwrap();
    write_color(w, a.bg.kind());
    let mut flags: Vec<&str> = Vec::new();
    for (on, name) in [
        (a.bold, "bold"),
        (a.italic, "italic"),
        (a.underline, "underline"),
        (a.reverse, "reverse"),
        (a.dim, "dim"),
    ] {
        if on {
            flags.push(name);
        }
    }
    write!(w, ",\"flags\":[").unwrap();
    for (i, f) in flags.iter().enumerate() {
        write!(w, "{}\"{f}\"", if i > 0 { "," } else { "" }).unwrap();
    }
    write!(w, "]}}").unwrap();
}

fn write_color(w: &mut impl Write, kind: ColorKind) {
    match kind {
        ColorKind::Default => write!(w, "null"),
        ColorKind::Indexed(i) => write!(w, "{i}"),
        ColorKind::Rgb(r, g, b) => write!(w, "[{r},{g},{b}]"),
    }
    .unwrap();
}

fn write_json_string(w: &mut impl Write, s: &str) {
    write!(w, "\"").unwrap();
    for ch in s.chars() {
        match ch {
            '"' => write!(w, "\\\""),
            '\\' => write!(w, "\\\\"),
            '\n' => write!(w, "\\n"),
            '\r' => write!(w, "\\r"),
            '\t' => write!(w, "\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => write!(w, "\\u{:04x}", c as u32),
            c => write!(w, "{c}"),
        }
        .unwrap();
    }
    write!(w, "\"").unwrap();
}
