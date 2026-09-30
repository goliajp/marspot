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
//! No budget is asserted yet.  A budget invented before the number is
//! known is a number about nothing; this establishes the number, and
//! the ceiling gets set from it.  What *is* asserted is that the meter
//! works: a control region with a known allocation in it must register,
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
            eprintln!("skip (no font stack): {e}");
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
