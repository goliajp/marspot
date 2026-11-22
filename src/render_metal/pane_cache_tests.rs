//! A pane patched row by row against the same pane laid out from
//! nothing.

use crate::font_cache::FontCache;
use crate::frame_build::frame::BuildStats;
use crate::frame_build::pane_cache::{push_panes, PaneInstanceCache};
use crate::glyph_atlas::GlyphAtlas;
use crate::layout::Layout;
use crate::render::SessionView;
use crate::render_metal::{new_atlas, system_default_device, CellInstance, GlyphInstance};
use crate::terminal::Terminal;

const COLS: u16 = 40;
const ROWS: u16 = 12;

struct Fixture {
    font: FontCache,
    atlas: GlyphAtlas,
    color_atlas: GlyphAtlas,
    layout: Layout,
}

fn fixture() -> Option<Fixture> {
    let device = system_default_device().ok()?;
    let font = FontCache::build().expect("font");
    let atlas = new_atlas(&device, 1024, 1024, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let color_atlas = new_atlas(&device, 1024, 1024, true, font.font_table()).map(|(a, _)| a).expect("colour atlas");
    let layout =
        Layout::build(font.cell_w * COLS as f64, font.cell_h * ROWS as f64, 0.0, 0.0, 0.0, 1, 1, font.cell_w, font.cell_h);
    Some(Fixture { font, atlas, color_atlas, layout })
}

/// Lay the terminal's grid out into `caches`, as frame `seq`.
fn lay(f: &mut Fixture, term: &Terminal, seq: u64, caches: &mut Vec<PaneInstanceCache>) -> BuildStats {
    lay_titled(f, term, seq, "", caches)
}

fn lay_titled(f: &mut Fixture, term: &Terminal, seq: u64, title: &str, caches: &mut Vec<PaneInstanceCache>) -> BuildStats {
    let view = SessionView {
        grid: term.grid(),
        view_offset: 0,
        cursor_visible: true,
        focused: true,
        title,
        selection: None,
        ime_preedit: "",
        dormant: false,
        recede: 0,
        scrim: 0.0,
        right_badge: "",
        agent_tui: false,
        cwd: "",
        top_fixed_h_cells: 0,
        bot_fixed_h_cells: 0,
        highlight_spans: &[],
        search_overlay: None,
        history: Default::default(),
        seq,
    };
    let (atlas_w, atlas_h) = f.atlas.dims();
    push_panes(
        &f.layout,
        std::slice::from_ref(&view),
        true,
        f.font.cell_w as f32,
        f.font.cell_h as f32,
        f.font.ascent as f32,
        atlas_w as f32,
        atlas_h as f32,
        &mut f.font,
        &mut f.atlas,
        &mut f.color_atlas,
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        &mut Vec::new(),
        caches,
        false,
    )
}

/// A copy of a pane's lists that is only ever written where the pane
/// says it changed: what a buffer on the GPU is.
#[derive(Default)]
struct GpuCopy {
    cells: Vec<CellInstance>,
    glyphs: Vec<GlyphInstance>,
    color_glyphs: Vec<GlyphInstance>,
}

fn write_changed<T: Clone + Default>(copy: &mut Vec<T>, list: &[T], spans: &mut Vec<(u32, u32)>) {
    copy.resize(list.len(), T::default());
    for (lo, hi) in spans.drain(..) {
        let (lo, hi) = (lo as usize, (hi as usize).min(list.len()));
        copy[lo..hi].clone_from_slice(&list[lo..hi]);
    }
}

impl GpuCopy {
    fn follow(&mut self, cache: &mut PaneInstanceCache) {
        write_changed(&mut self.cells, &cache.cells, &mut cache.dirty[0]);
        write_changed(&mut self.glyphs, &cache.glyphs, &mut cache.dirty[1]);
        write_changed(&mut self.color_glyphs, &cache.color_glyphs, &mut cache.dirty[2]);
    }
}

#[test]
fn a_pane_patched_row_by_row_is_the_pane_laid_out_from_nothing() {
    let Some(mut f) = fixture() else {
        eprintln!("skip (no Metal)");
        return;
    };
    let mut term = Terminal::new(COLS, ROWS);
    for i in 0..ROWS - 1 {
        term.feed(format!("\x1b[3{}mline {i} of the pane\x1b[0m\r\n", i % 7 + 1).as_bytes());
    }
    let mut patched = Vec::new();
    let mut copy = GpuCopy::default();
    let first = lay(&mut f, &term, 1, &mut patched);
    assert_eq!((first.rows_rebuilt, first.rows_total), (ROWS as u32, ROWS as u32));
    copy.follow(&mut patched[0]);

    // each edit, and the most rows it may lay out again: the rows it
    // writes, the row the cursor left and the row it lands on
    let edits: [(&str, u32); 6] = [
        ("\x1b[5;1Ha longer line than the one it replaces", 2),
        ("\x1b[2;1H\x1b[2K", 2),
        ("\x1b[8;3H\x1b[44mXY\x1b[0m", 2),
        ("\x1b[10;10H", 2),
        ("\x1b[3;1H\x1b[4munderlined\x1b[0m\x1b[4;1Hand the next", 3),
        ("\x1b[10;1H\u{1f600} \u{4e2d}\u{6587}", 2),
    ];
    for (n, (edit, most)) in edits.iter().enumerate() {
        term.feed(edit.as_bytes());
        let seq = n as u64 + 2;
        let stats = lay(&mut f, &term, seq, &mut patched);
        assert!(
            (1..=*most).contains(&stats.rows_rebuilt),
            "edit {edit:?} laid out {} rows, at most {most} changed",
            stats.rows_rebuilt
        );
        let mut scratch = Vec::new();
        lay(&mut f, &term, seq, &mut scratch);
        assert_eq!(patched[0].cells, scratch[0].cells, "cells after {edit:?}");
        assert_eq!(patched[0].glyphs, scratch[0].glyphs, "glyphs after {edit:?}");
        assert_eq!(patched[0].color_glyphs, scratch[0].color_glyphs, "colour glyphs after {edit:?}");
        copy.follow(&mut patched[0]);
        assert_eq!(copy.cells, patched[0].cells, "a copy written only where told, after {edit:?}");
        assert_eq!(copy.glyphs, patched[0].glyphs, "a copy written only where told, after {edit:?}");
        assert_eq!(copy.color_glyphs, patched[0].color_glyphs, "a copy written only where told, after {edit:?}");
    }

    // the same frame again: the pane is not looked at
    let last = edits.len() as u64 + 1;
    let again = lay(&mut f, &term, last, &mut patched);
    assert_eq!((again.rebuilt, again.rows_rebuilt), (0, 0));
    // a new frame holding the same thing: looked at, nothing laid out
    // again, nothing reported as changed
    let same = lay(&mut f, &term, last + 1, &mut patched);
    assert_eq!((same.rebuilt, same.rows_rebuilt), (1, 0));
    assert!(patched[0].dirty.iter().all(Vec::is_empty), "{:?}", patched[0].dirty);
}

/// Output that scrolls changes every row: the pane is laid out in
/// place, and still matches one laid out from nothing.
#[test]
fn a_pane_that_scrolled_is_laid_out_whole() {
    let Some(mut f) = fixture() else {
        eprintln!("skip (no Metal)");
        return;
    };
    let mut term = Terminal::new(COLS, ROWS);
    for i in 0..ROWS + 3 {
        term.feed(format!("line {i}\r\n").as_bytes());
    }
    let mut patched = Vec::new();
    lay(&mut f, &term, 1, &mut patched);
    term.feed(b"one more\r\n");
    let stats = lay(&mut f, &term, 2, &mut patched);
    assert_eq!(stats.rows_rebuilt, ROWS as u32);
    let mut scratch = Vec::new();
    lay(&mut f, &term, 2, &mut scratch);
    assert_eq!(patched[0].cells, scratch[0].cells);
    assert_eq!(patched[0].glyphs, scratch[0].glyphs);
}

/// A pane that scrolled keys only some of its rows: the frame after the
/// output stops lays it out whole once more, and the one after that
/// patches the row that changed, each matching a pane laid out from
/// nothing.
#[test]
fn the_frame_after_scrolling_keys_every_row_and_the_next_patches() {
    let Some(mut f) = fixture() else {
        eprintln!("skip (no Metal)");
        return;
    };
    let mut term = Terminal::new(COLS, ROWS);
    for i in 0..ROWS + 3 {
        term.feed(format!("line {i}\r\n").as_bytes());
    }
    let mut patched = Vec::new();
    lay(&mut f, &term, 1, &mut patched);
    let same_as_from_nothing = |f: &mut Fixture, term: &Terminal, seq: u64, patched: &[PaneInstanceCache], what: &str| {
        let mut scratch = Vec::new();
        lay(f, term, seq, &mut scratch);
        assert_eq!(patched[0].cells, scratch[0].cells, "cells {what}");
        assert_eq!(patched[0].glyphs, scratch[0].glyphs, "glyphs {what}");
    };
    for (seq, line) in [(2, "one more\r\n"), (3, "and another\r\n")] {
        term.feed(line.as_bytes());
        assert_eq!(lay(&mut f, &term, seq, &mut patched).rows_rebuilt, ROWS as u32, "a scroll is laid out whole");
        same_as_from_nothing(&mut f, &term, seq, &patched, "after a scroll");
    }
    term.feed(b"x");
    assert_eq!(lay(&mut f, &term, 4, &mut patched).rows_rebuilt, ROWS as u32, "the rows left unkeyed count as changed");
    same_as_from_nothing(&mut f, &term, 4, &patched, "after the first frame without a scroll");
    term.feed(b"y");
    let stats = lay(&mut f, &term, 5, &mut patched);
    assert!((1..=2).contains(&stats.rows_rebuilt), "{} rows laid out for one character", stats.rows_rebuilt);
    same_as_from_nothing(&mut f, &term, 5, &patched, "after a patch");
}

/// A pane's title strip is the window's: renaming a pane, or a new
/// badge, leaves the pane's own instances as they were.
#[test]
fn a_renamed_pane_is_left_as_it_was() {
    let Some(mut f) = fixture() else { return };
    let term = Terminal::new(40, 6);
    let mut caches = Vec::new();
    lay_titled(&mut f, &term, 1, "build", &mut caches);
    lay_titled(&mut f, &term, 1, "build", &mut caches);
    let stats = lay_titled(&mut f, &term, 1, "test", &mut caches);
    assert_eq!((stats.considered, stats.rebuilt), (1, 0), "a new title laid the pane out again");
    let stats = lay_titled(&mut f, &term, 2, "test", &mut caches);
    assert_eq!(stats.rebuilt, 1, "new output has to lay the pane out again");
}
