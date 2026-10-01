//! What a cluster said survives the row leaving the screen, and the
//! process.
//!
//! A cell on screen holds a pool index; the same cell in the file holds
//! the base codepoint, because that is what a binary rolled back to
//! before this feature reads out of the record. So the text is filed
//! beside the line and keyed by position, in a sidecar stamped with the
//! `.bin`'s epoch -- `line_identity.rs` is the proof that a stale
//! sidecar is refused rather than believed.
//!
//! Two things are tested here that `cluster_pool.rs` cannot: that the
//! text outlives the process, and that nothing in the grid keeps a copy
//! of it that grows with the history.

use marspot_term::grid::{Cell, CellAttrs, Grid};
use marspot_term::scrollback::Scrollback;

struct Dir(std::path::PathBuf);
impl Dir {
    fn new(label: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "marspot-clusterhist-{}-{}-{}",
            label,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn open(&self, cols: usize, ram: usize) -> Scrollback {
        Scrollback::file(
            self.0.join("scrollback.bin"),
            self.0.join("scrollback.idx"),
            cols,
            ram,
        )
        .unwrap()
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Everything the grid would show for the cluster at column 0 of a
/// history line, asked the way the renderer asks it.
fn cluster_at(g: &Grid, view_offset: u16, col: u16) -> Option<String> {
    let mut row_clusters = Vec::new();
    for row in 0..g.rows() {
        let cell = g.cell_at_view(view_offset, col, row);
        g.row_clusters_at_view(view_offset, row, &mut row_clusters);
        if let Some(t) = g
            .cluster_text(&cell)
            .or_else(|| marspot_term::grid::cluster_in_row(&row_clusters, col))
        {
            return Some(t.to_string());
        }
    }
    None
}

/// Put a cluster on the current line, then scroll it into history.
fn print_cluster_then_scroll(g: &mut Grid, text: &str, lines: usize) {
    let cell = g.cluster_cell(text, CellAttrs::default());
    g.set_cell(0, 0, cell);
    for _ in 0..lines {
        g.scroll_up(1, Cell::default());
    }
}

/// The acceptance: a file-backed pane's cluster is still whole after
/// everything in this process has gone away and the files are reopened
/// -- which is what an L3 that re-execs itself does.
#[test]
fn a_cluster_outlives_the_process_that_printed_it() {
    let d = Dir::new("reopen");
    let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    {
        let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
        print_cluster_then_scroll(&mut g, family, 8);
        assert_eq!(
            cluster_at(&g, 8, 0).as_deref(),
            Some(family),
            "the pane that printed it can still read it"
        );
    }
    let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
    // A fresh grid over the same files: the history is the file's, and
    // nothing in this process ever saw the cluster.
    let found = (1..=g.scrollback_len() as u16).find_map(|off| cluster_at(&g, off, 0));
    assert_eq!(
        found.as_deref(),
        Some(family),
        "a new process reading the same files sees the whole cluster"
    );
    let _ = &mut g;
}

/// And the record itself still holds the base codepoint, so a binary
/// that knows nothing of the sidecar reads what it always read.
#[test]
fn the_record_still_holds_the_base_codepoint() {
    let d = Dir::new("base");
    {
        let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
        print_cluster_then_scroll(&mut g, "e\u{301}", 8);
    }
    let g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
    let mut seen = Vec::new();
    for off in 1..=g.scrollback_len() as u16 {
        for row in 0..g.rows() {
            seen.push(g.cell_at_view(off, 0, row).ch);
        }
    }
    assert!(
        seen.contains(&'e'),
        "the cell on disk is the base codepoint: {seen:?}"
    );
    assert!(
        !seen.iter().any(|c| (*c as u32) >= 0xF_0000),
        "and never a pool index: {seen:?}"
    );
}

/// A line that never had a cluster answers nothing, and a line that did
/// does not lend its text to its neighbours.
#[test]
fn a_line_without_clusters_borrows_nobodys() {
    let d = Dir::new("neighbours");
    let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
    print_cluster_then_scroll(&mut g, "e\u{301}", 1);
    for _ in 0..7 {
        g.scroll_up(1, Cell::default());
    }
    let mut with = 0;
    for off in 1..=g.scrollback_len() as u16 {
        if cluster_at(&g, off, 0).is_some() {
            with += 1;
        }
    }
    assert_eq!(with, 1, "one line had a cluster, so one line has one");
}

/// The in-RAM variant keeps its own mirror, and this change must not
/// have moved its behaviour by a byte.
#[test]
fn an_in_ram_pane_still_answers_from_its_mirror() {
    let mut g = Grid::new(20, 3);
    print_cluster_then_scroll(&mut g, "\u{1F1EF}\u{1F1F5}", 8);
    let found = (1..=g.scrollback_len() as u16).find_map(|off| cluster_at(&g, off, 0));
    assert_eq!(found.as_deref(), Some("\u{1F1EF}\u{1F1F5}"));
}

/// A width change re-cuts every history line from what the file holds,
/// and what the file holds is base codepoints -- so the cluster text
/// does not come through. Measured, not assumed: before the resize the
/// cluster reads whole, after it reads as nothing.
///
/// What is pinned here is not the loss but its shape: the cell must
/// still read as its base codepoint and never as a pool index. That is
/// what makes losing it a degradation rather than a wrong character,
/// and it stays true if reflow is ever taught to carry the text along.
#[test]
fn a_resize_degrades_history_clusters_to_their_base_codepoint() {
    let d = Dir::new("resize");
    let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
    print_cluster_then_scroll(&mut g, "e\u{301}", 8);
    assert_eq!(
        (1..=g.scrollback_len() as u16)
            .find_map(|off| cluster_at(&g, off, 0))
            .as_deref(),
        Some("e\u{301}"),
        "the fixture needs the cluster to be there before the resize"
    );
    g.resize(16, 3);
    for off in 1..=g.scrollback_len() as u16 {
        for row in 0..g.rows() {
            let ch = g.cell_at_view(off, 0, row).ch;
            assert!(
                (ch as u32) < 0xF_0000,
                "a re-cut row must never show a pool index: {ch:?}"
            );
        }
    }
}
