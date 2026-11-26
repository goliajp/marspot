//! Screens read from a published frame rather than from a replay.

use super::tests::claude;
use super::*;

/// A frame as the session would publish it, taken from a grid that
/// was drawn the way a pane is.
fn frame_of(bytes: &[u8], cols: u16, rows: u16) -> marspot_term::grid_shm::Frame {
    let mut term = Terminal::new(cols, rows);
    term.feed(bytes);
    let g = term.grid();
    let (cursor_col, cursor_row) = g.cursor();
    let mut cells = Vec::new();
    let mut clusters = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let cell = g.cell(c, r);
            if let Some(t) = g.cluster_text(&cell) {
                clusters.push((c, r, t.to_string()));
            }
            cells.push(cell);
        }
    }
    marspot_term::grid_shm::Frame { cols, rows, cursor_col, cursor_row, view_offset: 0, scroll_push_count: 0, cells, clusters }
}

#[test]
fn a_published_frame_yields_the_typed_text_in_the_box() {
    let f = frame_of(&claude("❯ half a sentence"), 40, 10);
    assert_eq!(composer_in_frame(&f).as_deref(), Some("half a sentence"));
}

#[test]
fn a_published_frame_with_an_empty_box_yields_nothing() {
    let f = frame_of(&claude("❯ "), 40, 10);
    assert_eq!(composer_in_frame(&f), None);
}

#[test]
fn a_cluster_in_the_box_reads_back_as_its_text() {
    // a flag is two regional indicators: one cluster
    let f = frame_of(&claude("❯ ship it \u{1f1ef}\u{1f1f5}"), 40, 10);
    assert!(f.cells.iter().any(|c| c.is_cluster()), "fixture must hold a cluster");
    assert_eq!(composer_in_frame(&f).as_deref(), Some("ship it \u{1f1ef}\u{1f1f5}"));
}

/// A session from before clusters were published sends no text for
/// them. A box holding one reads as nothing rather than as a
/// placeholder in its place.
#[test]
fn a_cluster_whose_text_did_not_arrive_reads_as_nothing() {
    let mut f = frame_of(&claude("❯ ship it \u{1f1ef}\u{1f1f5}"), 40, 10);
    f.clusters.clear();
    assert_eq!(composer_in_frame(&f), None);
}

#[test]
fn a_frame_keeps_rows_cursor_and_wide_glyphs() {
    let f = frame_of("继续\r\nnext".as_bytes(), 10, 3);
    let g = grid_of(&f);
    assert_eq!(row_text_of(&g, 0, 0, |c| g.cell(c, 0)), "继续");
    assert_eq!(row_text_of(&g, 1, 0, |c| g.cell(c, 1)), "next");
    assert_eq!(g.cursor(), (4, 1));
}
