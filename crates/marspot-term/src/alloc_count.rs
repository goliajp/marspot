//! An allocator that counts, for tests that hold a path to zero.
//!
//! A test binary installs it with
//! `#[global_allocator] static A: Counting = Counting;` and measures a
//! region with [`counted`].  Counts are per thread, so tests running at
//! once in one process do not see each other's work.  The first
//! allocation in a measured region is remembered with its backtrace,
//! which is what a failing zero-allocation test should print.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static FIRST: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub struct Counting;

fn note(size: usize) {
    let counting = COUNTING.try_with(|c| c.replace(false)).unwrap_or(false);
    if !counting {
        return;
    }
    let n = ALLOCS.try_with(|a| {
        a.set(a.get() + 1);
        a.get()
    });
    if n == Ok(1) {
        // the counter is off while the trace itself allocates
        let trace = format!(
            "{size} B at\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        let _ = FIRST.try_with(|f| *f.borrow_mut() = Some(trace));
    }
    let _ = COUNTING.try_with(|c| c.set(true));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// What a measured region allocated.
pub struct Tally {
    /// Allocations and reallocations made on this thread.
    pub allocs: usize,
    /// The first of them, with where it came from.
    pub first: Option<String>,
}

/// Run `f` with this thread's count on.  Only meaningful in a binary
/// whose global allocator is [`Counting`]: elsewhere it reads zero.
pub fn counted<T>(f: impl FnOnce() -> T) -> (Tally, T) {
    ALLOCS.with(|a| a.set(0));
    FIRST.with(|f| f.borrow_mut().take());
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    let tally = Tally {
        allocs: ALLOCS.with(|a| a.get()),
        first: FIRST.with(|f| f.borrow_mut().take()),
    };
    (tally, out)
}
