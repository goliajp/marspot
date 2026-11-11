//! How much does building one frame of chrome allocate?
//!
//! The project's rule is that hot paths allocate nothing.  This counts:
//! a global allocator in this test binary tallies every allocation
//! that happens inside a measured region, and the region is a frame of
//! the arena tree, whose budget is zero.
//!
//! What is asserted throughout is that the meter works: a control
//! region with a known allocation in it must register, or a silently
//! broken counter would read as "zero allocations" — which is exactly
//! the answer we want to be true, and therefore the one we must not be
//! able to fake.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

// Per-thread, not global: the test harness runs tests concurrently in
// one process, and a global tally counts a sibling test's allocations
// as this frame's.  Measured in the sibling crate: a frame that
// allocates nothing read 4.  `const` init because a lazily initialised
// thread-local would allocate from inside the allocator.
thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

fn note(size: usize) {
    let _ = COUNTING.try_with(|c| {
        if c.get() {
            let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
            let _ = BYTES.try_with(|b| b.set(b.get() + size));
        }
    });
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A realloc is a grow that could have been reserved for, so it
        // counts: it is the signature of a Vec that did not know its
        // own size.
        note(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `f` with the tally on, and return `(allocations, bytes)`.
///
/// Only this thread's allocations, so a sibling test running at the
/// same time cannot be mistaken for this frame's work.
fn measured<T>(f: impl FnOnce() -> T) -> (usize, usize, T) {
    ALLOCS.with(|a| a.set(0));
    BYTES.with(|b| b.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (ALLOCS.with(|a| a.get()), BYTES.with(|b| b.get()), out)
}

#[test]
fn the_meter_registers_a_known_allocation() {
    let (n, bytes, _) = measured(|| Box::new([0u8; 64]));
    assert!(
        n >= 1 && bytes >= 64,
        "a counter that cannot see one Box would report every frame as free: {n} allocs, {bytes} B"
    );

    let (n, _, _) = measured(|| std::hint::black_box(1u64 + 1));
    assert_eq!(n, 0, "and it must not invent allocations that did not happen");
}

mod arena {
    use golia_ui_core::{
        Action, Edges, FakeMeasure, FontSpec, Ink, Inputs, Length as L, Metrics, Recording,
        Surface, TextFont, Ui, WidgetId,
    };

    pub const MEASURE: FakeMeasure = FakeMeasure::new(2.0);
    const BODY: TextFont = TextFont::Ui(FontSpec { size_q: 32, weight: 400, features: 0 });
    const MINI: TextFont = TextFont::Ui(FontSpec { size_q: 20, weight: 400, features: 0 });

    pub const ROWS: u32 = 40;

    /// A column of rows, each a label, a spacer and a detail, padded
    /// and framed.  The detail is formatted at build time from
    /// `salt`, so two builds with different salts are a change of data
    /// and not the same strings again.
    pub fn describe(ui: &mut Ui<'_>, rows: u32, salt: u32) {
        ui.vstack(|ui| {
            for i in 0..rows {
                ui.hstack(|ui| {
                    ui.text(BODY, Ink::Role(0), "label");
                    ui.spacer();
                    ui.text_fmt(MINI, Ink::Role(1), format_args!("detail {:02}", (i + salt) % 100));
                })
                .padding(Edges::xy(L::Pt(6.0), L::Pt(2.0)))
                .width(L::Pct(1.0))
                .height(L::Pt(if i % 7 == 0 { 24.0 } else { 20.0 }))
                .fill(Ink::Role(2))
                .fill_hover(Ink::Role(3))
                .widget(WidgetId(i), Action::new(1, i));
            }
        });
    }

    pub fn inputs(data_rev: u64) -> Inputs {
        Inputs {
            data_rev,
            theme_rev: 1,
            origin: (0.0, 0.0),
            avail: (800.0, 2000.0),
            metrics: Metrics { px_per_pt: 2.0, cell_w: 16.0, cell_h: 32.0, cell_ascent: 24.0 },
            font_rev: 1,
            atlas_gen: 1,
        }
    }

    /// A surface that has been through enough frames for its buffers
    /// to have reached the size this tree needs.
    pub fn warm() -> Surface<Recording> {
        let mut s = Surface::new(Recording::new());
        for rev in 1..=3 {
            s.frame(&inputs(rev), &MEASURE, |ui| describe(ui, ROWS, rev as u32));
        }
        s
    }
}

/// The decision the arena tree rests on: describing, laying out and
/// painting a tree again takes nothing from the heap.
///
/// Each phase is measured on its own, after three warm-up frames, with
/// different data from the frame before.
#[test]
fn arena_tree_rebuild_is_free() {
    use arena::{MEASURE, ROWS, describe, inputs, warm};
    use golia_ui_core::{Node, Rect, TextBox};

    let mut s = warm();
    let before = s.counters();

    let (build_n, build_b, ()) = measured(|| s.describe(|ui| describe(ui, ROWS, 57)));
    let (layout_n, layout_b, ()) = measured(|| s.lay_out(&inputs(4), &MEASURE));
    let (paint_n, paint_b, ()) = measured(|| s.paint());

    let after = s.counters();
    let nodes = s.tree().len();
    let ops = s.output().ops.len();
    let node_bytes = std::mem::size_of::<Node>();
    let layout_bytes = 2 * std::mem::size_of::<Rect>() + std::mem::size_of::<TextBox>();

    // the whole frame through the entry point a panel would call
    let (frame_n, _, ()) = measured(|| {
        s.frame(&inputs(5), &MEASURE, |ui| describe(ui, ROWS, 91));
    });
    let after_frame = s.counters();

    let mut times: Vec<u128> = (0..30u32)
        .map(|k| {
            let t = std::time::Instant::now();
            s.describe(|ui| describe(ui, ROWS, k));
            s.lay_out(&inputs(6), &MEASURE);
            s.paint();
            t.elapsed().as_nanos()
        })
        .collect();
    times.sort_unstable();
    let median_us = times[times.len() / 2] as f64 / 1000.0;

    eprintln!(
        "[frame-alloc] arena tree, {ROWS} rows → {nodes} nodes, {ops} paint ops\n\
         [frame-alloc]   build  {build_n:>6} allocs {build_b:>8} B\n\
         [frame-alloc]   layout {layout_n:>6} allocs {layout_b:>8} B\n\
         [frame-alloc]   paint  {paint_n:>6} allocs {paint_b:>8} B\n\
         [frame-alloc]   {node_bytes} B a node, {layout_bytes} B of layout a node, \
         {} B of text; rebuild + layout + paint {median_us:.1} us (median of 30)",
        s.tree().text_bytes(),
    );

    // each phase was entered, and did the work of a whole tree
    assert_eq!(nodes, 1 + ROWS as usize * 4);
    assert_eq!(ops, ROWS as usize * 3, "a fill and two runs of text a row");
    assert_eq!(
        (after.build - before.build, after.layout - before.layout, after.paint - before.paint),
        (1, 1, 1)
    );

    assert_eq!((build_n, build_b), (0, 0), "describing a warm tree allocated");
    assert_eq!((layout_n, layout_b), (0, 0), "laying out a warm tree allocated");
    assert_eq!((paint_n, paint_b), (0, 0), "painting a warm tree allocated");
    assert_eq!(frame_n, 0, "a frame with new data allocated");
    assert_eq!(
        (after_frame.build, after_frame.layout, after_frame.paint),
        (after.build + 1, after.layout + 1, after.paint + 1),
        "and that frame did describe, lay out and paint"
    );

    // the meter sees this path: a tree twice the size has to grow the
    // buffers, and that is counted
    let (grow_n, _, ()) = measured(|| s.describe(|ui| describe(ui, ROWS * 2, 0)));
    assert!(grow_n > 0, "growing the arena read as free, so the zeros above mean nothing");
    let (one, _, _) = measured(|| Box::new([0u8; 64]));
    assert!(one >= 1, "a deliberate allocation was not counted");
}

/// The frames a surface sees most: nothing changed, the pointer moved
/// onto another widget, the window was resized.  None allocates, and
/// each runs only the stages its change made stale.
#[test]
fn arena_surface_frames_run_only_what_is_stale_and_allocate_nothing() {
    use arena::{MEASURE, ROWS, describe, inputs, warm};
    use golia_ui_core::{Counters, Inputs};

    let mut s = warm();
    let same = inputs(3);
    // one pass through each kind of frame first, as a surface in use has had
    s.pointer_move((10.0, 10.0));
    s.frame(&same, &MEASURE, |ui| describe(ui, ROWS, 3));
    s.frame(&Inputs { avail: (700.0, 2000.0), ..same }, &MEASURE, |ui| describe(ui, ROWS, 3));
    s.frame(&same, &MEASURE, |ui| describe(ui, ROWS, 3));
    let c0 = s.counters();
    let kept = s.output().clone();

    let (idle_n, _, ()) = measured(|| {
        s.frame(&same, &MEASURE, |ui| describe(ui, ROWS, 3));
    });
    assert_eq!(idle_n, 0, "an idle frame allocated");
    assert_eq!(s.counters(), c0, "an idle frame built, laid out or painted");
    assert_eq!(*s.output(), kept, "an idle frame is the kept output");

    assert!(s.pointer_move((10.0, 100.0)), "the pointer has to have reached another row");
    let (hover_n, _, ()) = measured(|| {
        s.frame(&same, &MEASURE, |ui| describe(ui, ROWS, 3));
    });
    let c1 = s.counters();
    assert_eq!(hover_n, 0, "a hover frame allocated");
    assert_eq!(c1, Counters { paint: c0.paint + 1, ..c0 }, "a hover is a repaint and no more");
    assert_ne!(*s.output(), kept, "and the picture did change");

    let resized = Inputs { avail: (640.0, 2000.0), ..same };
    let (size_n, _, ()) = measured(|| {
        s.frame(&resized, &MEASURE, |ui| describe(ui, ROWS, 3));
    });
    let c2 = s.counters();
    assert_eq!(size_n, 0, "a resize frame allocated");
    assert_eq!(
        c2,
        Counters { layout: c1.layout + 1, paint: c1.paint + 1, ..c1 },
        "a resize lays out and paints the tree it already has"
    );

    // the build counter is not stuck: new data moves it
    s.frame(&Inputs { data_rev: 9, ..resized }, &MEASURE, |ui| describe(ui, ROWS, 9));
    assert_eq!(s.counters().build, c2.build + 1);

    let (one, _, _) = measured(|| Box::new([0u8; 64]));
    assert!(one >= 1, "a deliberate allocation was not counted");
}

/// What a tree's layout asks of the fonts, asked a second time: every
/// answer is already known, and none of them touches the heap.
///
/// The first asking is the control: it shapes each string and reads
/// its ink, and has to be seen allocating.  So does a cut at a width
/// nobody asked for before.
#[test]
fn text_measured_again_for_a_tree_is_free() {
    use golia_ui_core::{FontSpec, TextMeasure};
    use marspot::font_cache::FontCache;
    use marspot::ui::theme::PanelText;
    use marspot::ui::tree::FontMeasure;

    let mut font = FontCache::build().expect("font");
    let m = FontMeasure::new(&mut font);
    let roles = [PanelText::Title, PanelText::Label, PanelText::Item, PanelText::Caption]
        .map(|r| FontSpec::new(r.pt(), r.weight()));
    let strings = ["Reclaim idle panes", "Wheel speed", "日本語", "简体中文", "アイドル状態"];
    let long = "Give an idle pane's memory back after it has sat unused for a while";
    let narrow = m.advance(long, roles[1]) * 0.4;

    let ask = |m: &FontMeasure<'_>| {
        let mut sum = 0.0;
        for font in roles {
            for s in strings {
                let by = m.centring(s, font);
                sum += m.advance(s, font) + by.above + by.below;
            }
            let fm = m.metrics(font);
            let fit = m.fit_prefix(long, font, narrow);
            sum += fm.cap_h + fm.line_h + fit.width + fit.len as f64;
        }
        sum + m.mono_cells("a日b") as f64
    };
    let (cold, _, first) = measured(|| ask(&m));
    let (warm, bytes, again) = measured(|| ask(&m));
    assert!(cold > 0, "the first measurement shapes every string, and the meter read nothing");
    assert_eq!(again, first, "the second asking got other answers");
    assert_eq!((warm, bytes), (0, 0), "measuring text already measured allocated");

    let (cut, _, fit) = measured(|| m.fit_prefix(long, roles[1], narrow * 0.5));
    assert!(fit.len < long.len() && cut > 0, "a cut never made before read as free: {fit:?}, {cut} allocations");
    let (cut_again, _, same) = measured(|| m.fit_prefix(long, roles[1], narrow * 0.5));
    assert_eq!((cut_again, same), (0, fit), "the same cut a second time");

    eprintln!(
        "[frame-alloc] text measured for a tree: {cold} allocations the first time, {warm} the second, \
         over {} strings in {} roles with a cut each",
        strings.len(),
        roles.len()
    );
}
