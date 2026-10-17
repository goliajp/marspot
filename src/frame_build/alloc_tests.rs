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
        eprintln!("skip (no Metal)");
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
        update_pending: false,
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
        super::frame::build_instances(
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
