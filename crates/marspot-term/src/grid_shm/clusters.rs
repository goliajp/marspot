//! The text of a frame's grapheme clusters, published beside the frame.
//!
//! A cluster cell -- a flag, a ZWJ family, a letter with a combining
//! mark -- holds an index into the writer's pool, and the pool is not in
//! the grid region. Without its text a reader can only draw the index.
//! This region carries, for each cluster cell in view, its position and
//! its text.
//!
//! It is a region of its own, named after the session and created by
//! L3, rather than space added to the grid region: that region is sized
//! by L2 when the pane opens and kept for the pane's life, so growing it
//! would reach only panes opened after the upgrade.
//!
//! ## Layout
//!
//! ```text
//! magic u32 | version u32 | count u32 | bytes u32 | overflow u32
//! count × (col u16 | row u16 | len u16 | len bytes of UTF-8)
//! ```
//!
//! ## Consistency
//!
//! No lock of its own. The writer fills it while the grid region's
//! `seq` is odd, and a reader copies it between its two reads of that
//! `seq`, so a table is only ever taken together with the cells it was
//! written for. A torn copy is discarded with the cells.
//!
//! A reader that finds no region, or one it does not recognise, reads
//! no clusters -- which is what every reader saw before this existed.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const MAGIC: u32 = 0x4d_53_43_31; // "MSC1"
const VERSION: u32 = 1;
const HEADER: usize = 20;
const ENTRY_HEAD: usize = 6;

/// Bytes in the region. A 200 × 80 pane is 16 000 cells; measured over
/// real output, 55 cells in forty million are clusters.
pub const CAPACITY: usize = 64 * 1024;

/// The region's name for session `id`. Under macOS's ~31-byte limit.
pub fn region_name(id: u64) -> CString {
    CString::new(format!("/msp-c-{id}")).expect("no interior NUL")
}

/// Remove session `id`'s region. Mappings already open stay valid until
/// they are closed; nothing new can open it.
pub fn delete(id: u64) {
    let name = region_name(id);
    unsafe { libc::shm_unlink(name.as_ptr()) };
}

/// Writer side, owned by the session that publishes the grid.
pub struct ClusterTableWriter {
    _fd: OwnedFd,
    base: *mut u8,
    /// Where the next entry goes.
    at: usize,
    count: u32,
    overflow: bool,
    /// Entries in the last table written, so a frame with none after a
    /// frame with none writes nothing at all.
    pub(super) last_count: u32,
}

// The pointer is into a mapping this struct alone owns, written by one
// thread at a time.
unsafe impl Send for ClusterTableWriter {}

impl ClusterTableWriter {
    /// Create session `id`'s region, or take over the one a previous
    /// image of the same session left behind.
    pub fn open(id: u64) -> io::Result<Self> {
        let name = region_name(id);
        let fd = unsafe {
            libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let size = region_size(&fd)?;
        // macOS refuses to resize a shared memory object once it has a
        // size, so a region from a previous image is taken as it is
        if size == 0 {
            if unsafe { libc::ftruncate(fd.as_raw_fd(), CAPACITY as libc::off_t) } != 0 {
                return Err(io::Error::last_os_error());
            }
        } else if size != CAPACITY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "cluster region of another size"));
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
            put_u32(base, 0, MAGIC);
            put_u32(base, 4, VERSION);
            put_u32(base, 8, 0);
            put_u32(base, 12, 0);
            put_u32(base, 16, 0);
        }
        Ok(Self { _fd: fd, base, at: HEADER, count: 0, overflow: false, last_count: 0 })
    }

    pub(super) fn begin(&mut self) {
        self.at = HEADER;
        self.count = 0;
        self.overflow = false;
    }

    /// Add one cluster. Past the region's end the table is marked as
    /// overflowed and the rest are dropped; a reader treats them as it
    /// would with no table.
    pub(super) fn push(&mut self, col: u16, row: u16, text: &str) {
        let len = text.len();
        if self.overflow || len > u16::MAX as usize || self.at + ENTRY_HEAD + len > CAPACITY {
            self.overflow = true;
            return;
        }
        unsafe {
            put_u16(self.base, self.at, col);
            put_u16(self.base, self.at + 2, row);
            put_u16(self.base, self.at + 4, len as u16);
            std::ptr::copy_nonoverlapping(text.as_ptr(), self.base.add(self.at + ENTRY_HEAD), len);
        }
        self.at += ENTRY_HEAD + len;
        self.count += 1;
    }

    pub(super) fn finish(&mut self) {
        unsafe {
            put_u32(self.base, 8, self.count);
            put_u32(self.base, 12, (self.at - HEADER) as u32);
            put_u32(self.base, 16, self.overflow as u32);
        }
        self.last_count = self.count;
    }
}

impl Drop for ClusterTableWriter {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, CAPACITY) };
    }
}

/// Reader side: a read-only mapping of a session's region.
pub struct ClusterTableReader {
    base: *const u8,
}

unsafe impl Send for ClusterTableReader {}

impl ClusterTableReader {
    /// Map session `id`'s region read-only. An error when there is none
    /// -- an L3 from before this existed -- or it is not one this build
    /// can read.
    pub fn open(id: u64) -> io::Result<Self> {
        let name = region_name(id);
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_RDONLY, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        if region_size(&fd)? != CAPACITY {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "cluster region of another size"));
        }
        let base = unsafe {
            libc::mmap(std::ptr::null_mut(), CAPACITY, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let base = base as *const u8;
        let (magic, version) = unsafe { (get_u32(base, 0), get_u32(base, 4)) };
        if magic != MAGIC || version != VERSION {
            unsafe { libc::munmap(base as *mut libc::c_void, CAPACITY) };
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a cluster region this build reads"));
        }
        Ok(Self { base })
    }

    /// Copy the table into `out`. Call between the two reads of the
    /// grid region's `seq`; a torn copy is caught there. Entries that
    /// run past the region or are not UTF-8 end the copy, since only a
    /// torn read produces them.
    pub fn read_into(&self, out: &mut ClusterTable) {
        out.clear();
        let (count, overflow) = unsafe { (get_u32(self.base, 8), get_u32(self.base, 16)) };
        out.overflow = overflow != 0;
        let mut at = HEADER;
        for _ in 0..count {
            if at + ENTRY_HEAD > CAPACITY {
                break;
            }
            let (col, row, len) = unsafe {
                (get_u16(self.base, at), get_u16(self.base, at + 2), get_u16(self.base, at + 4))
            };
            let start = at + ENTRY_HEAD;
            if start + len as usize > CAPACITY {
                break;
            }
            let text = unsafe { std::slice::from_raw_parts(self.base.add(start), len as usize) };
            if std::str::from_utf8(text).is_err() {
                break;
            }
            let from = out.bytes.len() as u32;
            out.bytes.extend_from_slice(text);
            out.entries.push((col, row, from, len));
            at = start + len as usize;
        }
    }
}

impl Drop for ClusterTableReader {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, CAPACITY) };
    }
}

/// One frame's clusters, as copied out. Reused across frames: once its
/// buffers have grown to fit, a read allocates nothing.
#[derive(Default, Debug, Clone)]
pub struct ClusterTable {
    bytes: Vec<u8>,
    entries: Vec<(u16, u16, u32, u16)>,
    /// The writer ran out of room; clusters past the last entry were
    /// not written.
    pub overflow: bool,
}

impl ClusterTable {
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.entries.clear();
        self.overflow = false;
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `(col, row, text)` for each cluster in the frame.
    pub fn iter(&self) -> impl Iterator<Item = (u16, u16, &str)> {
        self.entries.iter().map(|&(col, row, from, len)| {
            let b = &self.bytes[from as usize..from as usize + len as usize];
            // validated on the way in
            (col, row, std::str::from_utf8(b).unwrap_or(""))
        })
    }
}

fn region_size(fd: &OwnedFd) -> io::Result<usize> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.st_size as usize)
}

unsafe fn put_u32(base: *mut u8, at: usize, v: u32) {
    unsafe { std::ptr::write_unaligned(base.add(at) as *mut u32, v) }
}

unsafe fn put_u16(base: *mut u8, at: usize, v: u16) {
    unsafe { std::ptr::write_unaligned(base.add(at) as *mut u16, v) }
}

unsafe fn get_u32(base: *const u8, at: usize) -> u32 {
    u32::from_ne_bytes(unsafe { std::ptr::read_volatile(base.add(at) as *const [u8; 4]) })
}

unsafe fn get_u16(base: *const u8, at: usize) -> u16 {
    u16::from_ne_bytes(unsafe { std::ptr::read_volatile(base.add(at) as *const [u8; 2]) })
}

#[cfg(test)]
mod tests {
    use super::super::{GridShmReader, GridShmWriter, create_region};
    use super::*;
    use crate::terminal::Terminal;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicU64, Ordering};

    const FLAG: &str = "\u{1f1ef}\u{1f1f5}";
    const FAMILY: &str = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    const ACCENT: &str = "e\u{301}";

    /// A session id no other test or live pane uses.
    fn test_id() -> u64 {
        static N: AtomicU64 = AtomicU64::new(0);
        0xC1_0000_0000 + (std::process::id() as u64) * 1000 + N.fetch_add(1, Ordering::Relaxed)
    }

    struct Pair {
        id: u64,
        w: GridShmWriter,
        r: GridShmReader,
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            delete(self.id);
        }
    }

    fn pair(cols: u16, rows: u16) -> Pair {
        let id = test_id();
        let fd = create_region(cols, rows).unwrap();
        let mut r = GridShmReader::from_fd(fd.as_raw_fd()).unwrap();
        let mut w = GridShmWriter::from_fd(fd).unwrap();
        w.attach_clusters(id);
        r.attach_clusters(ClusterTableReader::open(id).unwrap());
        Pair { id, w, r }
    }

    fn read(p: &Pair) -> Vec<(u16, u16, String)> {
        let (mut cells, mut wrapped, mut t) = (Vec::new(), Vec::new(), ClusterTable::default());
        p.r.read_with_clusters(&mut cells, &mut wrapped, &mut t).unwrap();
        t.iter().map(|(c, r, s)| (c, r, s.to_string())).collect()
    }

    #[test]
    fn the_text_of_each_cluster_in_view_reaches_the_reader() {
        let mut p = pair(20, 3);
        let mut term = Terminal::new(20, 3);
        term.feed(format!("{FLAG} {FAMILY}\r\nx{ACCENT}").as_bytes());
        p.w.publish(term.grid(), 0, 0);
        let got = read(&p);
        assert_eq!(
            got,
            vec![(0, 0, FLAG.to_string()), (3, 0, FAMILY.to_string()), (1, 1, ACCENT.to_string())]
        );
    }

    #[test]
    fn a_frame_without_clusters_after_one_with_them_clears_the_table() {
        let mut p = pair(20, 3);
        let mut term = Terminal::new(20, 3);
        term.feed(FLAG.as_bytes());
        p.w.publish(term.grid(), 0, 0);
        assert_eq!(read(&p).len(), 1);
        term.feed(b"\x1b[2J\x1b[H");
        p.w.publish(term.grid(), 0, 0);
        assert!(read(&p).is_empty());
    }

    /// A cluster grows in place as codepoints join it: same index, new
    /// text. The dedupe hash has to see that, or the table goes stale.
    #[test]
    fn a_cluster_extended_in_place_is_published_again() {
        let mut p = pair(20, 2);
        let mut term = Terminal::new(20, 2);
        term.feed("e\u{301}".as_bytes());
        assert!(p.w.publish_if_changed(term.grid(), 0, 0));
        term.feed("\u{302}".as_bytes());
        assert!(p.w.publish_if_changed(term.grid(), 0, 0), "the text changed");
        assert_eq!(read(&p)[0].2, "e\u{301}\u{302}");
    }

    #[test]
    fn clusters_in_scrolled_back_history_are_published() {
        let mut p = pair(20, 2);
        let mut term = Terminal::new(20, 2);
        term.feed(format!("{FLAG}\r\nb\r\nc\r\nd").as_bytes());
        // the flag's row has scrolled off; look back far enough to see it
        let off = term.grid().scrollback_len() as u16;
        p.w.publish(term.grid(), off, 0);
        assert!(read(&p).iter().any(|(_, _, t)| t == FLAG), "{:?}", read(&p));
    }

    #[test]
    fn a_table_past_the_region_is_marked_overflowed() {
        let id = test_id();
        let mut w = ClusterTableWriter::open(id).unwrap();
        let r = ClusterTableReader::open(id).unwrap();
        let long = "e\u{301}".repeat(1000);
        w.begin();
        for i in 0..100 {
            w.push(i, 0, &long);
        }
        w.finish();
        let mut t = ClusterTable::default();
        r.read_into(&mut t);
        delete(id);
        assert!(t.overflow);
        assert!(!t.is_empty());
        assert!(t.iter().all(|(_, _, s)| s == long));
    }

    #[test]
    fn a_session_without_a_region_has_no_reader() {
        assert!(ClusterTableReader::open(test_id()).is_err());
    }

    #[test]
    fn a_new_image_of_the_same_session_takes_the_region_over() {
        let id = test_id();
        let first = ClusterTableWriter::open(id).unwrap();
        drop(first);
        let again = ClusterTableWriter::open(id);
        delete(id);
        assert!(again.is_ok(), "{:?}", again.err());
    }
}
