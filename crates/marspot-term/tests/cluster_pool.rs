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
    let bogus = Cell { ch: '\u{ff}', attrs: c.attrs };
    g.set_cell(1, 0, bogus);
    assert_eq!(g.cluster_text(&g.cell(1, 0)), None, "no text rather than the wrong text");

    g.sweep_clusters();
    assert!(!g.cell(1, 0).is_cluster(), "and the sweep clears the flag");
    assert_eq!(g.cell(1, 0).ch, ' ');
}
