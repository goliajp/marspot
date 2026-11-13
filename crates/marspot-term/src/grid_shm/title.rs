//! A pane's window title (OSC 0 / OSC 2), published beside the frame.
//!
//! Nothing draws it; the shell reads it to learn what a program says
//! about itself -- a Codex TUI can put the id of the thread it shows
//! there, which no other observable carries.
//!
//! A region of its own, named after the grid region and created by L3, for
//! the same reason as the cluster table: the grid region is sized by L2
//! when the pane opens. A new image of the same session takes the
//! region over without clearing it, so the title a program set before
//! an update is still there after it, until the program sets another.
//!
//! ## Layout
//!
//! ```text
//! magic u32 | version u32 | seq u32 | len u32 | len bytes of UTF-8
//! ```
//!
//! `seq` is odd while the writer is inside a write; a reader copies the
//! text between two equal, even reads of it.  The region ends with
//! whose it is -- see [`super::ident`].

use std::ffi::CStr;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU32, Ordering, fence};

use super::ident::{self, Identity, SessionRegions};
use super::names::Side;

const MAGIC: u32 = 0x4d_53_54_31; // "MST1"
const VERSION: u32 = 1;
const HEADER: usize = 16;

/// Bytes in the region: one page. The terminal keeps at most 256
/// characters of a title, at most 1 KiB of UTF-8.
pub const CAPACITY: usize = 16 * 1024;

/// Writer side, owned by the session that publishes the grid.
pub struct TitleWriter {
    _fd: OwnedFd,
    base: *mut u8,
    /// The terminal's title as last seen, not the region's: a new image
    /// starts with an empty terminal title and must not wipe the one
    /// its predecessor published.
    last: String,
}

// The pointer is into a mapping this struct alone owns, written by one
// thread at a time.
unsafe impl Send for TitleWriter {}

impl TitleWriter {
    /// Create `owner`'s region under `name`, or take over the one a
    /// previous image of the same session left there.
    pub fn open(name: &CStr, owner: Identity) -> io::Result<Self> {
        let (fd, base) = ident::open_owned(name, CAPACITY, owner)?;
        unsafe {
            std::ptr::write_unaligned(base as *mut u32, MAGIC);
            std::ptr::write_unaligned(base.add(4) as *mut u32, VERSION);
        }
        Ok(Self { _fd: fd, base, last: String::new() })
    }

    /// Publish `title` if it differs from the last one seen.
    pub fn update(&mut self, title: &str) {
        if title == self.last {
            return;
        }
        self.last.clear();
        self.last.push_str(title);
        let bytes = &title.as_bytes()[..title.len().min(CAPACITY - HEADER)];
        let seq = self.seq();
        let s = seq.load(Ordering::Relaxed);
        seq.store(s.wrapping_add(1) | 1, Ordering::Relaxed);
        fence(Ordering::Release);
        unsafe {
            std::ptr::write_unaligned(self.base.add(12) as *mut u32, bytes.len() as u32);
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(HEADER), bytes.len());
        }
        fence(Ordering::Release);
        seq.store((s | 1).wrapping_add(1), Ordering::Release);
    }

    fn seq(&self) -> &AtomicU32 {
        // offset 8 of a page-aligned mapping: aligned for u32
        unsafe { &*(self.base.add(8) as *const AtomicU32) }
    }
}

impl Drop for TitleWriter {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, CAPACITY) };
    }
}

/// The title of the session `regions` names. `Ok(None)` when it has
/// no region -- an L3 from before titles were published -- or one this
/// build does not read, or the region there is another session's.
pub fn read(regions: &SessionRegions) -> io::Result<Option<String>> {
    let Some(name) = regions.side_name(Side::Title) else {
        return Ok(None);
    };
    let base = match ident::map_readonly(&name, CAPACITY) {
        Ok(base) => base,
        Err(e) if matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::InvalidData) => {
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    let ours = regions.admit(unsafe { ident::read(base.add(CAPACITY - ident::BYTES)) }).is_ok();
    let out = if ours { unsafe { read_mapped(base) } } else { None };
    unsafe { libc::munmap(base as *mut libc::c_void, CAPACITY) };
    Ok(out)
}

unsafe fn read_mapped(base: *const u8) -> Option<String> {
    let (magic, version) = unsafe {
        (std::ptr::read_volatile(base as *const u32), std::ptr::read_volatile(base.add(4) as *const u32))
    };
    if magic != MAGIC || version != VERSION {
        return None;
    }
    let seq = unsafe { &*(base.add(8) as *const AtomicU32) };
    let mut buf = Vec::new();
    // a write is a memcpy of at most 1 KiB; a reader racing several in
    // a row is a program rewriting its title in a loop
    for _ in 0..64 {
        let before = seq.load(Ordering::Acquire);
        if before & 1 == 1 {
            std::hint::spin_loop();
            continue;
        }
        let len = (unsafe { std::ptr::read_volatile(base.add(12) as *const u32) } as usize).min(CAPACITY - HEADER);
        buf.clear();
        buf.extend_from_slice(unsafe { std::slice::from_raw_parts(base.add(HEADER), len) });
        fence(Ordering::Acquire);
        if seq.load(Ordering::Relaxed) == before {
            return String::from_utf8(buf).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session no other test or live pane uses, and the name of
    /// its title region.
    fn session() -> (SessionRegions, std::ffi::CString, Identity) {
        static N: AtomicU32 = AtomicU32::new(0);
        let id = (u64::from(std::process::id()) << 20) | u64::from(N.fetch_add(1, Ordering::Relaxed)) | (1 << 62);
        let regions = SessionRegions::new(super::super::session_shm_name(id), id, true);
        let (name, owner) = (regions.side_name(Side::Title).unwrap(), regions.owner());
        (regions, name, owner)
    }

    #[test]
    fn a_title_round_trips_and_a_missing_region_reads_as_none() {
        let (regions, name, owner) = session();
        assert_eq!(read(&regions).unwrap(), None);
        let mut w = TitleWriter::open(&name, owner).unwrap();
        assert_eq!(read(&regions).unwrap().as_deref(), Some(""));
        w.update("⠋ marspot | 01a161ee-50e9-7b50-8ad6-4bcae...");
        assert_eq!(read(&regions).unwrap().as_deref(), Some("⠋ marspot | 01a161ee-50e9-7b50-8ad6-4bcae..."));
        w.update("marspot");
        assert_eq!(read(&regions).unwrap().as_deref(), Some("marspot"));
        regions.delete();
        assert_eq!(read(&regions).unwrap(), None);
    }

    /// A new image of a session starts with an empty terminal title; the
    /// program's title stays until the program sets another.
    #[test]
    fn a_new_writer_keeps_the_title_until_the_terminal_sets_one() {
        let (regions, name, owner) = session();
        let mut old = TitleWriter::open(&name, owner).unwrap();
        old.update("codex | 01a1");
        drop(old);
        let mut new = TitleWriter::open(&name, owner).unwrap();
        new.update("");
        assert_eq!(read(&regions).unwrap().as_deref(), Some("codex | 01a1"));
        new.update("codex | 01a2");
        assert_eq!(read(&regions).unwrap().as_deref(), Some("codex | 01a2"));
        regions.delete();
    }
}
