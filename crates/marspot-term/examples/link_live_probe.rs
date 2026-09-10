//! Scan a LIVE pane's published frame the way the render pass does,
//! with a working directory given on the command line and the real
//! filesystem answering — so a link that is missing on screen can be
//! told apart from a link the scanner cannot find.
//!
//!   cargo run -p marspot-term --release --example link_live_probe -- <sid> [cwd]
use marspot_linkify::{CellSource, PathOracle, PathVerdict, ScanOpts};
use std::os::fd::AsRawFd;

struct Frame {
    cells: Vec<marspot_term::grid::Cell>,
    cols: u16,
    rows: u16,
}

impl CellSource for Frame {
    fn cols(&self) -> u16 { self.cols }
    fn rows(&self) -> u16 { self.rows }
    fn char_at(&self, col: u16, row: u16) -> char {
        self.cells[row as usize * self.cols as usize + col as usize].ch
    }
    fn is_soft_wrap_continuation(&self, _row: u16) -> bool { false }
    fn cursor(&self) -> (u16, u16) { (0, 0) }
    fn is_wide(&self, ch: char) -> bool { marspot_term::grid::char_width(ch) == 2 }
}

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
    let id: u64 = std::env::args().nth(1).unwrap().parse().unwrap();
    let cwd = std::env::args().nth(2);
    let name = marspot_term::grid_shm::session_shm_name(id);
    let fd = marspot_term::grid_shm::open_region(&name).expect("open shm");
    let r = marspot_term::grid_shm::GridShmReader::from_fd(fd.as_raw_fd()).expect("map");
    let (mut cells, mut wrapped) = (Vec::new(), Vec::new());
    let s = r.read(&mut cells, &mut wrapped).expect("frame");
    let f = Frame { cells, cols: s.cols, rows: s.rows };
    for row in 0..f.rows {
        let line: String = (0..f.cols)
            .map(|c| { let ch = f.char_at(c, row); if ch == '\0' { ' ' } else { ch } })
            .collect();
        if line.contains(".md") || line.contains('/') {
            println!("  r{row:>2}: {}", line.trim_end());
        }
    }
    for tui_mode in [false, true] {
        let hits = marspot_linkify::scan_visible_links_with(
            &f,
            ScanOpts { tui_mode, cwd: cwd.as_deref() },
            &Real,
        );
        println!("tui_mode={tui_mode} cwd={cwd:?} -> {} hits", hits.len());
        for h in &hits {
            println!("    r{} {:?} {:?} target={:?}", h.row, h.kind, h.text, h.target);
        }
    }
}
