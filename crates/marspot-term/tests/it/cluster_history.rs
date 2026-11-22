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

const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";

/// Every cluster in history, as (line, column, text), oldest first.
fn history_clusters(g: &Grid) -> Vec<(usize, u16, String)> {
    let mut out = Vec::new();
    let mut row = Vec::new();
    for line in 0..g.scrollback_len() {
        g.scrollback_clusters(line, &mut row);
        out.extend(row.drain(..).map(|(c, t)| (line, c, t)));
    }
    out
}

/// Every cluster on screen, as (column, row, text).
fn live_clusters(g: &Grid) -> Vec<(u16, u16, String)> {
    let mut out = Vec::new();
    for row in 0..g.rows() {
        for col in 0..g.cols() {
            if let Some(t) = g.cluster_text(&g.cell(col, row)) {
                out.push((col, row, t.to_string()));
            }
        }
    }
    out
}

fn plain(ch: char) -> Cell {
    Cell { ch, ..Cell::default() }
}

/// Fifteen narrow cells, then a wide cluster at columns 15-16, scrolled
/// into history: a 16-column cut falls between the cluster's halves.
fn print_wide_cluster_at_15_then_scroll(g: &mut Grid) {
    for c in 0..15 {
        g.set_cell(c, 0, plain('a'));
    }
    let cell = g.cluster_cell(FAMILY, CellAttrs::default());
    g.set_cell(15, 0, cell);
    g.set_cell(16, 0, plain('\0'));
    for _ in 0..8 {
        g.scroll_up(1, Cell::default());
    }
}

/// A width change re-cuts every history line from what the file holds,
/// which is base codepoints; the text rides beside the cells through
/// the re-cut and is filed again under the new epoch.  The cell itself
/// still reads as its base codepoint and never as a pool index.
#[test]
fn a_resize_keeps_history_clusters_whole() {
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
    assert_eq!(
        (1..=g.scrollback_len() as u16)
            .find_map(|off| cluster_at(&g, off, 0))
            .as_deref(),
        Some("e\u{301}"),
        "the re-cut line still reads whole"
    );
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

/// A wide cluster the new width would cut in half moves to the next
/// row whole, lead and trail together, with its text.
#[test]
fn a_cluster_at_the_new_wrap_point_moves_whole_to_the_next_row() {
    let d = Dir::new("split");
    let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
    print_wide_cluster_at_15_then_scroll(&mut g);
    g.resize(16, 3);
    let found = history_clusters(&g);
    assert_eq!(found.len(), 1, "one cluster, filed once: {found:?}");
    let (line, col, text) = &found[0];
    assert_eq!((*col, text.as_str()), (0, FAMILY));
    assert!(g.scrollback_wrapped(*line), "it is on the continuation row");
    let row = g.scrollback_line(*line).unwrap();
    assert_eq!((row[0].ch, row[1].ch), ('\u{1F468}', '\0'));
    let before = g.scrollback_line(line - 1).unwrap();
    assert_eq!(before[15].ch, '\0', "the column it left is padding");
}

/// Narrowing and widening back returns every cluster to where it
/// started, for both kinds of history, however often it is done.
#[test]
fn narrowing_then_widening_puts_clusters_back_where_they_were() {
    let d = Dir::new("roundtrip");
    for mut g in [
        Grid::with_scrollback_kind(20, 3, d.open(20, 4)),
        Grid::new(20, 3),
    ] {
        print_wide_cluster_at_15_then_scroll(&mut g);
        assert_eq!(history_clusters(&g).len(), 1);
        for _ in 0..5 {
            g.resize(16, 3);
            g.resize(20, 3);
        }
        let got = history_clusters(&g);
        assert_eq!(
            got.iter().map(|(_, c, t)| (*c, t.as_str())).collect::<Vec<_>>(),
            vec![(15, FAMILY)],
            "back at column 15, whole"
        );
        let row = g.scrollback_line(got[0].0).unwrap();
        assert_eq!((row[14].ch, row[15].ch, row[16].ch), ('a', '\u{1F468}', '\0'));
        assert!(!g.scrollback_wrapped(got[0].0), "one line again, not two");
    }
}

/// History another process wrote -- nothing in this one ever saw its
/// clusters -- comes through a resize whole, and the re-cut text is
/// in the files: a third process reads it too.
#[test]
fn history_from_an_earlier_process_keeps_its_clusters_through_a_resize() {
    let d = Dir::new("earlier");
    {
        let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
        print_cluster_then_scroll(&mut g, FAMILY, 8);
    }
    {
        let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
        assert_eq!(history_clusters(&g).len(), 1, "the fixture is in the file");
        g.resize(16, 3);
        let found = history_clusters(&g);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].2, FAMILY);
    }
    let g = Grid::with_scrollback_kind(16, 3, d.open(16, 4));
    let found = history_clusters(&g);
    assert_eq!(
        found.iter().map(|(_, c, t)| (*c, t.as_str())).collect::<Vec<_>>(),
        vec![(0, FAMILY)]
    );
}

/// A cluster on screen goes back into the pool, and the pool holds
/// only what the screen shows however many resizes there are.
#[test]
fn a_cluster_on_screen_survives_resizes_without_growing_the_pool() {
    let mut g = Grid::new(20, 3);
    let cell = g.cluster_cell("e\u{301}", CellAttrs::default());
    g.set_cell(3, 1, cell);
    for _ in 0..50 {
        g.resize(16, 3);
        g.resize(20, 3);
    }
    assert_eq!(live_clusters(&g), vec![(3, 1, "e\u{301}".to_string())]);
    assert_eq!(g.cluster_pool_len(), 1);
    assert!(history_clusters(&g).is_empty());
}

/// Every resize files history again, under a new epoch; the files do
/// not keep the earlier generations' records.
#[test]
fn repeated_resizes_do_not_grow_the_files() {
    let d = Dir::new("bounded");
    let mut g = Grid::with_scrollback_kind(20, 3, d.open(20, 4));
    for _ in 0..4 {
        print_wide_cluster_at_15_then_scroll(&mut g);
    }
    let size = |d: &Dir| -> u64 {
        std::fs::read_dir(&d.0)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum()
    };
    g.resize(16, 3);
    g.resize(20, 3);
    let after_one = size(&d);
    for _ in 0..30 {
        g.resize(16, 3);
        g.resize(20, 3);
    }
    assert!(size(&d) <= after_one, "{} bytes grew to {}", after_one, size(&d));
    assert_eq!(history_clusters(&g).len(), 4);
}

/// A VS16 cluster is two cells wide while its base codepoint is one:
/// at the end of a soft-wrapped row its trail is not padding, and the
/// row it continues into must not slide left over it.
#[test]
fn a_wide_vs16_cluster_at_a_seam_keeps_its_trail_through_a_resize() {
    let mut g = Grid::new(20, 3);
    for c in 0..18 {
        g.set_cell(c, 0, plain('a'));
    }
    let cell = g.cluster_cell("\u{26A0}\u{FE0F}", CellAttrs::default());
    g.set_cell(18, 0, cell);
    g.set_cell(19, 0, plain('\0'));
    g.set_cell(0, 1, plain('b'));
    g.set_row_wrapped(1, true);
    g.resize(24, 3);
    assert_eq!(live_clusters(&g), vec![(18, 0, "\u{26A0}\u{FE0F}".to_string())]);
    assert_eq!((g.cell(19, 0).ch, g.cell(20, 0).ch), ('\0', 'b'));
}

/// The file drops a line's trailing blanks; a soft wrap that fell on a
/// blank still has to glue at the old width once the line is read back
/// from disk, or "abc def" re-cuts as "abcdef".
#[test]
fn a_soft_wrap_on_a_blank_read_back_from_the_file_keeps_the_blank() {
    let d = Dir::new("blankwrap");
    let mut g = Grid::with_scrollback_kind(10, 3, d.open(10, 2));
    let row = |s: &str| -> Vec<Cell> {
        let mut r: Vec<Cell> = s.chars().map(plain).collect();
        r.resize(10, Cell::default());
        r
    };
    g.push_historic_scrollback_line(&row("abcdefghi"), false);
    g.push_historic_scrollback_line(&row("xyz"), true);
    for _ in 0..6 {
        g.push_historic_scrollback_line(&row("q"), false);
    }
    g.resize(20, 3);
    let line = (0..g.scrollback_len())
        .map(|i| g.scrollback_line(i).unwrap())
        .find(|r| r[0].ch == 'a')
        .expect("the line is still in history");
    assert_eq!((line[9].ch, line[10].ch), (' ', 'x'));
}
