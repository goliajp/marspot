//! Every distinct link a pane's bytelog ever put on screen, replayed
//! through the real scanner with the real filesystem answering.
//!
//! The replay probe next door answers "was THIS token a link, and on
//! which frame".  This one answers the question a change to the
//! arbitration rules raises: **what did the set of links on screen
//! become?**  Run it on both sides of the change and diff the output —
//! every line that moved is a link the change added, removed, or
//! retargeted, on real panes rather than on a made-up line.
//!
//!   cargo run -p marspot-term --release --example link_set_probe \
//!     -- <bytelog> <cols> <rows> <cwd> [why-substring]
//!
//! With `why-substring`, every link whose text contains it is printed
//! with the screen row that produced it — a set diff says a link
//! moved, this says which line moved it.
//!
//! Both sides of the comparison have to be handed the same world, and
//! neither half of it holds still on its own:
//!
//!   * **the bytelogs** — the panes are live and still writing, so the
//!     second run replays bytes the first never saw.  Copy the session
//!     directories somewhere first and point both runs at the copy.
//!   * **the filesystem** — see `Real` below, and `MARSPOT_LINK_SNAP`.
//!
//! Skip either and the diff carries links that only moved because the
//! world did: the first attempt at this measurement reported 48
//! changed links where the scanner had changed 46, and the two extra
//! were `/tmp` files that appeared between the runs.  Both are cheap
//! to pin and neither announces itself when you don't.
use marspot_linkify::{PathOracle, PathVerdict};
use std::cell::RefCell;
use std::collections::BTreeMap;

/// The filesystem, pinned.
///
/// A diff across two builds is only a diff of the SCANNER if both
/// sides were told the same thing about the disk.  They are not, by
/// default: the panes being replayed are live, and a minute apart the
/// second run sees `/tmp` files the first never did — 14 phantom
/// links, indistinguishable from a real change, on the first attempt
/// at this measurement.
///
/// With `MARSPOT_LINK_SNAP=<file>` every answer is served from that
/// file, misses are asked once and written back, and the next run
/// inherits them.  Run the two builds against the same file, then run
/// the first one again so both read the identical table.
struct Real {
    snap: Option<String>,
    known: RefCell<BTreeMap<String, bool>>,
}

impl Real {
    fn new() -> Self {
        let snap = std::env::var("MARSPOT_LINK_SNAP").ok();
        let mut known = BTreeMap::new();
        if let Some(f) = &snap {
            if let Ok(text) = std::fs::read_to_string(f) {
                for line in text.lines() {
                    if let Some((v, p)) = line.split_once('\t') {
                        known.insert(p.to_string(), v == "1");
                    }
                }
            }
        }
        Self { snap, known: RefCell::new(known) }
    }
    fn save(&self) {
        let Some(f) = &self.snap else { return };
        let mut out = String::new();
        for (p, v) in self.known.borrow().iter() {
            out.push_str(if *v { "1\t" } else { "0\t" });
            out.push_str(p);
            out.push('\n');
        }
        std::fs::write(f, out).unwrap();
    }
}

impl PathOracle for Real {
    fn probe(&self, p: &str) -> PathVerdict {
        if let Some(v) = self.known.borrow().get(p) {
            return if *v { PathVerdict::Exists } else { PathVerdict::Missing };
        }
        let v = std::fs::symlink_metadata(p).is_ok();
        self.known.borrow_mut().insert(p.to_string(), v);
        if v { PathVerdict::Exists } else { PathVerdict::Missing }
    }
}

fn main() {
    let mut a = std::env::args().skip(1);
    let log = a.next().expect("bytelog");
    let cols: u16 = a.next().expect("cols").parse().unwrap();
    let rows: u16 = a.next().expect("rows").parse().unwrap();
    let cwd = a.next().expect("cwd");
    let why = a.next();
    let data = std::fs::read(&log).unwrap();
    let mut t = marspot_term::terminal::Terminal::new(cols, rows);
    // A BTreeSet, not a Vec: the output is meant to be diffed, so the
    // order has to come from the content and not from the replay.
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let mut why_seen: std::collections::BTreeSet<String> = Default::default();
    let oracle = Real::new();
    for (ci, chunk) in data.chunks(2048).enumerate() {
        t.feed(chunk);
        for h in marspot_term::grid_links::scan_visible_links_with(
            t.grid(),
            0,
            marspot_term::grid_links::ScanOpts { cwd: Some(&cwd), cc_mode: true },
            &oracle,
        ) {
            if let Some(w) = &why {
                if h.text.contains(w.as_str()) {
                    let line: String = (0..t.grid().cols())
                        .map(|c| t.grid().cell_at_view(0, c, h.row).ch)
                        .filter(|c| *c != '\0')
                        .collect();
                    let entry = format!("{}\t{}", h.text, line.trim_end());
                    let entry = format!("chunk {ci}\t{entry}");
                    if why_seen.insert(entry.clone()) {
                        eprintln!("why: {entry}");
                    }
                }
            }
            seen.insert(format!(
                "{:?}\t{}\t{}",
                h.kind,
                h.text,
                h.target.as_deref().unwrap_or("-")
            ));
        }
    }
    oracle.save();
    for s in &seen {
        println!("{s}");
    }
    eprintln!("{} distinct links", seen.len());
}
