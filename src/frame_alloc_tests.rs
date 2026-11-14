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

pub(crate) fn counted<T>(f: impl FnOnce() -> T) -> (usize, T) {
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

/// A surface that keeps what it painted: a frame in which nothing
/// changed describes nothing, lays nothing out, paints nothing and
/// takes nothing from the heap, and is the same instances; a pointer
/// moving onto a control repaints and does not lay out.
#[test]
fn a_kept_surface_frame_does_only_what_went_stale() {
    use crate::font_cache::FontCache;
    use crate::render_metal::{new_atlas, system_default_device};
    use crate::ui::core::ViewPainter;
    use crate::ui::theme::spec::space;
    use crate::ui::tree::kept::Place;
    use crate::ui::tree::KeptSurface;
    use crate::ui::tree::preset_parity_tests::sheet;
    use golia_ui_core::Counters;

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
    let size = (1280.0, 700.0);
    let mut kept = KeptSurface::new();
    let mut frame = |kept: &mut KeptSurface, data_rev: u64| {
        cells.clear();
        glyphs.clear();
        ui_rects.clear();
        let mut p = ViewPainter {
            cell_w,
            cell_h,
            ascent,
            atlas_w: atlas_w as f32,
            atlas_h: atlas_h as f32,
            window_w: 1400.0,
            window_h: 900.0,
            font: &mut font,
            atlas: &mut atlas,
            cells: &mut cells,
            glyphs: &mut glyphs,
            ui_rects: &mut ui_rects,
        };
        let at = Place { data_rev, origin: (60.0, 100.0), avail: size };
        kept.frame(&mut p, at, None, |ui, fm| sheet(ui, fm, size, space::HEADING_TO_CARD));
        (cells.len(), glyphs.clone(), ui_rects.clone())
    };
    let count = |c: Counters| (c.build, c.layout, c.paint);

    // twice with new data: the buffers have grown to what the sheet needs
    frame(&mut kept, 1);
    let first = frame(&mut kept, 2);
    assert_eq!(count(kept.surface.counters()), (2, 2, 2));
    assert!(first.1.len() > 100 && first.2.len() > 20, "the sheet was painted");

    let (idle_n, idle) = counted(|| frame(&mut kept, 2));
    // the clones this test takes of the frame are its own
    let own = 2;
    assert_eq!(count(kept.surface.counters()), (2, 2, 2), "an idle frame ran a stage");
    assert_eq!(idle_n, own, "an idle frame allocated {} times", idle_n - own);
    assert!(idle == first, "the kept instances are not the frame that was painted");

    // the pointer onto the first button
    let on = kept.surface.tree().nodes().iter().position(|n| n.leaf_tag == crate::ui::tree::preset::BUTTON);
    let r = kept.surface.layout().rect(golia_ui_core::NodeId(on.expect("a button") as u32));
    assert!(kept.surface.pointer_move((r.x + 4.0, r.y + 4.0)), "the pointer is on nothing");
    let (hover_n, hover) = counted(|| frame(&mut kept, 2));
    assert_eq!(count(kept.surface.counters()), (2, 2, 3), "a hover is a repaint and nothing else");
    assert!(hover.2 != first.2, "the hovered button kept its fill");
    assert_eq!(hover.1, first.1, "a hover moved a glyph");

    let (data_n, _) = counted(|| frame(&mut kept, 3));
    assert_eq!(count(kept.surface.counters()), (3, 3, 4));
    eprintln!(
        "[frame-alloc] kept surface, {} nodes, {} glyphs, {} rounded rects: idle frame {} allocations, \
         hover frame {}, new-data frame {}",
        kept.surface.tree().len(),
        first.1.len(),
        first.2.len(),
        idle_n - own,
        hover_n - own,
        data_n - own,
    );
}

/// What a frame of the settings panel costs drawn through its surface:
/// nothing at all while nothing changes, a repaint when the pointer
/// moves onto a control, and all three stages when a setting changes.
/// The readings are printed beside the counts that are asserted.
#[test]
fn settings_surface_frame() {
    use crate::font_cache::FontCache;
    use crate::render_metal::{new_atlas, system_default_device};
    use crate::ui::components::settings_view::SettingsRender;
    use crate::ui::components::{controls, settings_modal};
    use crate::ui::core::ViewPainter;
    use crate::ui::tree::preset;
    use golia_ui_core::{Counters, NodeId};
    use std::sync::Arc;

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
    let path = "/Users/you/Library/Application Support/app/settings.toml";
    let at_rest = SettingsRender::new(rect, 0.0, settings.clone(), path);
    // the two values the data alternates between once warm
    let other = SettingsRender {
        settings: Arc::new(crate::settings::Settings { dim_scale: 0.6, ..settings.clone() }),
        ..at_rest.clone()
    };

    let (cell_w, cell_h, ascent) = (font.cell_w as f32, font.cell_h as f32, font.ascent as f32);
    let (mut cells, mut glyphs, mut ui_rects) = (Vec::new(), Vec::new(), Vec::new());
    let mut frame = |sp: &SettingsRender| {
        cells.clear();
        glyphs.clear();
        ui_rects.clear();
        let mut p = ViewPainter {
            cell_w,
            cell_h,
            ascent,
            atlas_w: atlas_w as f32,
            atlas_h: atlas_h as f32,
            window_w,
            window_h,
            font: &mut font,
            atlas: &mut atlas,
            cells: &mut cells,
            glyphs: &mut glyphs,
            ui_rects: &mut ui_rects,
        };
        sp.surface.borrow_mut().frame(&mut p, sp, 0.0);
        (cells.len(), glyphs.len(), ui_rects.len())
    };
    let counters = |sp: &SettingsRender| sp.surface.borrow().kept.surface.counters();
    let stages = |c: Counters| (c.build, c.layout, c.paint);

    // both values twice: every buffer has grown to what either needs
    for sp in [&at_rest, &other, &at_rest, &other, &at_rest] {
        frame(sp);
    }
    let warm = counters(&at_rest);
    assert_eq!(stages(warm), (5, 5, 5));

    let (idle_n, drawn) = counted(|| frame(&at_rest));
    assert_eq!(counters(&at_rest), warm, "a frame with nothing new ran a stage");
    assert_eq!(idle_n, 0, "a frame with nothing new allocated {idle_n} times");
    assert!(drawn.1 > 50 && drawn.2 > 3, "the panel has to have been painted");

    // the pointer onto a segment of the first segmented control in use
    let on = {
        let s = at_rest.surface.borrow();
        let tree = s.kept.surface.tree();
        let i = tree
            .nodes()
            .iter()
            .position(|n| n.leaf_tag == preset::SEGMENT && !n.has(golia_ui_core::node::flag::DISABLED))
            .expect("a segment");
        let r = s.kept.surface.layout().rect(NodeId(i as u32));
        (r.x + r.w * 0.5, r.y + r.h * 0.5)
    };
    assert!(at_rest.surface.borrow_mut().kept.surface.pointer_move(on), "the pointer is on nothing");
    let (hover_n, _) = counted(|| frame(&at_rest));
    let hovered = counters(&at_rest);
    assert_eq!(
        (hovered.build, hovered.layout, hovered.paint),
        (warm.build, warm.layout, warm.paint + 1),
        "a hover is a repaint and nothing else"
    );
    at_rest.surface.borrow_mut().kept.surface.pointer_leave();
    frame(&at_rest);

    let (data_n, _) = counted(|| frame(&other));
    let changed = counters(&at_rest);
    assert_eq!((changed.build, changed.layout), (hovered.build + 1, hovered.layout + 1));

    let median = |times: &mut Vec<u128>| {
        times.sort_unstable();
        times[times.len() / 2] as f64 / 1000.0
    };
    let mut idle: Vec<u128> = (0..30)
        .map(|_| {
            let t = std::time::Instant::now();
            std::hint::black_box(frame(&other));
            t.elapsed().as_nanos()
        })
        .collect();
    let mut rebuilt: Vec<u128> = (0..30)
        .map(|k| {
            let sp = if k % 2 == 0 { &at_rest } else { &other };
            let t = std::time::Instant::now();
            std::hint::black_box(frame(sp));
            t.elapsed().as_nanos()
        })
        .collect();
    let nodes = at_rest.surface.borrow().kept.surface.tree().len();
    eprintln!(
        "[frame-alloc] settings panel through its surface, {nodes} nodes, {} flat rects, {} rounded \
         rects, {} glyphs: idle frame {idle_n} allocations, {:.1} us median of 30; hover frame \
         {hover_n} allocations; new-data frame {data_n} allocations, {:.1} us median of 30",
        drawn.0,
        drawn.2,
        drawn.1,
        median(&mut idle),
        median(&mut rebuilt),
    );
}
