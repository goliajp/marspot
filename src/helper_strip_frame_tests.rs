//! What a frame costs when only a pane's strip has changed, and what a
//! strip is drawn as.

use crate::font_cache::FontCache;
use crate::frame_alloc_tests::counted;
use crate::frame_build::frame::{build_instances, BuildStats, FrameKeep};
use crate::glyph_atlas::GlyphAtlas;
use crate::layout::Layout;
use crate::render::{SessionView, StripView};
use crate::render_metal::{new_atlas, system_default_device};
use crate::terminal::Terminal;
use crate::ui::components::density_kit::at_scale;
use golia_ui_core::scene::{GlyphInstance, RectInstance, UiRectInstance};
use marspot_term::grid::{Cell, CellAttrs, Color, Grid};

const STRIP: u16 = 4;
fn dim() -> CellAttrs {
    CellAttrs { dim: true, ..CellAttrs::default() }
}

#[derive(Default)]
struct Lists {
    cells: Vec<RectInstance>,
    glyphs: Vec<GlyphInstance>,
    color_glyphs: Vec<GlyphInstance>,
    dots: Vec<RectInstance>,
    ui_rects: Vec<UiRectInstance>,
    ov_cells: Vec<RectInstance>,
    ov_glyphs: Vec<GlyphInstance>,
    ov_rects: Vec<UiRectInstance>,
}

struct Bench {
    font: FontCache,
    atlas: GlyphAtlas,
    color_atlas: GlyphAtlas,
    layout: Layout,
    keep: FrameKeep,
    lists: Lists,
}

impl Bench {
    /// One pane with a strip of `strip` rows.
    fn new(strip: u16) -> Option<Self> {
        let Ok(device) = system_default_device() else {
            eprintln!("SKIPPED: no Metal device");
            return None;
        };
        let font = FontCache::build().expect("font");
        let atlas = |color| new_atlas(&device, 2048, 2048, color, font.font_table()).map(|(a, _)| a).expect("atlas");
        let (atlas, color_atlas) = (atlas(false), atlas(true));
        let layout = Layout::build(1000.0, 800.0, 0.0, 0.0, 0.0, 1, 1, font.cell_w, font.cell_h)
            .with_strips(&[strip], font.cell_w, font.cell_h);
        Some(Self { font, atlas, color_atlas, layout, keep: FrameKeep::default(), lists: Lists::default() })
    }

    fn frame(&mut self, view: SessionView) -> BuildStats {
        let l = &mut self.lists;
        l.cells.clear();
        l.glyphs.clear();
        l.color_glyphs.clear();
        l.dots.clear();
        l.ui_rects.clear();
        l.ov_cells.clear();
        l.ov_glyphs.clear();
        l.ov_rects.clear();
        build_instances(
            &self.layout, &[view], &[], 0, true, None, None, None, None, None, None, None,
            &mut self.font, &mut self.atlas, &mut self.color_atlas,
            &mut l.cells, &mut l.glyphs, &mut l.color_glyphs, &mut l.dots, &mut l.ui_rects,
            &mut self.keep,
            &mut l.ov_cells, &mut l.ov_glyphs, &mut l.ov_rects,
        )
    }
}

fn write(g: &mut Grid, col: u16, row: u16, text: &str, attrs: CellAttrs) {
    for (c, ch) in (col..).zip(text.chars()) {
        g.set_cell(c, row, Cell { ch, attrs });
    }
}

fn pane(b: &Bench) -> Terminal {
    let c = b.layout.cells[0];
    let mut t = Terminal::new(c.cols, c.rows);
    t.feed(b"see https://example.com/x and more\r\n\x1b[31mred\x1b[0m $ ");
    t
}

/// The strip's clock going on by a second, every second: nothing is
/// allocated, and the pane's rows and links are not gone through.
#[test]
fn helper_strip_tick_leaves_the_pane_as_it_was_and_allocates_nothing() {
    at_scale(1.0, || {
        let Some(mut b) = Bench::new(STRIP) else { return };
        let term = pane(&b);
        let mut strip = Grid::with_scrollback(b.layout.cells[0].cols, STRIP, 0);
        write(&mut strip, 2, 1, "● main", CellAttrs::default());
        write(&mut strip, 2, 2, "○ codex", CellAttrs { fg: Color::indexed(4), ..dim() });
        write(&mut strip, 40, 2, "0123456789s", dim());
        let mut tick = |b: &mut Bench, n: u64| {
            let digits = [b'0' + (n / 10 % 10) as u8, b'0' + (n % 10) as u8];
            for (c, d) in (20..).zip(digits) {
                strip.set_cell(c, 2, Cell { ch: d as char, attrs: dim() });
            }
            b.frame(SessionView {
                cursor_visible: true,
                focused: true,
                session: 7,
                seq: 1,
                strip: Some(StripView { grid: &strip, seq: n, top_row: 2 }),
                ..SessionView::plain(term.grid())
            })
        };
        for n in 0..4 {
            tick(&mut b, n);
        }
        for n in 4..70 {
            let before = b.lists.glyphs.clone();
            let (allocs, stats) = counted(|| tick(&mut b, n));
            assert_eq!(allocs, 0, "second {n} allocated {allocs} times");
            assert_eq!((stats.rebuilt, stats.rows_rebuilt), (0, 0), "second {n} laid the pane out again");
            assert!(b.lists.glyphs.iter().zip(&before).any(|(a, b)| a.uv0 != b.uv0), "second {n} is not on screen");
        }
    });
}

/// A row of a strip is drawn exactly as the same row of a terminal.
#[test]
fn helper_strip_is_drawn_as_the_terminal_draws() {
    at_scale(1.0, || {
        let Some(mut b) = Bench::new(STRIP) else { return };
        let c = b.layout.cells[0];
        let fill = |g: &mut Grid, top: u16| {
            write(g, 2, top, "● main  ── box", CellAttrs::default());
            write(g, 2, top + 1, "bold", CellAttrs { bold: true, ..CellAttrs::default() });
            write(g, 8, top + 1, "under", CellAttrs { underline: true, ..CellAttrs::default() });
            write(g, 14, top + 1, "on blue", CellAttrs { bg: Color::indexed(4), ..dim() });
            for (i, ch) in "漢字".chars().enumerate() {
                let col = 24 + 2 * i as u16;
                g.set_cell(col, top + 1, Cell { ch, attrs: dim() });
                g.set_cell(col + 1, top + 1, Cell { ch: '\0', attrs: dim() });
            }
        };
        let blank = Terminal::new(c.cols, c.rows);
        let mut strip = Grid::with_scrollback(c.cols, STRIP, 0);
        fill(&mut strip, 0);
        for top in [5, c.rows] {
            b.frame(SessionView {
                seq: 1,
                strip: Some(StripView { grid: &strip, seq: 1, top_row: top }),
                ..SessionView::plain(blank.grid())
            });
            let (cells, glyphs) = (b.lists.cells.clone(), b.lists.glyphs.clone());
            let mut own = Grid::with_scrollback(c.cols, c.rows + STRIP, 0);
            fill(&mut own, top);
            b.frame(SessionView { seq: 2, ..SessionView::plain(&own) });
            assert_eq!(format!("{cells:?}"), format!("{:?}", b.lists.cells), "at row {top}");
            assert_eq!(format!("{glyphs:?}"), format!("{:?}", b.lists.glyphs), "at row {top}");
            assert!(glyphs.len() > 20);
        }
    });
}

/// Two sessions of one size and frame number in one place: the second
/// one's picture is drawn.
#[test]
fn helper_strip_pane_given_another_session_is_drawn_again() {
    at_scale(1.0, || {
        let Some(mut b) = Bench::new(0) else { return };
        let c = b.layout.cells[0];
        let (mut one, mut two) = (Terminal::new(c.cols, c.rows), Terminal::new(c.cols, c.rows));
        one.feed(b"parent");
        two.feed(b"helper");
        let view = |t: &Terminal, session: u64| b.frame(SessionView { session, seq: 5, ..SessionView::plain(t.grid()) });
        let mut view = view;
        assert_eq!(view(&one, 1).rebuilt, 1);
        assert_eq!(view(&one, 1).rebuilt, 0, "the same session again");
        assert_eq!(view(&two, 2).rebuilt, 1, "another session");
    });
}
