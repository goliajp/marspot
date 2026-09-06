//! Replay a byte range through the real terminal and print the screen.
//!
//! A session's bytelog is the ground truth for "what did the program
//! actually draw".  Stripping escapes out of it is not enough: a TUI
//! that paints with absolute cursor positioning collapses into one
//! line that way, and the layout — the thing in question — is exactly
//! what the escapes carry.  So run the real parser over it.
//!
//!   cargo run --example replay_bytes -- <file> [cols] [rows]
fn main() {
    let mut a = std::env::args().skip(1);
    let path = a.next().expect("usage: replay_bytes <file> [cols] [rows]");
    let cols: u16 = a.next().map(|s| s.parse().unwrap()).unwrap_or(108);
    let rows: u16 = a.next().map(|s| s.parse().unwrap()).unwrap_or(33);
    let bytes = std::fs::read(&path).expect("read");
    let mut t = marspot_term::terminal::Terminal::new(cols, rows);
    t.feed(&bytes);
    let g = t.grid();
    for row in 0..rows {
        let line: String = (0..cols)
            .map(|c| g.cell(c, row).ch)
            .filter(|c| *c != '\0')
            .collect();
        // The wrap flag is the datum that decides whether the row
        // above continues into this one — a path broken across it is
        // merged only when the terminal says it wrapped.
        let w = if g.wrapped_at_view(0, row) { "↩" } else { " " };
        println!("{row:>3}{w}|{}|", line.trim_end());
    }
}
