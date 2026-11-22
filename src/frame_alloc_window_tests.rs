//! What a frame of a whole window costs once warm: nine panes with
//! their title strips and badges, the sidebar, the toolbar and the
//! seams.

use crate::font_cache::FontCache;
use crate::frame_alloc_tests::counted;
use crate::frame_build::frame::{build_instances, FrameKeep};
use crate::layout::Layout;
use crate::render::{SessionView, SidebarEntry};
use crate::render_metal::{new_atlas, system_default_device};
use crate::session::SessionState;
use crate::terminal::Terminal;
use crate::frame_build::chrome::ChromeShape;
use crate::ui::components::title_strip_view::BadgesOnly;
use golia_ui_core::scene::{GlyphInstance, RectInstance, UiRectInstance};

const NAMES: [(&str, &str); 9] = [
    ("smix", "P1 3f9c2a1e"),
    ("devops", ""),
    ("insight-mono", "P2 77d0e4b2"),
    ("alt", ""),
    ("marspot", "P3 0c41aa90"),
    ("torajs-lane-biz", ""),
    ("mailrs", "P4 9e02b7c3"),
    ("sentori", ""),
    ("kevy", ""),
];

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

/// A warm frame takes nothing from the heap, whether nothing changed,
/// a pane had output, the pointer moved onto another sidebar row, or a
/// pane was renamed; nor does the pointer moving over the chrome.
#[test]
fn a_whole_window_frame_allocates_nothing_once_warm() {
    let Ok(device) = system_default_device() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    crate::ui::components::density_kit::at_scale(1.0, || {
        let mut font = FontCache::build().expect("font");
        let mut atlas = new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        let mut color_atlas =
            new_atlas(&device, 2048, 2048, true, font.font_table()).map(|(a, _)| a).expect("colour atlas");
        let layout = Layout::build(
            1800.0,
            1100.0,
            crate::ui::SIDEBAR_W_LOGICAL,
            crate::HEADER_PT,
            crate::ui::CELL_TITLE_PT,
            3,
            3,
            font.cell_w,
            font.cell_h,
        )
        .with_chrome(1.0, NAMES.len(), None);
        let terms: Vec<Terminal> = layout
            .cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut t = Terminal::new(c.cols, c.rows);
                t.feed(format!("\x1b[3{}m{i} the quick brown fox\r\nover the lazy dog\r\n", 1 + i % 6).as_bytes());
                t
            })
            .collect();
        let sidebar: Vec<SidebarEntry> = NAMES
            .iter()
            .enumerate()
            .map(|(i, &(label, _))| SidebarEntry { label, state: [SessionState::Active, SessionState::Idle][i % 2] })
            .collect();
        let mut keep = FrameKeep::default();
        let mut l = Lists::default();
        let badges = |i: usize| NAMES[i].1;
        let strips = BadgesOnly(NAMES.len(), badges);
        let shape = ChromeShape { layout: &layout, rows: NAMES.len(), strips: &strips };
        let row_h = marspot_term::layout::sidebar_row_h();
        let row_at = |i: usize| (20.0, layout.top_inset + layout.sidebar_top_pad_phys + (i as f64 + 0.5) * row_h);
        let mut frame = |seq: u64, renamed: bool, pointer: (f64, f64)| {
            keep.chrome.pointer_move(&mut font, &shape, pointer);
            l.cells.clear();
            l.glyphs.clear();
            l.color_glyphs.clear();
            l.dots.clear();
            l.ui_rects.clear();
            l.ov_cells.clear();
            l.ov_glyphs.clear();
            l.ov_rects.clear();
            let views: [SessionView; 9] = std::array::from_fn(|i| SessionView {
                grid: terms[i].grid(),
                view_offset: 0,
                cursor_visible: i == 4,
                focused: i == 4,
                title: if renamed && i == 2 { "insight" } else { NAMES[i].0 },
                selection: None,
                ime_preedit: "",
                dormant: false,
                recede: 0,
                scrim: 0.0,
                right_badge: NAMES[i].1,
                agent_tui: false,
                cwd: "",
                top_fixed_h_cells: 0,
                bot_fixed_h_cells: 0,
                highlight_spans: &[],
                search_overlay: None,
                history: Default::default(),
                seq: if i == 4 { seq } else { 1 },
            });
            build_instances(
                &layout, &views, &sidebar, 4, true, None, None, None, None, None, None, None,
                &mut font, &mut atlas, &mut color_atlas,
                &mut l.cells, &mut l.glyphs, &mut l.color_glyphs, &mut l.dots, &mut l.ui_rects,
                &mut keep,
                &mut l.ov_cells, &mut l.ov_glyphs, &mut l.ov_rects,
            );
            l.glyphs.len()
        };
        let (lit, off) = (row_at(6), (900.0, 600.0));
        // every picture once, so each kept buffer has grown to fit
        for (seq, renamed, pointer) in [(1, false, off), (2, false, lit), (3, true, off), (4, false, off)] {
            frame(seq, renamed, pointer);
        }
        let (n, glyphs) = counted(|| frame(4, false, off));
        assert!(glyphs > 100, "the window has to have been drawn: {glyphs} glyphs");
        assert_eq!(n, 0, "a frame with nothing new allocated {n} times");
        for (what, seq, renamed, pointer) in
            [("output in a pane", 5, false, off), ("a row lit", 5, false, lit), ("a pane renamed", 5, true, lit)]
        {
            let (n, _) = counted(|| frame(seq, renamed, pointer));
            assert_eq!(n, 0, "a frame after {what} allocated {n} times");
        }
        // the pointer swept across every button, row and strip, and off
        // them, with no frame in between
        let sweep: Vec<(f64, f64)> =
            (0..400).map(|k| ((k * 37 % 1800) as f64 + 0.5, (k * 53 % 1100) as f64 + 0.5)).collect();
        let (n, lit_changes) = counted(|| {
            let mut changes = 0;
            for &p in &sweep {
                changes += keep.chrome.pointer_move(&mut font, &shape, p) as u32;
            }
            changes
        });
        assert!(lit_changes > 10, "the sweep has to have crossed controls: {lit_changes}");
        assert_eq!(n, 0, "moving the pointer over the chrome allocated {n} times");
    });
}
