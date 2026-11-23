//! Giving the pages of a buffer that waits back to the system.
//!
//! A pane's reader and disk writers keep their buffers from one burst
//! to the next, so that a steady flood allocates nothing.  Once a flood
//! has filled them they stay resident for the rest of the pane's life:
//! about 768 KiB a pane that has printed a big file once, a quarter of
//! the session's footprint, while it sits silent.  Freeing them would
//! not help -- the allocator keeps the dirty pages -- so the buffers
//! stay allocated and their pages are handed back instead, once output
//! has stopped and only when a burst wrote far into them.  A keystroke's
//! echo never comes near that, so typing never pays for any of this.
//!
//! On macOS that is `MADV_FREE_REUSABLE`, which takes the pages out of
//! the footprint at once, and `MADV_FREE_REUSE` before they are written
//! again.  The second call is not optional: a page written while still
//! marked reusable is not counted, and the system may take it away
//! with what was written into it.  `MADV_FREE` and `MADV_DONTNEED` do
//! not lower the footprint at all.  On Linux `MADV_DONTNEED` drops the
//! pages and the next write finds zeroed ones; nothing is needed before
//! it.

use std::sync::atomic::{AtomicUsize, Ordering};

/// How far a burst must have written into a buffer for its pages to be
/// given back when output stops: half a 64 KiB buffer.  A key's echo is
/// a few bytes and a full repaint of a big pane a few tens of KiB; only
/// bulk output gets here.
pub const WORTH_GIVING_BACK: usize = 32 * 1024;

/// The start of a buffer that keeps its pages: a key's echo, a prompt
/// and the start of the next burst fit there and take nothing back.
/// Without it, the first keystrokes after a flood took back a whole
/// buffer each, and kept it.
pub const KEPT: usize = 16 * 1024;

/// The pages of one buffer that have been given back, if any.
///
/// The buffer must not be reallocated, grown past its capacity or
/// written beyond [`Self::room`] until [`Self::take_back`] has run.
#[derive(Debug, Default)]
pub struct Given {
    /// Offsets into the allocation: the pages from `lo` to `hi`, none
    /// while `hi` is 0.  Four bytes each keep a writer's hand-over
    /// small; a buffer of 4 GiB or more never gives its pages back.
    lo: u32,
    hi: u32,
}

/// Times pages were given back, in this process.
static TIMES: AtomicUsize = AtomicUsize::new(0);

/// How many times this process has given pages back.
pub fn times_given_back() -> usize {
    TIMES.load(Ordering::Relaxed)
}

/// Times pages given back were taken back, in this process.
static TAKEN: AtomicUsize = AtomicUsize::new(0);

/// How many times this process has taken pages back.
pub fn times_taken_back() -> usize {
    TAKEN.load(Ordering::Relaxed)
}

fn page() -> usize {
    static PAGE: AtomicUsize = AtomicUsize::new(0);
    match PAGE.load(Ordering::Relaxed) {
        0 => {
            // SAFETY: sysconf has no preconditions.
            let p = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
                .unwrap_or(4096)
                .max(4096);
            PAGE.store(p, Ordering::Relaxed);
            p
        }
        p => p,
    }
}

impl Given {
    /// Give back the whole pages of `buf`'s allocation from byte `keep`
    /// to byte `touched`.  `false` when there is no whole page there or
    /// some are given back already.
    ///
    /// What the buffer held there is gone: it reads as zeroes or as the
    /// old bytes, whichever the system leaves.
    pub fn give_back(&mut self, buf: &mut Vec<u8>, keep: usize, touched: usize) -> bool {
        if self.is_given() {
            return false;
        }
        let base = buf.as_mut_ptr() as usize;
        let p = page();
        let lo = (base + keep).next_multiple_of(p) - base;
        let hi = ((base + touched.min(buf.capacity())) & !(p - 1)).saturating_sub(base);
        let (Ok(lo32), Ok(hi32)) = (u32::try_from(lo), u32::try_from(hi)) else {
            return false;
        };
        if hi <= lo {
            return false;
        }
        advise(base + lo, hi - lo, GIVE);
        (self.lo, self.hi) = (lo32, hi32);
        TIMES.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Make what [`Self::give_back`] gave back writable again.
    pub fn take_back(&mut self, buf: &mut Vec<u8>) {
        if self.is_given() {
            let (lo, hi) = (self.lo as usize, self.hi as usize);
            (self.lo, self.hi) = (0, 0);
            debug_assert!(
                hi <= buf.capacity(),
                "the buffer shrank while its pages were given back"
            );
            TAKEN.fetch_add(1, Ordering::Relaxed);
            if let Some(take) = TAKE {
                advise(buf.as_mut_ptr() as usize + lo, hi - lo, take);
            }
        }
    }

    /// How much of the buffer may be written before [`Self::take_back`]:
    /// all of `cap` unless pages are given back.
    pub fn room(&self, cap: usize) -> usize {
        if self.is_given() {
            (self.lo as usize).min(cap)
        } else {
            cap
        }
    }

    /// Where the pages given back end; 0 when none are.
    pub fn end(&self) -> usize {
        self.hi as usize
    }

    pub fn is_given(&self) -> bool {
        self.hi != 0
    }
}

#[cfg(target_os = "macos")]
const GIVE: libc::c_int = libc::MADV_FREE_REUSABLE;
#[cfg(target_os = "macos")]
const TAKE: Option<libc::c_int> = Some(libc::MADV_FREE_REUSE);
#[cfg(not(target_os = "macos"))]
const GIVE: libc::c_int = libc::MADV_DONTNEED;
#[cfg(not(target_os = "macos"))]
const TAKE: Option<libc::c_int> = None;

fn advise(at: usize, len: usize, advice: libc::c_int) {
    // SAFETY: whole pages inside an allocation the caller holds by
    // `&mut`; none of these advices unmap anything, and the bytes they
    // may zero are plain `u8`s.  A failure leaves the pages as they
    // were, which costs memory, never correctness.
    unsafe {
        libc::madvise(at as *mut libc::c_void, len, advice);
    }
}

#[cfg(test)]
mod tests;
