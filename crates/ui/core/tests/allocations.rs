//! Building a frame allocates nothing.
//!
//! This is the line the whole design is bent around: the slab and the
//! layer array come from the caller, instances are written straight
//! into them, and nothing in between reaches for the heap.  Asserting
//! it is cheap; the expensive part is a meter that can fail, so the
//! zero this reports means something.

use std::alloc::{GlobalAlloc, Layout as AllocLayout, System};
use std::cell::Cell;

use golia_ui_core::scene::{GlyphInstance, Layer, RectInstance, Scene};
use golia_ui_core::{RectPx, Rgba8};

// Per-thread, because the test harness runs tests concurrently in one
// process and a global counter would tally other tests' allocations as
// this frame's.  That is not a hypothetical: it read 4 on a frame that
// allocates nothing, and the four belonged to a sibling test.
//
// `const` init matters — a lazily initialised thread-local would
// allocate on first touch, from inside the allocator.
thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: AllocLayout) -> *mut u8 {
        note();
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocLayout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocLayout, n: usize) -> *mut u8 {
        note();
        unsafe { System.realloc(p, l, n) }
    }
}

fn note() {
    // `try_with` because a thread tearing down has already dropped its
    // locals, and panicking inside the allocator would be worse than
    // missing a count nobody is reading.
    let _ = COUNTING.try_with(|c| {
        if c.get() {
            let _ = ALLOCS.try_with(|a| a.set(a.get() + 1));
        }
    });
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn measured<T>(f: impl FnOnce() -> T) -> (usize, T) {
    ALLOCS.with(|a| a.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (ALLOCS.with(|a| a.get()), out)
}

#[test]
fn the_meter_can_fail() {
    let (n, _) = measured(|| Box::new([0u8; 128]));
    assert!(n >= 1, "a counter blind to one Box would call every frame free");
    let (n, _) = measured(|| std::hint::black_box(2 + 2));
    assert_eq!(n, 0, "and it must not invent allocations");
}

#[test]
fn the_meter_ignores_other_threads() {
    // The reason it is per-thread.  A sibling allocating hard must not
    // show up in this thread's count.
    let busy = std::thread::spawn(|| {
        for _ in 0..10_000 {
            std::hint::black_box(Box::new([0u8; 64]));
        }
    });
    let (n, _) = measured(|| {
        std::thread::yield_now();
        std::hint::black_box(2 + 2)
    });
    busy.join().expect("sibling");
    assert_eq!(n, 0, "another thread's allocations were counted as ours");
}

#[test]
fn one_pane_of_cells_and_glyphs_allocates_nothing() {
    // An 80×24 grid: every cell a background rectangle and a glyph,
    // which is the worst case for a full screen of text.
    const CELLS: usize = 80 * 24;
    let mut slab = vec![0u8; 256 * 1024];
    let mut layers = vec![Layer::default(); 32];

    // Warm: the first pass touches pages, and page faults are not
    // allocations but are noise worth keeping out of the number.
    {
        let mut scene = Scene::new(&mut slab, &mut layers);
        let mut l = scene.layer(RectPx::new(0.0, 0.0, 800.0, 600.0), 1).unwrap();
        l.rects().push(RectInstance::default());
    }

    let (allocs, (rects, glyphs, overflowed)) = measured(|| {
        let mut scene = Scene::new(&mut slab, &mut layers);
        let mut l = scene
            .layer(RectPx::new(0.0, 0.0, 800.0, 600.0), 0xC0FFEE)
            .expect("a layer");
        let mut r = l.rects();
        for i in 0..CELLS {
            r.push(RectInstance {
                origin: [(i % 80) as f32 * 10.0, (i / 80) as f32 * 25.0],
                size: [10.0, 25.0],
                color: Rgba8::hex(0x101010),
            });
        }
        let rects = r.len();
        drop(r);
        let mut g = l.glyphs();
        for i in 0..CELLS {
            g.push(GlyphInstance {
                origin: [(i % 80) as f32 * 10.0, (i / 80) as f32 * 25.0],
                size: [10.0, 25.0],
                uv0: [0.0, 0.0],
                uv1: [0.1, 0.1],
                color: Rgba8::WHITE,
            });
        }
        let glyphs = g.len();
        drop(g);
        drop(l);
        (rects, glyphs, scene.overflowed())
    });

    assert_eq!(rects as usize, CELLS);
    assert_eq!(glyphs as usize, CELLS);
    assert!(!overflowed, "256 KiB should hold one pane comfortably");
    assert_eq!(allocs, 0, "building a frame reached for the heap {allocs} times");
}

