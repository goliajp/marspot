//! Replay a bytelog FRAME BY FRAME and ask whether an alt-screen
//! repaint can be read as a vertical SHIFT of the frame before it.
//!
//! If it can, a selection on such a pane can be re-anchored to the
//! content the program moved; if it cannot, nothing downstream can.
//!
//! Frames are cut on the program's own synchronized-output brackets
//! (`CSI ?2026l`) when it uses them, else on cursor-home — cutting on
//! byte-count instead measures streaming output, not scrolling, and
//! answers a different question while looking like the same one.
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

/// Best vertical alignment of `new` onto `old`: the shift with the
/// most matching non-blank rows.  Returns (shift, matched rows).
fn best_shift(old: &[u64], new: &[u64], blank: u64) -> (i32, usize) {
    let n = old.len() as i32;
    let mut best = (0i32, 0usize);
    for s in -(n - 1)..n {
        let mut m = 0;
        for r in 0..n {
            let src = r + s;
            if src < 0 || src >= n {
                continue;
            }
            if new[r as usize] == old[src as usize] && new[r as usize] != blank {
                m += 1;
            }
        }
        if m > best.1 {
            best = (s, m);
        }
    }
    best
}

/// Split at the END of each frame, keeping the delimiter with the
/// frame it closes.
fn frames(data: &[u8], delim: &[u8]) -> Vec<usize> {
    let mut cuts = Vec::new();
    let mut i = 0;
    while let Some(p) = data[i..].windows(delim.len()).position(|w| w == delim) {
        i += p + delim.len();
        cuts.push(i);
    }
    cuts
}

fn main() {
    let log = std::env::args().nth(1).unwrap();
    let mut data = std::fs::read(&log).unwrap();
    // `--tail N` looks at only the last N bytes, which is how a
    // specific gesture gets measured instead of a whole session's
    // streaming output.  Mid-stream there is no `?1049h` to be seen,
    // so the alt screen is asserted before the replay starts.
    if let Some(i) = std::env::args().position(|a| a == "--tail") {
        let n: usize = std::env::args().nth(i + 1).unwrap().parse().unwrap();
        let cut = data.len().saturating_sub(n);
        let mut head = b"\x1b[?1049h".to_vec();
        head.extend_from_slice(&data[cut..]);
        data = head;
    }
    let (cols, rows) = (73u16, 63u16);
    let mut cuts = frames(&data, b"\x1b[?2026l");
    let delim = if cuts.is_empty() {
        cuts = frames(&data, b"\x1b[H");
        "CSI H"
    } else {
        "CSI ?2026l"
    };
    println!("{} frames, cut on {delim}", cuts.len());
    let mut t = marspot_term::terminal::Terminal::new(cols, rows);
    let mut bh = DefaultHasher::new();
    for _ in 0..cols {
        ' '.hash(&mut bh);
    }
    let blank = bh.finish();
    let mut prev: Option<Vec<u64>> = None;
    let (mut changed, mut half, mut two_thirds) = (0usize, 0usize, 0usize);
    let mut hist: std::collections::BTreeMap<i32, usize> = Default::default();
    let mut start = 0usize;
    for end in cuts {
        t.feed(&data[start..end]);
        start = end;
        if !t.in_alt_screen() {
            prev = None;
            continue;
        }
        let cur = row_hashes(&t, cols, rows);
        let live = cur.iter().filter(|h| **h != blank).count();
        if let Some(p) = &prev {
            if *p != cur && live >= 8 {
                changed += 1;
                let (s, m) = best_shift(p, &cur, blank);
                if s != 0 && m * 2 >= live {
                    half += 1;
                    *hist.entry(s).or_default() += 1;
                    if m * 3 >= live * 2 {
                        two_thirds += 1;
                    }
                }
            }
        }
        prev = Some(cur);
    }
    println!("alt frames that changed: {changed}");
    println!("  a nonzero shift explains >=1/2 of the live rows: {half}");
    println!("  ... >=2/3: {two_thirds}");
    let mut top: Vec<_> = hist.into_iter().collect();
    top.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    top.truncate(12);
    println!("  most common shifts: {top:?}");
}
