//! Replay a real session's bytelog and dump what the link scan sees.
//!
//! Usage: cargo run --release --example link_replay_dump -- <session-dir>
use marspot_term::grid_links::{self, ScanOpts};
use marspot_term::terminal::Terminal;

fn read_geometry(dir: &std::path::Path) -> (u16, u16) {
    let toml = std::fs::read_to_string(dir.join("entry.toml")).expect("entry.toml");
    let (mut cols, mut rows) = (80u16, 24u16);
    for line in toml.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let (k, v) = (k.trim(), v.trim());
            match k {
                "cols" => cols = v.parse().unwrap_or(cols),
                "rows" => rows = v.parse().unwrap_or(rows),
                _ => {}
            }
        }
    }
    (cols, rows)
}

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("session dir"));
    let (cols, rows) = read_geometry(&dir);
    let mut term = Terminal::new(cols, rows);
    let upto: Option<usize> = std::env::args().nth(2).and_then(|s| s.parse().ok());
    for seg in ["bytelog.1", "bytelog"] {
        let p = dir.join(seg);
        if let Ok(bytes) = std::fs::read(&p) {
            let slice = match (seg, upto) {
                ("bytelog", Some(n)) if n < bytes.len() => &bytes[..n],
                _ => &bytes[..],
            };
            term.feed(slice);
        }
    }
    let grid = term.grid();
    let (ccol, crow) = grid.cursor();
    println!("geometry {cols}x{rows}   cursor=(col {ccol}, row {crow})");
    println!("--- all rows ---");
    for r in 0..rows {
        let mut line = String::new();
        for c in 0..cols {
            line.push(grid.cell(c, r).ch);
        }
        println!("  row{r:>3} |{}| wrapped={}", line.trim_end(), grid.row_wrapped(r));
    }
    for cc in [true, false] {
        let links = grid_links::scan_visible_links(grid, 0, ScanOpts { cc_mode: cc, ..Default::default() });
        println!("--- cc_mode={cc}: {} links ---", links.len());
        let mut seen: Vec<(u16, &str)> = links.iter().map(|l| (l.row, l.text.as_str())).collect();
        seen.sort();
        seen.dedup();
        for (row, t) in seen {
            println!("  row{row:>3}  {t}");
        }
    }
}
