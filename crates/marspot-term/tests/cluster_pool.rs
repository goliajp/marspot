//! The pool that lets a cell hold more than one codepoint.
//!
//! A `Cell` is twenty bytes and those twenty bytes go to disk, so a
//! cluster cannot live in it.  It lives in a pool on the grid, and the
//! cell holds an index with a flag bit saying so.  Everything here is
//! about the two properties that makes load-bearing: an ordinary cell
//! is unchanged, and the pool does not grow without bound.

use marspot_term::grid::{Cell, CellAttrs, Grid, FLAG_CLUSTER};

#[test]
fn an_ordinary_cell_is_byte_for_byte_what_it_always_was() {
    let plain = Cell { ch: 'a', attrs: CellAttrs::default() };
    assert!(!plain.is_cluster());
    assert_eq!(plain.cluster_index(), None);
    // The flag lives in a byte that every cell ever written to disk
    // has zero, which is what "not a cluster" means.
    assert_eq!(plain.attrs._pad, [0, 0, 0]);
    assert_eq!(std::mem::size_of::<Cell>(), 20);
}

#[test]
fn a_cluster_goes_in_and_comes_back_whole() {
    let mut g = Grid::new(20, 4);
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let cell = g.cluster_cell(family, CellAttrs::default());
    assert!(cell.is_cluster());
    assert_eq!(cell.attrs._pad[0] & FLAG_CLUSTER, FLAG_CLUSTER);
    g.set_cell(0, 0, cell);
    assert_eq!(g.cluster_text(&g.cell(0, 0)), Some(family));
}

#[test]
fn several_clusters_keep_their_own_text() {
    let mut g = Grid::new(20, 4);
    let texts = ["e\u{301}", "\u{1f1ef}\u{1f1f5}", "\u{26a0}\u{fe0f}"];
    for (i, t) in texts.iter().enumerate() {
        let c = g.cluster_cell(t, CellAttrs::default());
        g.set_cell(i as u16, 0, c);
    }
    for (i, t) in texts.iter().enumerate() {
        assert_eq!(g.cluster_text(&g.cell(i as u16, 0)), Some(*t), "cell {i}");
    }
    assert_eq!(g.cluster_pool_len(), 3);
}

/// The pool cannot grow with uptime.
///
/// This is the property the whole design turns on: a TUI that repaints
/// forever would otherwise push a cluster per repaint into a pool
/// nothing ever empties.
#[test]
fn a_cell_overwritten_releases_its_cluster() {
    let mut g = Grid::new(20, 4);
    for i in 0..50 {
        let c = g.cluster_cell(&format!("e\u{301}{i}"), CellAttrs::default());
        g.set_cell(0, 0, c);
    }
    assert_eq!(g.cluster_pool_len(), 50, "every write added one");

    g.sweep_clusters();
    assert_eq!(g.cluster_pool_len(), 1, "only the one a cell still points at survives");
    assert_eq!(g.cluster_text(&g.cell(0, 0)), Some("e\u{301}49"), "and it is the right one");
}

#[test]
fn a_sweep_repoints_every_survivor() {
    let mut g = Grid::new(20, 4);
    // Three in, then drop the middle one by overwriting its cell.
    for (i, t) in ["aa\u{301}", "bb\u{301}", "cc\u{301}"].iter().enumerate() {
        let c = g.cluster_cell(t, CellAttrs::default());
        g.set_cell(i as u16, 0, c);
    }
    g.set_cell(1, 0, Cell { ch: 'x', attrs: CellAttrs::default() });
    g.sweep_clusters();

    assert_eq!(g.cluster_pool_len(), 2);
    assert_eq!(g.cluster_text(&g.cell(0, 0)), Some("aa\u{301}"), "the first moved index");
    assert_eq!(g.cluster_text(&g.cell(2, 0)), Some("cc\u{301}"), "and so did the last");
    assert_eq!(g.cell(1, 0).ch, 'x');
}

#[test]
fn sweeping_an_empty_pool_does_nothing() {
    let mut g = Grid::new(20, 4);
    g.sweep_clusters();
    assert_eq!(g.cluster_pool_len(), 0);
}

/// A stale index reads as blank, not as somebody else's cluster.
#[test]
fn a_cell_the_sweep_could_not_see_degrades_to_blank() {
    let mut g = Grid::new(20, 4);
    let c = g.cluster_cell("e\u{301}", CellAttrs::default());
    g.set_cell(0, 0, c);
    // A cell pointing past the pool — what a torn read or a future
    // format mismatch would produce.
    let bogus = Cell {
        ch: char::from_u32(marspot_term::grid::CLUSTER_INDEX_BASE + 999).unwrap(),
        attrs: c.attrs,
    };
    g.set_cell(1, 0, bogus);
    assert_eq!(g.cluster_text(&g.cell(1, 0)), None, "no text rather than the wrong text");

    g.sweep_clusters();
    assert!(!g.cell(1, 0).is_cluster(), "and the sweep clears the flag");
    assert_eq!(g.cell(1, 0).ch, ' ');
}

// ── through the parser, on a real terminal ───────────────────────────

use marspot_term::terminal::Terminal;

fn text_at(t: &Terminal, col: u16, row: u16) -> String {
    let cell = t.grid().cell(col, row);
    t.grid()
        .cluster_text(&cell)
        .map(|s| s.to_string())
        .unwrap_or_else(|| cell.ch.to_string())
}

#[test]
fn a_combining_mark_stays_with_its_letter() {
    let mut t = Terminal::new(20, 4);
    t.feed("e\u{301}".as_bytes());
    assert_eq!(text_at(&t, 0, 0), "e\u{301}", "the mark used to be dropped");
}

#[test]
fn a_zwj_family_is_one_cell_holding_all_of_it() {
    let mut t = Terminal::new(20, 4);
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    t.feed(family.as_bytes());
    assert_eq!(text_at(&t, 0, 0), family);
}

#[test]
fn a_flag_keeps_both_halves() {
    let mut t = Terminal::new(20, 4);
    t.feed("\u{1f1ef}\u{1f1f5}".as_bytes());
    assert_eq!(text_at(&t, 0, 0), "\u{1f1ef}\u{1f1f5}");
}

#[test]
fn plain_text_still_goes_in_as_plain_text() {
    let mut t = Terminal::new(20, 4);
    t.feed(b"hello");
    for (i, c) in "hello".chars().enumerate() {
        let cell = t.grid().cell(i as u16, 0);
        assert!(!cell.is_cluster(), "column {i} should not be a cluster");
        assert_eq!(cell.ch, c);
    }
    assert_eq!(t.grid().cluster_pool_len(), 0, "and nothing went in the pool");
}

/// The pool does not grow with uptime.
///
/// This is the property that decides whether the design is allowed to
/// exist at all: a program redrawing a cluster forever must not push a
/// pool entry per redraw that nothing ever drops.
#[test]
fn redrawing_a_cluster_for_ever_does_not_grow_the_pool() {
    let mut t = Terminal::new(20, 4);
    for _ in 0..20_000 {
        t.feed(b"\x1b[H");
        t.feed("e\u{301}".as_bytes());
    }
    let pool = t.grid().cluster_pool_len();
    assert!(
        pool <= 8192,
        "twenty thousand redraws left {pool} entries; the sweep is not running"
    );
    assert_eq!(text_at(&t, 0, 0), "e\u{301}", "and the cell still says the right thing");
}

/// History looks the way it did before the pool existed.
///
/// A scrollback row is plain cells with nowhere to keep a pool, so a
/// cluster degrades to its base on the way out rather than leaving an
/// index pointing at whatever later takes that slot.
#[test]
fn a_cluster_that_scrolls_off_degrades_rather_than_dangles() {
    let mut t = Terminal::new(20, 3);
    t.feed("e\u{301}\r\n".as_bytes());
    for _ in 0..5 {
        t.feed(b"x\r\n");
    }
    assert!(t.grid().scrollback_len() > 0, "the fixture needs scrollback");
    // Nothing on screen points into the pool any more.
    for row in 0..3 {
        for col in 0..20 {
            let c = t.grid().cell(col, row);
            if c.is_cluster() {
                assert!(
                    t.grid().cluster_text(&c).is_some(),
                    "a live cell points at nothing"
                );
            }
        }
    }
}

/// Copying a cluster copies the cluster, not its pool index.
///
/// Everything above is about the grid.  A cell's `ch` is a plane-15
/// index once it points into the pool, and every reader that takes
/// `ch` directly — the clipboard, `--read`, the pane's own text — puts
/// that codepoint where a person will see it.  This is the one that
/// reaches the system pasteboard, so it gets its own test.
#[test]
fn the_clipboard_gets_the_text_and_not_the_index() {
    use marspot_term::render::grid_selection_text;

    let mut t = Terminal::new(20, 4);
    t.feed("ae\u{301}b".as_bytes());

    // `abs` counts from the bottom row, so text written on row 0 of a
    // four-row grid is abs 3.
    let abs = (t.grid().rows() - 1) as u32;
    let text = grid_selection_text(t.grid(), (0, abs), (2, abs), false)
        .expect("a selection across the cluster");
    assert!(
        text.contains("e\u{301}"),
        "the mark did not make it to the clipboard: {text:?}"
    );
    assert!(
        !text.chars().any(|c| (c as u32) >= 0xF_0000),
        "a pool index reached the clipboard: {text:?}"
    );
}
