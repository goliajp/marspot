//! The text of a frame's grapheme clusters, published beside the frame.
//!
//! A cluster cell -- a flag, a ZWJ family, a letter with a combining
//! mark -- holds an index into the writer's pool, and the pool is not in
//! the grid region. Without its text a reader can only draw the index.
//! This region carries, for each cluster cell in view, its position and
//! its text.
//!
//! It is a region of its own, named after the grid region and created
//! by L3, rather than space added to the grid region: that region is sized
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
//! The region ends with whose it is -- see [`super::ident`].
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

use std::ffi::CStr;
use std::io;
use std::os::fd::OwnedFd;

use super::ident::{self, Identity, SessionRegions};
use super::names::Side;

const MAGIC: u32 = 0x4d_53_43_31; // "MSC1"
const VERSION: u32 = 1;
const HEADER: usize = 20;
const ENTRY_HEAD: usize = 6;

/// Bytes in the region. A 200 × 80 pane is 16 000 cells; measured over
/// real output, 55 cells in forty million are clusters.
pub const CAPACITY: usize = 64 * 1024;

/// Where the table ends and the region's identity begins.
const TABLE_END: usize = CAPACITY - ident::BYTES;

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
    /// Create `owner`'s region under `name`, or take over the one a
    /// previous image of the same session left there.  A region of
    /// another session under that name loses the name: its mappings
    /// stay with whoever holds them.
    pub fn open(name: &CStr, owner: Identity) -> io::Result<Self> {
        let (fd, base) = super::ident::open_owned(name, CAPACITY, owner)?;
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
        if self.overflow || len > u16::MAX as usize || self.at + ENTRY_HEAD + len > TABLE_END {
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
    /// Map the cluster region of the session `regions` names,
    /// read-only. An error when there is none -- an L3 from before
    /// this existed -- or it is not one this build can read, or it is
    /// another session's.
    pub fn open(regions: &SessionRegions) -> io::Result<Self> {
        let name = regions
            .side_name(Side::Clusters)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no cluster region for this grid region"))?;
        let base = ident::map_readonly(&name, CAPACITY)?;
        let (magic, version) = unsafe { (get_u32(base, 0), get_u32(base, 4)) };
        let ok = magic == MAGIC && version == VERSION;
        let admitted = regions.admit(unsafe { ident::read(base.add(TABLE_END)) });
        if !ok || admitted.is_err() {
            unsafe { libc::munmap(base as *mut libc::c_void, CAPACITY) };
            admitted?;
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

    /// A session no other test or live pane uses.
    fn test_regions() -> SessionRegions {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = 0xC1_0000_0000 + (std::process::id() as u64) * 1000 + N.fetch_add(1, Ordering::Relaxed);
        SessionRegions::new(super::super::session_shm_name(id), id, true)
    }

    struct Pair {
        regions: SessionRegions,
        w: GridShmWriter,
        r: GridShmReader,
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            self.regions.delete();
        }
    }

    fn pair(cols: u16, rows: u16) -> Pair {
        let regions = test_regions();
        let fd = create_region(cols, rows).unwrap();
        let mut r = GridShmReader::from_fd(fd.as_raw_fd()).unwrap();
        let mut w = GridShmWriter::from_fd(fd).unwrap();
        w.attach_side_regions(regions.grid_name(), regions.owner());
        r.attach_clusters(ClusterTableReader::open(&regions).unwrap());
        Pair { regions, w, r }
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
        let regions = test_regions();
        let name = regions.side_name(Side::Clusters).unwrap();
        let mut w = ClusterTableWriter::open(&name, regions.owner()).unwrap();
        let long = "e\u{301}".repeat(1000);
        w.begin();
        for i in 0..100 {
            w.push(i, 0, &long);
        }
        w.finish();
        // opened after the overflow: the table stopped short of the
        // region's identity
        let r = ClusterTableReader::open(&regions);
        regions.delete();
        let mut t = ClusterTable::default();
        r.expect("the region is still the session's").read_into(&mut t);
        assert!(t.overflow);
        assert!(!t.is_empty());
        assert!(t.iter().all(|(_, _, s)| s == long));
    }

    #[test]
    fn a_session_without_a_region_has_no_reader() {
        assert!(ClusterTableReader::open(&test_regions()).is_err());
    }

    #[test]
    fn a_new_image_of_the_same_session_takes_the_region_over() {
        let regions = test_regions();
        let name = regions.side_name(Side::Clusters).unwrap();
        let first = ClusterTableWriter::open(&name, regions.owner()).unwrap();
        drop(first);
        let again = ClusterTableWriter::open(&name, regions.owner());
        regions.delete();
        assert!(again.is_ok(), "{:?}", again.err());
    }

    /// The same name asked for as another session's: what a reader
    /// sees when a second app's session has put its region there.
    fn as_another_session(regions: &SessionRegions) -> SessionRegions {
        SessionRegions::new(regions.grid_name().to_owned(), regions.owner().session + 1, true)
    }

    fn one_cluster(w: &mut ClusterTableWriter, text: &str) {
        w.begin();
        w.push(0, 0, text);
        w.finish();
    }

    fn clusters_of(r: &ClusterTableReader) -> Vec<String> {
        let mut t = ClusterTable::default();
        r.read_into(&mut t);
        t.iter().map(|(_, _, s)| s.to_string()).collect()
    }

    #[test]
    fn a_reader_refuses_the_region_of_another_session() {
        let regions = test_regions();
        let name = regions.side_name(Side::Clusters).unwrap();
        let _w = ClusterTableWriter::open(&name, regions.owner()).unwrap();
        let other = ClusterTableReader::open(&as_another_session(&regions));
        let own = ClusterTableReader::open(&regions);
        regions.delete();
        assert!(other.is_err(), "another session's table was taken");
        assert!(own.is_ok(), "{:?}", own.err());
    }

    #[test]
    fn a_writer_does_not_write_into_the_region_of_another_session() {
        let first = test_regions();
        let second = as_another_session(&first);
        let name = first.side_name(Side::Clusters).unwrap();
        let mut w1 = ClusterTableWriter::open(&name, first.owner()).unwrap();
        let r1 = ClusterTableReader::open(&first).unwrap();
        one_cluster(&mut w1, FLAG);
        // the second session arrives under the same name
        let mut w2 = ClusterTableWriter::open(&name, second.owner()).unwrap();
        one_cluster(&mut w2, FAMILY);
        let r2 = ClusterTableReader::open(&second);
        let late = ClusterTableReader::open(&first);
        first.delete();
        assert_eq!(clusters_of(&r1), [FLAG], "the first session's table was written by the second");
        assert_eq!(clusters_of(&r2.expect("the second session's own region")), [FAMILY]);
        assert!(late.is_err(), "the name is the second session's now");
    }

    /// A region as an L3 from before identities leaves it.
    fn unstamped_region(name: &CStr) {
        let fd = unsafe { libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600) };
        assert!(fd >= 0, "{}", io::Error::last_os_error());
        assert_eq!(unsafe { libc::ftruncate(fd, CAPACITY as libc::off_t) }, 0);
        let base = unsafe {
            libc::mmap(std::ptr::null_mut(), CAPACITY, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
        } as *mut u8;
        unsafe {
            put_u32(base, 0, MAGIC);
            put_u32(base, 4, VERSION);
            libc::munmap(base as *mut libc::c_void, CAPACITY);
            libc::close(fd);
        }
    }

    #[test]
    fn a_region_without_an_identity_is_read_only_for_a_session_that_writes_none() {
        let regions = test_regions();
        let name = regions.side_name(Side::Clusters).unwrap();
        unstamped_region(&name);
        let old_session = SessionRegions::new(regions.grid_name().to_owned(), regions.owner().session, false);
        let from_old = ClusterTableReader::open(&old_session);
        let from_new = ClusterTableReader::open(&regions);
        // the session's next image takes the region over and signs it
        let taken = ClusterTableWriter::open(&name, regions.owner());
        let after = ClusterTableReader::open(&regions);
        regions.delete();
        assert!(from_old.is_ok(), "{:?}", from_old.err());
        assert!(from_new.is_err(), "an unsigned region passed for a signing session's");
        assert!(taken.is_ok(), "{:?}", taken.err());
        assert!(after.is_ok(), "{:?}", after.err());
    }
}
