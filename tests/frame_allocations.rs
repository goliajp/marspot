//! How much does building one frame of chrome allocate?
//!
//! The project's rule is that hot paths allocate nothing, and the GUI
//! work ahead of us has to hold that line while the chrome moves onto
//! the declarative tree.  There was no instrument for it: "allocates
//! nothing" was an intention, checked by reading.
//!
//! This counts.  A global allocator in this test binary tallies every
//! allocation that happens inside a measured region, and the region is
//! one build of the dev panel's view tree — today the only real
//! consumer of that tree, and the shape every other panel is going to
//! take.
//!
//! No budget is asserted for that tree.  A budget invented before the
//! number is known is a number about nothing; those readings establish
//! the number.  The arena tree at the end of this file is the one held
//! to a budget, and its budget is zero.  What is asserted throughout
//! is that the meter works: a control region with a known allocation
//! in it must register,
//! or a silently broken counter would read as "zero allocations" —
//! which is exactly the answer we want to be true, and therefore the
//! one we must not be able to fake.

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

#[test]
fn one_dev_panel_frame() {
    let mut font = match marspot::font_cache::FontCache::build() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("SKIPPED: no font stack: {e}");
            return;
        }
    };
    let state = marspot::ui::components::DevPanelState {
        visible: true,
        origin_pt: (0.0, 0.0),
        size_pt: (420.0, 520.0),
        active_tab: marspot::ui::components::dev_panel::TAB_SESSIONS,
        active_section: marspot::ui::components::dev_panel::SECTION_SESSIONS_ARCHITECTURE,
        scale: 2.0,
    };

    // Warm: the first builds fill the font's own caches, and those are
    // amortised by construction — they are not what a steady frame
    // pays.
    for _ in 0..3 {
        let measure = marspot::chrome_measure::ChromeMeasure::new(&mut font, 16.0, 32.0);
        let c = marspot::ui::components::build_dev_panel_canvas(
            &state, 840.0, 1040.0, 16.0, 32.0, 24.0, &measure,
        );
        std::hint::black_box(&c);
    }

    let mut runs = Vec::new();
    for _ in 0..5 {
        let measure = marspot::chrome_measure::ChromeMeasure::new(&mut font, 16.0, 32.0);
        let (n, bytes, canvas) = measured(|| {
            marspot::ui::components::build_dev_panel_canvas(
                &state, 840.0, 1040.0, 16.0, 32.0, 24.0, &measure,
            )
        });
        runs.push((n, bytes, canvas.primitives().len()));
    }
    runs.sort();
    let (n, bytes, prims) = runs[runs.len() / 2];

    eprintln!(
        "[frame-alloc] dev panel: {n} allocations, {bytes} B, for {prims} primitives \
         (median of 5, after 3 warm-up builds)"
    );
    assert!(
        n > 0,
        "building a tree that produces {prims} primitives without a single allocation \
         would mean the meter is not seeing this path"
    );
}

/// Where a frame's allocations actually come from.
///
/// `one_dev_panel_frame` says the total.  A total is not a plan: the
/// three candidate architectures for the declarative tree each kill a
/// different one of the suspected causes, and picking between them off
/// a reading of the code is how you rewrite the part that was never
/// the cost.
///
/// So this splits one frame into its three phases — build the tree,
/// lay it out, paint it into a canvas — and counts each.  The tree is
/// built here rather than taken from the dev panel because the panel's
/// builders are private to it; what is being attributed is the
/// pipeline, and the pipeline is the same one.
///
/// `MockFontMetrics` on purpose: it makes text measurement free, so
/// whatever is left is structural.  The real provider's cost is the
/// subject of its own reading below.
#[test]
fn where_a_frames_allocations_come_from() {
    use marspot::ui::core::Length as L;
    use marspot::ui::view::{
        Constraints, Edges, FrameSpec, LayoutCtx, MockFontMetrics, Text, UiSize,
        hstack, layout_view, paint_into, spacer, vstack,
    };

    // A tree in the shape the chrome actually builds: a column of
    // rows, each row a couple of labels with a spacer between them,
    // padded and framed.  40 rows ≈ the dev panel's menu plus a
    // section's content.
    let build = || {
        let rows: Vec<_> = (0..40)
            .map(|i| {
                hstack(vec![
                    Text::new("label").ui_size(UiSize::Body).build(),
                    spacer(),
                    Text::new("detail").ui_size(UiSize::Mini).build(),
                ])
                .padding(Edges::xy(L::Pt(6.0), L::Pt(2.0)))
                .frame(FrameSpec {
                    width: Some(L::Pct(1.0)),
                    height: Some(L::Pt(if i % 7 == 0 { 24.0 } else { 20.0 })),
                    ..Default::default()
                })
            })
            .collect();
        vstack(rows)
    };

    let fonts = MockFontMetrics { cell_w_phys: 16.0, cell_h_phys: 32.0 };
    let ctx = LayoutCtx {
        scale: 2.0,
        cell_w_phys: 16.0,
        cell_h_phys: 32.0,
        ascent_phys: 24.0,
        fonts: &fonts,
    };
    let constraints = Constraints::loose(800.0, 2000.0);

    // Warm any lazily-built global the pipeline touches once.
    {
        let v = build();
        let laid = layout_view(&v, ctx, (0.0, 0.0), constraints);
        let mut c = marspot::ui::core::canvas::Canvas::new(
            2.0,
            marspot::ui::core::canvas::ParentRect::window(800.0, 2000.0),
        );
        paint_into(&mut c, &laid, ctx);
        std::hint::black_box(&c);
    }

    let (build_n, build_b, view) = measured(build);
    let (layout_n, layout_b, laid) =
        measured(|| layout_view(&view, ctx, (0.0, 0.0), constraints));
    let mut canvas = marspot::ui::core::canvas::Canvas::new(
        2.0,
        marspot::ui::core::canvas::ParentRect::window(800.0, 2000.0),
    );
    let (paint_n, paint_b, ()) = measured(|| paint_into(&mut canvas, &laid, ctx));

    let prims = canvas.primitives().len();
    eprintln!(
        "[frame-alloc] 40-row tree → {prims} primitives\n\
         [frame-alloc]   build  {build_n:>6} allocs {build_b:>8} B\n\
         [frame-alloc]   layout {layout_n:>6} allocs {layout_b:>8} B\n\
         [frame-alloc]   paint  {paint_n:>6} allocs {paint_b:>8} B"
    );

    // The one thing asserted is that each phase was actually entered.
    // A phase reported as free because it was skipped is the failure
    // this whole measurement exists to avoid.
    assert!(prims > 40, "the tree has to have been painted: {prims} primitives");
    assert!(build_n > 0 && layout_n > 0, "build {build_n}, layout {layout_n}");
}

/// Layout is the phase that costs; this asks which half of it.
///
/// Two suspects, and they scale differently.  Cloning the `View` into
/// every `LaidOut` is linear in node count.  Laying each child out
/// twice — once to measure, once to place — doubles per level of
/// nesting, so the same leaves cost 2^depth.
///
/// The trees below hold the leaf count fixed and vary only the depth
/// they are wrapped in.  Linear growth acquits the double pass;
/// doubling convicts it.
#[test]
fn which_half_of_layout_costs() {
    use marspot::ui::view::{
        Constraints, LayoutCtx, MockFontMetrics, Text, UiSize, layout_view, vstack,
    };

    let fonts = MockFontMetrics { cell_w_phys: 16.0, cell_h_phys: 32.0 };
    let ctx = LayoutCtx {
        scale: 2.0,
        cell_w_phys: 16.0,
        cell_h_phys: 32.0,
        ascent_phys: 24.0,
        fonts: &fonts,
    };

    // 32 leaves every time.  At depth d they are split into 2^d groups
    // nested d deep, so the node count barely moves and the nesting
    // does.
    fn nest(depth: u32, leaves: usize) -> marspot::ui::view::View {
        if depth == 0 {
            return vstack(
                (0..leaves)
                    .map(|_| Text::new("label").ui_size(UiSize::Body).build())
                    .collect(),
            );
        }
        vstack(vec![nest(depth - 1, leaves / 2), nest(depth - 1, leaves / 2)])
    }

    let mut readings = Vec::new();
    for depth in 0..=4 {
        let v = nest(depth, 32);
        let c = Constraints::loose(800.0, 4000.0);
        // Warm, then measure.
        std::hint::black_box(layout_view(&v, ctx, (0.0, 0.0), c));
        let (n, b, laid) = measured(|| layout_view(&v, ctx, (0.0, 0.0), c));
        std::hint::black_box(&laid);
        readings.push((depth, n, b));
    }

    for (depth, n, b) in &readings {
        eprintln!("[frame-alloc] layout depth {depth}: {n:>7} allocs {b:>9} B (32 leaves)");
    }

    let flat = readings[0].1;
    let deep = readings[4].1;
    assert!(flat > 0, "the flat tree has to have been laid out");
    eprintln!(
        "[frame-alloc] depth 0 → 4 with the same 32 leaves: {flat} → {deep} allocations \
         ({:.1}×)",
        deep as f64 / flat as f64
    );
}

/// What the real font provider adds on top of the structural cost.
///
/// Every reading above mocks text measurement to free, so they are a
/// floor.  This lays out the same tree twice — once with the mock,
/// once with the provider the chrome actually uses — and the
/// difference is what measuring text costs per frame.
#[test]
fn what_measuring_text_costs() {
    use marspot::ui::view::{
        Constraints, LayoutCtx, MockFontMetrics, Text, UiSize, layout_view, vstack,
    };

    let mut font = match marspot::font_cache::FontCache::build() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("SKIPPED: no font stack: {e}");
            return;
        }
    };

    // Distinct strings: one shared string would measure a cache that
    // a real panel never has.
    let labels: Vec<String> = (0..64).map(|i| format!("row label {i}")).collect();
    let view = vstack(
        labels
            .iter()
            .map(|s| Text::new(s.as_str()).ui_size(UiSize::Body).build())
            .collect(),
    );
    let c = Constraints::loose(800.0, 4000.0);

    let mock = MockFontMetrics { cell_w_phys: 16.0, cell_h_phys: 32.0 };
    let mock_ctx = LayoutCtx {
        scale: 2.0, cell_w_phys: 16.0, cell_h_phys: 32.0, ascent_phys: 24.0, fonts: &mock,
    };
    std::hint::black_box(layout_view(&view, mock_ctx, (0.0, 0.0), c));
    let (mock_n, mock_b, _) = measured(|| layout_view(&view, mock_ctx, (0.0, 0.0), c));

    let measure = marspot::chrome_measure::ChromeMeasure::new(&mut font, 16.0, 32.0);
    let real_ctx = LayoutCtx {
        scale: 2.0, cell_w_phys: 16.0, cell_h_phys: 32.0, ascent_phys: 24.0, fonts: &measure,
    };
    // Warm twice: whatever the provider caches, a steady frame has it.
    for _ in 0..2 {
        std::hint::black_box(layout_view(&view, real_ctx, (0.0, 0.0), c));
    }
    let (real_n, real_b, _) = measured(|| layout_view(&view, real_ctx, (0.0, 0.0), c));

    eprintln!(
        "[frame-alloc] 64 text leaves, layout only:\n\
         [frame-alloc]   mock provider {mock_n:>6} allocs {mock_b:>8} B\n\
         [frame-alloc]   real provider {real_n:>6} allocs {real_b:>8} B\n\
         [frame-alloc]   measuring text costs {} allocs {} B per frame, warm",
        real_n as i64 - mock_n as i64,
        real_b as i64 - mock_b as i64
    );
    assert!(mock_n > 0 && real_n > 0, "mock {mock_n}, real {real_n}");
}

/// What building the tree costs, by what the node is made of.
///
/// After layout stopped laying children out twice and stopped carrying
/// copies of its subtree, building the tree is the largest phase of
/// the 40-row tree above: 201 of its 769.
///
/// Note which tree that is.  `one_dev_panel_frame` measures a
/// different, smaller tree and reports one number for all three
/// phases; its split has never been measured, because the panel's
/// builders are private to it.  Reading 201 against the dev panel's
/// 215 — as an earlier version of this comment did — puts two trees'
/// numbers in one sentence and makes layout look like 14 allocations
/// when it is 402 on the tree it was measured on.
///
/// Four trees of 64 nodes each, differing only in what a node holds.
#[test]
fn what_building_a_tree_costs() {
    use marspot::ui::core::Length as L;
    use marspot::ui::view::{Edges, FrameSpec, Text, UiSize, vstack};

    const N: usize = 64;

    // Warm whatever the first build touches once.
    std::hint::black_box(vstack(
        (0..N).map(|_| Text::new("x").ui_size(UiSize::Body).build()).collect(),
    ));

    let (bare_n, bare_b, _) = measured(|| {
        vstack((0..N).map(|_| Text::new("label").ui_size(UiSize::Body).build()).collect())
    });

    // Owned strings: the same tree, but each label had to be made.
    let owned: Vec<String> = (0..N).map(|i| format!("label {i}")).collect();
    let (owned_n, owned_b, _) = measured(|| {
        vstack(owned.iter().map(|s| Text::new(s.as_str()).ui_size(UiSize::Body).build()).collect())
    });

    // One modifier per node.
    let (pad_n, pad_b, _) = measured(|| {
        vstack(
            (0..N)
                .map(|_| {
                    Text::new("label")
                        .ui_size(UiSize::Body)
                        .build()
                        .padding(Edges::xy(L::Pt(4.0), L::Pt(2.0)))
                })
                .collect(),
        )
    });

    // Two modifiers per node — the shape the chrome actually builds.
    let (two_n, two_b, _) = measured(|| {
        vstack(
            (0..N)
                .map(|_| {
                    Text::new("label")
                        .ui_size(UiSize::Body)
                        .build()
                        .padding(Edges::xy(L::Pt(4.0), L::Pt(2.0)))
                        .frame(FrameSpec { width: Some(L::Pct(1.0)), ..Default::default() })
                })
                .collect(),
        )
    });

    eprintln!(
        "[frame-alloc] building {N} nodes:\n\
         [frame-alloc]   &'static str, no modifier   {bare_n:>5} allocs {bare_b:>8} B\n\
         [frame-alloc]   owned str,    no modifier   {owned_n:>5} allocs {owned_b:>8} B\n\
         [frame-alloc]   &'static str, 1 modifier    {pad_n:>5} allocs {pad_b:>8} B\n\
         [frame-alloc]   &'static str, 2 modifiers   {two_n:>5} allocs {two_b:>8} B"
    );
    assert!(bare_n > 0, "the bare tree has to have been built");
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

    /// The tree `where_a_frames_allocations_come_from` builds, on the
    /// arena: a column of rows, each a label, a spacer and a detail,
    /// padded and framed.  The detail is formatted at build time from
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
/// The same 40 rows as `where_a_frames_allocations_come_from`, whose
/// three phases on the previous tree are 201, 402 and 166 allocations.
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
