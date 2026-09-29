//! Replay a pane's bytelog and, every time a given token is on the
//! screen, scan the grid the way the render pass does — with the real
//! working directory and the real filesystem answering.  Says whether
//! the scanner ever failed to find it, and on which frame.
//!
//!   cargo run -p marspot-term --release --example link_replay_probe \
//!     -- <bytelog> <cwd> <token>
use marspot_linkify::{PathOracle, PathVerdict, ScanOpts};

struct Real;
impl PathOracle for Real {
    fn probe(&self, p: &str) -> PathVerdict {
        if std::fs::symlink_metadata(p).is_ok() {
            PathVerdict::Exists
        } else {
            PathVerdict::Missing
        }
    }
}

fn main() {
    let mut a = std::env::args().skip(1);
    let log = a.next().unwrap();
    let cwd = a.next().unwrap();
    let token = a.next().unwrap();
    let data = std::fs::read(&log).unwrap();
    let mut t = marspot_term::terminal::Terminal::new(73, 63);
    let (mut on_screen, mut linked, mut first_miss) = (0usize, 0usize, None::<String>);
    for (i, chunk) in data.chunks(2048).enumerate() {
        t.feed(chunk);
        let g = t.grid();
        let mut row_with = None;
        for r in 0..g.rows() {
            let line: String = (0..g.cols())
                .map(|c| g.cell_at_view(0, c, r).ch)
                .filter(|c| *c != '\0')
                .collect();
            if line.contains(&token) {
                row_with = Some((r, line.trim_end().to_string()));
                break;
            }
        }
        let Some((_r, line)) = row_with else { continue };
        on_screen += 1;
        let hits = marspot_term::grid_links::scan_visible_links_with(
            g,
            0,
            marspot_term::grid_links::ScanOpts { cwd: Some(&cwd), cc_mode: true },
            &Real,
        );
        let _ = ScanOpts::default();
        if hits.iter().any(|h| h.text.contains(&token)) {
            linked += 1;
        } else if first_miss.is_none() {
            first_miss = Some(format!("chunk {i}: {line}"));
        }
    }
    println!("frames with the token on screen: {on_screen}");
    println!("  of those, scanned as a link:   {linked}");
    if let Some(m) = first_miss {
        println!("  first frame where it was NOT a link — {m}");
    }
}
