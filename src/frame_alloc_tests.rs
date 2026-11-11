//! A terminal frame allocates nothing once its glyphs are cached.
//!
//! Counted, not read for: an allocator in this test binary tallies what
//! the measuring thread allocates while a frame is built.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = COUNTING.try_with(|c| {
            if c.get() {
                let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = COUNTING.try_with(|c| {
            if c.get() && new_size > layout.size() {
                let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
            }
        });
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn counted<T>(f: impl FnOnce() -> T) -> (usize, T) {
    ALLOCS.with(|a| a.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (ALLOCS.with(|a| a.get()), out)
}

#[test]
fn the_meter_sees_an_allocation() {
    let (n, v) = counted(|| vec![1u8; 64]);
    assert_eq!(v.len(), 64);
    assert!(n >= 1, "a Vec was allocated and the meter read {n}");
}

/// The second build of an unchanged screen, with the pane's cache
/// invalidated so every cell is walked again: what a frame costs after
/// new output, once every glyph on screen is in the atlas.
#[test]
fn a_terminal_frame_allocates_nothing_once_warm() {
    use crate::font_cache::FontCache;
    use crate::layout::Layout as PaneLayout;
    use crate::render::SessionView;
    use crate::render_metal::{new_atlas, system_default_device};
    use crate::terminal::Terminal;

    let Ok(device) = system_default_device() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas = new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 2048, 2048, true, font.font_table()).map(|(a, _)| a).expect("colour atlas");

    let (cols, rows) = (100u16, 30u16);
    let mut term = Terminal::new(cols, rows);
    for i in 0..rows - 2 {
        term.feed(
            format!(
                "\x1b[3{}m{i:>3}\x1b[0m plain ascii text, \x1b[1mbold\x1b[0m 漢字かな ①② 🚀 \
                 \u{1f1ef}\u{1f1f5} \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} e\u{301} \
                 https://example.com/x ╭──╮ ▇\r\n",
                1 + i % 6
            )
            .as_bytes(),
        );
    }
    let layout = PaneLayout::build(
        font.cell_w * cols as f64,
        font.cell_h * rows as f64,
        0.0,
        0.0,
        0.0,
        1,
        1,
        font.cell_w,
        font.cell_h,
    );
    let view = |seq: u64| SessionView {
        grid: term.grid(),
        view_offset: 0,
        cursor_visible: true,
        focused: true,
        title: "",
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
        seq,
    };
    let (mut cells, mut glyphs, mut color_glyphs) = (Vec::new(), Vec::new(), Vec::new());
    let (mut dots, mut ui_rects, mut caches) = (Vec::new(), Vec::new(), Vec::new());
    let (mut ov_cells, mut ov_glyphs, mut ov_rects) = (Vec::new(), Vec::new(), Vec::new());
    let mut frame = |seq: u64| {
        // what the renderer does before every build
        for v in [&mut cells, &mut ov_cells, &mut dots] {
            v.clear();
        }
        for v in [&mut glyphs, &mut color_glyphs, &mut ov_glyphs] {
            v.clear();
        }
        ui_rects.clear();
        ov_rects.clear();
        let v = view(seq);
        crate::frame_build::frame::build_instances(
            &layout,
            std::slice::from_ref(&v),
            &[],
            0,
            true,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            &mut font,
            &mut atlas,
            &mut color_atlas,
            &mut cells,
            &mut glyphs,
            &mut color_glyphs,
            &mut dots,
            &mut ui_rects,
            &mut caches,
            &mut ov_cells,
            &mut ov_glyphs,
            &mut ov_rects,
        );
    };
    frame(1);
    frame(2);
    let (n, ()) = counted(|| frame(3));
    assert_eq!(n, 0, "a warm terminal frame allocated {n} times");
}

/// What one frame of the settings panel costs when it is painted
/// directly, every frame, as it is today: the number a retained panel
/// has to be compared with.
///
/// Nothing is asserted about the count beyond the meter having seen
/// the path; the reading is printed.
#[test]
fn settings_immediate_frame() {
    use crate::font_cache::FontCache;
    use crate::render_metal::{new_atlas, system_default_device};
    use crate::ui::components::settings_paint::{SettingsRender, push_settings_panel_via_view};
    use crate::ui::components::{controls, settings_modal};

    let Ok(device) = system_default_device() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let (atlas_w, atlas_h) = atlas.dims();

    let (window_w, window_h) = (1000.0, 1200.0);
    let settings = crate::settings::Settings {
        reclaim_enabled: false,
        reclaim_prefetch: false,
        appearance_circled_wide: false,
        dim_scale: 1.0,
        scroll_factor: 1.0,
        cc_statusline_hook: true,
        ..Default::default()
    };
    let rect = {
        let mut measure = controls::font_measure(&mut font);
        settings_modal::panel_rect(window_w, window_h, &settings, &mut measure, 0.0)
    };
    let panel = SettingsRender {
        rect,
        scroll: 0.0,
        settings,
        path: "/Users/you/Library/Application Support/app/settings.toml".to_string(),
        hover: settings_modal::Hover::None,
    };

    let (cell_w, cell_h, ascent) = (font.cell_w as f32, font.cell_h as f32, font.ascent as f32);
    let (mut cells, mut glyphs, mut ui_rects) = (Vec::new(), Vec::new(), Vec::new());
    let mut frame = |font: &mut FontCache, atlas: &mut crate::glyph_atlas::GlyphAtlas| {
        cells.clear();
        glyphs.clear();
        ui_rects.clear();
        push_settings_panel_via_view(
            &panel, 0.0, cell_w, cell_h, ascent, atlas_w as f32, atlas_h as f32,
            window_w, window_h, font, atlas, &mut cells, &mut glyphs, &mut ui_rects,
        );
        (cells.len(), glyphs.len(), ui_rects.len())
    };
    for _ in 0..3 {
        frame(&mut font, &mut atlas);
    }
    let (n, (n_cells, n_glyphs, n_rects)) = counted(|| frame(&mut font, &mut atlas));

    let mut times: Vec<u128> = (0..30)
        .map(|_| {
            let t = std::time::Instant::now();
            std::hint::black_box(frame(&mut font, &mut atlas));
            t.elapsed().as_nanos()
        })
        .collect();
    times.sort_unstable();
    let median_us = times[times.len() / 2] as f64 / 1000.0;

    eprintln!(
        "[frame-alloc] settings panel, painted directly: {n} allocations a frame, \
         {median_us:.1} us median of 30, for {n_cells} flat rects, {n_rects} rounded rects, \
         {n_glyphs} glyphs"
    );
    assert!(n_glyphs > 50 && n_rects > 3, "the panel has to have been painted");
    assert!(n > 0, "a path that collects its slots into a Vec read as allocation-free");
}

/// A run of text drawn as a tree node costs what the same run costs
/// drawn by hand, and describing and laying the node out costs
/// nothing: the tree adds no allocation of its own to a frame.
#[test]
fn a_tree_text_node_adds_no_allocation_to_the_run_it_draws() {
    use crate::font_cache::FontCache;
    use crate::render_metal::{new_atlas, system_default_device};
    use crate::ui::core::ViewPainter;
    use crate::ui::theme::PanelText;
    use crate::ui::tree::{FontMeasure, Tone, TreeOut, font_of, metrics};
    use golia_ui_core::{Align, Constraints, Interaction, Layout as TreeLayout, Length, Tree, paint};
    use marspot_term::layout::{Alignment, Rect};

    let Ok(device) = system_default_device() else {
        eprintln!("SKIPPED: no Metal device");
        return;
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let (atlas_w, atlas_h) = atlas.dims();
    let (cell_w, cell_h, ascent) = (font.cell_w as f32, font.cell_h as f32, font.ascent as f32);
    let (mut cells, mut glyphs, mut ui_rects) = (Vec::new(), Vec::new(), Vec::new());
    macro_rules! painter {
        () => {
            ViewPainter {
                cell_w,
                cell_h,
                ascent,
                atlas_w: atlas_w as f32,
                atlas_h: atlas_h as f32,
                window_w: 2000.0,
                window_h: 1000.0,
                font: &mut font,
                atlas: &mut atlas,
                cells: &mut cells,
                glyphs: &mut glyphs,
                ui_rects: &mut ui_rects,
            }
        };
    }

    let (role, s) = (PanelText::Label, "Reclaim idle panes 日本語");
    let rect = Rect { x: 40.0, y_top: 60.0, w: 900.0, h: 44.0 };
    let by_hand = |p: &mut ViewPainter<'_>| p.panel_text_in(role, rect, s, Tone::Fg.rgba(), Alignment::CenterLeft);
    let (mut tree, mut layout, state) = (Tree::new(), TreeLayout::new(), Interaction::default());
    let mut lay_out = |p: &mut ViewPainter<'_>| {
        tree.build(|ui| {
            ui.text(font_of(role), Tone::Fg.into(), s)
                .width(Length::Px(rect.w))
                .height(Length::Px(rect.h))
                .align(Align::Leading);
        });
        let m = metrics(p.font);
        layout.run(&tree, (rect.x, rect.y_top), Constraints::loose(2000.0, 1000.0), &m, &FontMeasure::new(&mut *p.font));
        m
    };

    for _ in 0..3 {
        by_hand(&mut painter!());
        lay_out(&mut painter!());
    }
    glyphs.clear();
    let (hand_n, ()) = counted(|| by_hand(&mut painter!()));
    let drawn_by_hand = std::mem::take(&mut glyphs);
    glyphs.reserve(drawn_by_hand.len());

    let (layout_n, m) = counted(|| lay_out(&mut painter!()));
    let (paint_n, ()) = counted(|| paint(&tree, &layout, &state, &m, &mut TreeOut::new(&mut painter!())));

    eprintln!(
        "[frame-alloc] one run of {} glyphs: {hand_n} allocations drawn by hand; as a tree node \
         {layout_n} to describe and lay out, {paint_n} to paint",
        drawn_by_hand.len()
    );
    assert!(!drawn_by_hand.is_empty() && glyphs == drawn_by_hand, "the node drew another run");
    assert_eq!(layout_n, 0, "describing and laying out a warm tree allocated");
    assert_eq!(paint_n, hand_n, "painting the node cost more than drawing the run by hand");
}
