//! Replay a bytelog and ask, frame by frame, whether an alt-screen
//! repaint can be read as a vertical SHIFT of the previous frame.
//!
//! If it can, a selection on such a pane can follow the content the
//! program moved; if it cannot, nothing downstream can either.
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

fn row_hashes(t: &marspot_term::terminal::Terminal, cols: u16, rows: u16) -> Vec<u64> {
    (0..rows)
        .map(|r| {
            let mut h = DefaultHasher::new();
            for c in 0..cols {
                t.grid().cell_at_view(0, c, r).ch.hash(&mut h);
            }
            h.finish()
        })
        .collect()
}

/// Best vertical alignment of `new` onto `old`: the shift with the most
/// matching non-blank rows.  Returns (shift, matched rows).
fn best_shift(old: &[u64], new: &[u64], blank: u64) -> (i32, usize) {
    let n = old.len() as i32;
    let mut best = (0i32, 0usize);
    for s in -(n - 1)..n {
        let mut m = 0;
        for r in 0..n {
            let src = r + s;
            if src < 0 || src >= n { continue; }
            if new[r as usize] == old[src as usize] && new[r as usize] != blank { m += 1; }
        }
        if m > best.1 { best = (s, m); }
    }
    best
}

fn main() {
    let log = std::env::args().nth(1).unwrap();
    let data = std::fs::read(&log).unwrap();
    let (cols, rows) = (73u16, 63u16);
    let mut t = marspot_term::terminal::Terminal::new(cols, rows);
    let mut blank_h = DefaultHasher::new();
    for _ in 0..cols { ' '.hash(&mut blank_h); }
    let blank = blank_h.finish();
    let mut prev: Option<Vec<u64>> = None;
    let mut alt_frames = 0usize;
    let mut shifted = 0usize;
    let mut clean = 0usize;
    let mut hist: std::collections::BTreeMap<i32, usize> = Default::default();
    for chunk in data.chunks(4096) {
        t.feed(chunk);
        if !t.in_alt_screen() { prev = None; continue; }
        let cur = row_hashes(&t, cols, rows);
        if let Some(p) = &prev {
            if *p != cur {
                alt_frames += 1;
                let (s, m) = best_shift(p, &cur, blank);
                if s != 0 && m >= 8 {
                    shifted += 1;
                    *hist.entry(s).or_default() += 1;
                    // "clean" = the shift explains most of the screen,
                    // which is what re-anchoring a selection needs.
                    if m >= (rows as usize * 2) / 3 { clean += 1; }
                }
            }
        }
        prev = Some(cur);
    }
    println!("alt frames changed: {alt_frames}");
    println!("  read as a shift (>=8 rows matched): {shifted}");
    println!("  shift explains >=2/3 of the screen: {clean}");
    println!("  shift histogram: {hist:?}");
}
