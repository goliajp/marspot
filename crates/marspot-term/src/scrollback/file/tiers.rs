//! Where a line is: which file, and where in it.
//!
//! A segment is read through a read-only mapping of the whole file,
//! made the first time it is read: its size never changes, so the
//! mapping never has to.  Files of the format before segments are read
//! with `pread`, as the cold tier always was.

use std::cell::Cell;

use super::*;

/// A segment in use, and the lines of the history it holds.
pub(super) struct SegTier {
    pub(super) seg: Seg,
    pub(super) first_line: u64,
    /// Lines it holds.  For the hot segment this is the count when it
    /// stopped being written, or zero: see `FileScrollback::seg_lines`.
    pub(super) lines: u64,
    /// Where its records and notes end, likewise.
    pub(super) used_end: u64,
    pub(super) map: Mapping,
}

/// A read-only mapping of a whole file, made on first use.
///
/// Shared, not private: on macOS a private mapping of a page not yet
/// written -- the index entries and records past the last line, which
/// a segment's next lines are written into -- keeps the zeroes it was
/// first read as, and lines written there afterwards never show in it.
/// A shared mapping is the page cache itself.  Nothing writes through
/// it, so it costs no more memory than a private one.
pub(super) struct Mapping {
    ptr: Cell<*mut u8>,
    len: Cell<usize>,
}

// The mapping is only read, by the thread that owns the scrollback, and
// is dropped -- unmapped -- wherever the tier ends up.
unsafe impl Send for Mapping {}

impl Mapping {
    pub(super) fn none() -> Self {
        Self { ptr: Cell::new(std::ptr::null_mut()), len: Cell::new(0) }
    }

    /// `len` bytes at `at` of `file`, which is `size` bytes long.
    pub(super) fn bytes(&self, file: &std::fs::File, size: u64, at: u64, len: usize) -> Option<&[u8]> {
        if self.ptr.get().is_null() {
            use std::os::fd::AsRawFd;
            // SAFETY: mapping a whole file read-only.
            let p = unsafe {
                libc::mmap(std::ptr::null_mut(), size as libc::size_t, libc::PROT_READ, libc::MAP_SHARED, file.as_raw_fd(), 0)
            };
            if p == libc::MAP_FAILED {
                return None;
            }
            self.ptr.set(p.cast());
            self.len.set(size as usize);
        }
        let end = (at as usize).checked_add(len)?;
        if end > self.len.get() {
            return None;
        }
        // SAFETY: inside the mapping, which lives as long as `self`.
        Some(unsafe { std::slice::from_raw_parts(self.ptr.get().add(at as usize), len) })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if !self.ptr.get().is_null() {
            // SAFETY: a mapping this value made and still holds.
            unsafe { libc::munmap(self.ptr.get().cast(), self.len.get()) };
        }
    }
}

impl SegTier {
    pub(super) fn new(seg: Seg, first_line: u64, lines: u64, used_end: u64) -> Self {
        Self { seg, first_line, lines, used_end, map: Mapping::none() }
    }

    pub(super) fn bytes(&self, at: u64, len: usize) -> Option<&[u8]> {
        self.map.bytes(&self.seg.file, self.seg.hdr.file_bytes(), at, len)
    }

    fn u32_at(&self, at: u64) -> Option<u32> {
        self.bytes(at, 4).map(|b| u32::from_le_bytes(b.try_into().expect("four bytes")))
    }

    /// Where the record of local line `i` starts, if this use wrote its
    /// index entry.
    pub(super) fn entry(&self, i: u64) -> Option<u64> {
        let b = self.bytes(self.seg.hdr.index_at() + i * 8, 8)?;
        let e = u64::from_le_bytes(b.try_into().expect("eight bytes"));
        ((e >> 32) as u32 == self.seg.hdr.serial as u32).then_some(e & 0xffff_ffff)
    }

    /// The body of the record at `off` -- flags, columns, cells -- when
    /// it lies inside the record area.
    pub(super) fn body(&self, off: u64) -> Option<&[u8]> {
        let len = self.u32_at(off)? as usize;
        if len < 3 || off + 4 + len as u64 > self.seg.hdr.index_at() {
            return None;
        }
        self.bytes(off + 4, len)
    }

    /// Where the record at `off` ends.
    pub(super) fn record_end(&self, off: u64) -> Option<u64> {
        self.body(off).map(|b| off + 4 + b.len() as u64)
    }
}

/// Where a logical line is.
#[derive(Clone, Copy, Debug)]
pub(super) enum Loc {
    Legacy(usize, u64),
    Seg(usize, u64),
}

impl FileScrollback {
    /// Lines segment `s` holds.
    pub(super) fn seg_lines(&self, s: usize) -> u64 {
        if s + 1 == self.segs.len() { self.hot_lines } else { self.segs[s].lines }
    }

    /// Where segment `s`'s records and notes end.
    pub(super) fn seg_end(&self, s: usize) -> u64 {
        if s + 1 == self.segs.len() { self.hot_pos } else { self.segs[s].used_end }
    }

    /// Which file holds logical line `line`, or `None` for a line the
    /// budget has let go or that is not there yet.
    pub(super) fn locate(&self, line: u64) -> Option<Loc> {
        if line >= self.total_lines {
            return None;
        }
        for s in (0..self.segs.len()).rev() {
            let t = &self.segs[s];
            if line >= t.first_line {
                let local = line - t.first_line;
                return (local < self.seg_lines(s)).then_some(Loc::Seg(s, local));
            }
        }
        for (k, l) in self.legacy.iter().enumerate().rev() {
            if line >= l.first_line {
                let local = line - l.first_line;
                return (local < l.lines).then_some(Loc::Legacy(k, local));
            }
        }
        None
    }

    /// The record body of a line that is not in the RAM ring.
    pub(super) fn with_body<R>(&self, line: u64, read: impl FnOnce(&[u8]) -> R) -> Option<R> {
        match self.locate(line)? {
            Loc::Seg(s, local) => {
                // the hot segment, or one changed from moments ago, may
                // still be in a buffer
                self.ensure_flushed();
                let t = &self.segs[s];
                Some(read(t.body(t.entry(local)?)?))
            }
            Loc::Legacy(k, local) => {
                let l = &self.legacy[k];
                let off = read_idx_at(&l.idx, local).ok()?;
                Some(read(&read_body_at(&l.bin, off).ok()?))
            }
        }
    }
}
