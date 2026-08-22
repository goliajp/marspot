//! Replay a real session's PTY bytelog into a Grid and measure what
//! one `scan_visible_links` costs — how many path probes it issues,
//! how many are distinct, and how long the scan takes.
//!
//! Exists because the render-path link scan is the one place where
//! marspot puts a filesystem syscall on a per-frame path, and the
//! only honest way to size that is against a real pane's content.
//! On 2026-08-22 a live 14-pane window logged `l2.loop.stall` frames
//! of 2.1 s–13.6 s, build-bound, with 79.5 % of render self-time in
//! `lstat` beneath this call.
//!
//! Both oracles are measured: the blocking `FsOracle` (what the
//! render path used before 2026-08-23) and the async
//! `marspot::link_probe` (what it uses now).  Run it on a host that
//! actually has the network mounts — the whole point is the tail.
//!
//! Usage:
//!   cargo run --release --example link_scan_probe -- <session-dir> [scans]
//!
//! `<session-dir>` is `~/Library/Caches/marspot/sessions/<id>` — cols
//! and rows are read from its `entry.toml`, content from `bytelog`
//! (preceded by `bytelog.1` when present, matching `segment_paths`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use marspot_term::grid_links::{self, FsOracle, PathOracle, PathVerdict, ScanOpts};
use marspot_term::terminal::Terminal;

/// Wraps another oracle and records every question asked of it.
struct CountingOracle<'a> {
    inner: &'a dyn PathOracle,
    calls: RefCell<usize>,
    distinct: RefCell<HashSet<String>>,
    /// distinct path -> times asked, for the repeat histogram.
    per_path: RefCell<HashMap<String, usize>>,
}

impl<'a> CountingOracle<'a> {
    fn new(inner: &'a dyn PathOracle) -> Self {
        Self {
            inner,
            calls: RefCell::new(0),
            distinct: RefCell::new(HashSet::new()),
            per_path: RefCell::new(HashMap::new()),
        }
    }
}

impl PathOracle for CountingOracle<'_> {
    fn probe(&self, path: &str) -> PathVerdict {
        *self.calls.borrow_mut() += 1;
        self.distinct.borrow_mut().insert(path.to_string());
        *self.per_path.borrow_mut().entry(path.to_string()).or_insert(0) += 1;
        self.inner.probe(path)
    }
}

fn read_geometry(dir: &Path) -> (u16, u16) {
    let toml = std::fs::read_to_string(dir.join("entry.toml")).expect("entry.toml");
    let mut cols = 80u16;
    let mut rows = 24u16;
    for line in toml.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        match k.trim() {
            "cols" => cols = v.parse().unwrap_or(cols),
            "rows" => rows = v.parse().unwrap_or(rows),
            _ => {}
        }
    }
    (cols, rows)
}

/// Measure both oracles over a grid that holds `text`, so the
/// adversarial case can be stated instead of waited for: a path under
/// a network mount or the `auto_home` autofs map.
fn synthetic(text: &str, scans: usize) {
    let mut term = Terminal::new(120, 30);
    term.feed(format!("see {text} for details\r\n").as_bytes());
    let opts = ScanOpts { cc_mode: false };

    marspot::link_probe::install();
    for (label, oracle) in [
        ("FsOracle (blocking)", &FsOracle as &dyn PathOracle),
        ("link_probe (async) ", marspot::link_probe::oracle()),
    ] {
        let t_cold = Instant::now();
        grid_links::scan_visible_links_with(term.grid(), 0, opts, oracle);
        let cold = t_cold.elapsed();
        std::thread::sleep(std::time::Duration::from_millis(500));
        let t0 = Instant::now();
        for _ in 0..scans {
            grid_links::scan_visible_links_with(term.grid(), 0, opts, oracle);
        }
        let warm = t0.elapsed();
        println!(
            "  {label}  first {:9.2} ms   steady {:9.3} ms/scan",
            cold.as_secs_f64() * 1000.0,
            warm.as_secs_f64() * 1000.0 / scans as f64
        );
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next().expect("usage: link_scan_probe <session-dir|--synthetic PATH> [scans]");
    if first == "--synthetic" {
        let path = args.next().expect("--synthetic needs a path");
        let scans: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(20);
        println!("synthetic grid holding: {path}");
        synthetic(&path, scans);
        return;
    }
    let dir = PathBuf::from(first);
    let scans: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);

    let (cols, rows) = read_geometry(&dir);
    let mut term = Terminal::new(cols, rows);

    let mut fed = 0usize;
    for seg in ["bytelog.1", "bytelog"] {
        let p = dir.join(seg);
        let Ok(bytes) = std::fs::read(&p) else { continue };
        fed += bytes.len();
        term.feed(&bytes);
    }
    println!(
        "pane {}x{} — replayed {:.1} MiB of PTY bytes",
        cols,
        rows,
        fed as f64 / (1024.0 * 1024.0)
    );

    // cc_mode is what a claudecode pane actually renders with: it
    // merges hanging-indent continuation rows into one logical line,
    // which is exactly what makes tokens (and their candidate sets)
    // long.  Measuring without it would understate the real cost.
    marspot::link_probe::install();

    // cc_mode is what a claudecode pane actually renders with: it
    // merges hanging-indent continuation rows into one logical line,
    // which is what makes tokens (and their candidate sets) long.
    // Measuring without it would understate the real cost.
    let opts = ScanOpts { cc_mode: true };

    for (label, oracle) in [
        ("FsOracle (blocking, pre-2026-08-23)", &FsOracle as &dyn PathOracle),
        ("link_probe (async, current)", marspot::link_probe::oracle()),
    ] {
        let counter = CountingOracle::new(oracle);

        // Warm pass: the async oracle answers Unknown until its
        // worker lands verdicts, so a cold first scan measures the
        // queueing path, not the steady state.  Both are worth
        // seeing, so report the first scan separately from the rest.
        let t_cold = Instant::now();
        let cold_links = grid_links::scan_visible_links_with(term.grid(), 0, opts, &counter).len();
        let cold = t_cold.elapsed();

        std::thread::sleep(std::time::Duration::from_millis(400));

        let t0 = Instant::now();
        let mut links = 0usize;
        for _ in 0..scans {
            links = grid_links::scan_visible_links_with(term.grid(), 0, opts, &counter).len();
        }
        let warm = t0.elapsed();

        let calls = *counter.calls.borrow();
        let distinct = counter.distinct.borrow().len();
        println!(
            "{label}\n  \
             first scan: {:8.2} ms ({cold_links} links)\n  \
             steady:     {:8.2} ms/scan ({links} links)\n  \
             probes asked: {calls} total, {distinct} distinct",
            cold.as_secs_f64() * 1000.0,
            warm.as_secs_f64() * 1000.0 / scans as f64,
        );
    }

    // What each distinct path actually costs to stat here — the
    // number the async move exists for.
    println!("\nper-path blocking cost on THIS host:");
    let probe_paths: Vec<String> = {
        let fs = FsOracle;
        let c = CountingOracle::new(&fs);
        grid_links::scan_visible_links_with(term.grid(), 0, opts, &c);
        let v: Vec<String> = c.distinct.borrow().iter().cloned().collect();
        v
    };
    for path in probe_paths {
        let t = Instant::now();
        let v = FsOracle.probe(&path);
        println!("  {:>10.1} us  {v:?}  {path}", t.elapsed().as_secs_f64() * 1e6);
    }
}
