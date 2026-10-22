//! A pane's window title (OSC 0 / OSC 2), published beside the frame.
//!
//! Nothing draws it; the shell reads it to learn what a program says
//! about itself -- a Codex TUI can put the id of the thread it shows
//! there, which no other observable carries.
//!
//! A region of its own, named after the session and created by L3, for
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
//! text between two equal, even reads of it.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering, fence};

const MAGIC: u32 = 0x4d_53_54_31; // "MST1"
const VERSION: u32 = 1;
const HEADER: usize = 16;

/// Bytes in the region: one page. The terminal keeps at most 256
/// characters of a title, at most 1 KiB of UTF-8.
pub const CAPACITY: usize = 16 * 1024;

/// The region's name for session `id`. Under macOS's ~31-byte limit.
pub fn region_name(id: u64) -> CString {
    CString::new(format!("/msp-t-{id}")).expect("no interior NUL")
}

/// Remove session `id`'s region.
pub fn delete(id: u64) {
    let name = region_name(id);
    unsafe { libc::shm_unlink(name.as_ptr()) };
}

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
    /// Create session `id`'s region, or take over the one a previous
    /// image of the same session left behind.
    pub fn open(id: u64) -> io::Result<Self> {
        let name = region_name(id);
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = region_size(&fd)?;
        if size == 0 {
            if unsafe { libc::ftruncate(fd.as_raw_fd(), CAPACITY as libc::off_t) } != 0 {
                return Err(io::Error::last_os_error());
            }
        } else if size != CAPACITY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "title region of another size"));
        }
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                CAPACITY,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = base as *mut u8;
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

/// Session `id`'s title. `Ok(None)` when it has no region -- an L3 from
/// before titles were published -- or one this build does not read.
pub fn read(id: u64) -> io::Result<Option<String>> {
    let name = region_name(id);
    let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(e) };
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    if region_size(&fd)? != CAPACITY {
        return Ok(None);
    }
    let base = unsafe {
        libc::mmap(std::ptr::null_mut(), CAPACITY, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0)
    };
    if base == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    let base = base as *const u8;
    let out = unsafe { read_mapped(base) };
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

fn region_size(fd: &OwnedFd) -> io::Result<usize> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.st_size as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_id() -> u64 {
        // unique per test process and call, far from real session ids
        static N: AtomicU32 = AtomicU32::new(0);
        (u64::from(std::process::id()) << 20) | u64::from(N.fetch_add(1, Ordering::Relaxed)) | (1 << 62)
    }

    #[test]
    fn a_title_round_trips_and_a_missing_region_reads_as_none() {
        let id = session_id();
        assert_eq!(read(id).unwrap(), None);
        let mut w = TitleWriter::open(id).unwrap();
        assert_eq!(read(id).unwrap().as_deref(), Some(""));
        w.update("⠋ marspot | 01a161ee-50e9-7b50-8ad6-4bcae...");
        assert_eq!(read(id).unwrap().as_deref(), Some("⠋ marspot | 01a161ee-50e9-7b50-8ad6-4bcae..."));
        w.update("marspot");
        assert_eq!(read(id).unwrap().as_deref(), Some("marspot"));
        delete(id);
        assert_eq!(read(id).unwrap(), None);
    }

    /// A new image of a session starts with an empty terminal title; the
    /// program's title stays until the program sets another.
    #[test]
    fn a_new_writer_keeps_the_title_until_the_terminal_sets_one() {
        let id = session_id();
        let mut old = TitleWriter::open(id).unwrap();
        old.update("codex | 01a1");
        drop(old);
        let mut new = TitleWriter::open(id).unwrap();
        new.update("");
        assert_eq!(read(id).unwrap().as_deref(), Some("codex | 01a1"));
        new.update("codex | 01a2");
        assert_eq!(read(id).unwrap().as_deref(), Some("codex | 01a2"));
        delete(id);
    }
}
