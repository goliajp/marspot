//! The links of a frame's OSC 8 hyperlinks, in the cluster region
//! after its clusters -- the layout and why it is safe for a reader
//! from before links are in [`super::clusters`].

use super::clusters::{ClusterTable, ClusterTableWriter, HEADER, TABLE_END, get_u16, get_u32, put_u32};

const LINKS_MAGIC: u32 = 0x4d_53_4c_31; // "MSL1"
const LINKS_HEAD: usize = 16;
const LINK_HEAD: usize = 8;

impl ClusterTableWriter {
    /// Add one link: on `row`, columns `c0..=c1`.
    pub(super) fn push_link(&mut self, row: u16, c0: u16, c1: u16, uri: &str) {
        let Ok(len) = u16::try_from(uri.len()) else { return };
        for v in [row, c0, c1, len] {
            self.links.extend_from_slice(&v.to_ne_bytes());
        }
        self.links.extend_from_slice(uri.as_bytes());
    }

    /// The links after the clusters, as many whole ones as fit, stated
    /// as the links of the frame the grid region carries `seq` for.
    pub(super) fn write_links(&mut self, seq: u64) {
        let at = self.at;
        if at + LINKS_HEAD > TABLE_END {
            return;
        }
        let room = TABLE_END - at - LINKS_HEAD;
        let (mut used, mut n) = (0usize, 0u32);
        while used + LINK_HEAD <= self.links.len() {
            let len = u16::from_ne_bytes([self.links[used + 6], self.links[used + 7]]) as usize;
            if used + LINK_HEAD + len > room {
                break;
            }
            used += LINK_HEAD + len;
            n += 1;
        }
        unsafe {
            put_u32(self.base, at, LINKS_MAGIC);
            std::ptr::write_unaligned(self.base.add(at + 4) as *mut u64, seq);
            put_u32(self.base, at + 12, n);
            std::ptr::copy_nonoverlapping(self.links.as_ptr(), self.base.add(at + LINKS_HEAD), used);
        }
    }
}

impl ClusterTable {
    /// `(row, first col, last col, URI)` for each link in the frame.
    pub fn links(&self) -> impl Iterator<Item = (u16, u16, u16, &str)> {
        self.links.iter().map(|&(row, c0, c1, from, len)| {
            let b = &self.bytes[from as usize..from as usize + len as usize];
            // validated on the way in
            (row, c0, c1, std::str::from_utf8(b).unwrap_or(""))
        })
    }
}

/// # Safety
/// `base` maps a cluster region.
pub(super) unsafe fn read_links(base: *const u8, out: &mut ClusterTable, seq: u64) {
    let bytes = unsafe { get_u32(base, 12) } as usize;
    let at = HEADER + bytes;
    if at + LINKS_HEAD > TABLE_END {
        return;
    }
    let (magic, stated, n) = unsafe {
        (
            get_u32(base, at),
            u64::from(get_u32(base, at + 4)) | u64::from(get_u32(base, at + 8)) << 32,
            get_u32(base, at + 12),
        )
    };
    if magic != LINKS_MAGIC || stated != seq {
        return;
    }
    let mut p = at + LINKS_HEAD;
    for _ in 0..n {
        if p + LINK_HEAD > TABLE_END {
            break;
        }
        let (row, c0, c1, len) =
            unsafe { (get_u16(base, p), get_u16(base, p + 2), get_u16(base, p + 4), get_u16(base, p + 6)) };
        let start = p + LINK_HEAD;
        if start + len as usize > TABLE_END {
            break;
        }
        let uri = unsafe { std::slice::from_raw_parts(base.add(start), len as usize) };
        if std::str::from_utf8(uri).is_err() {
            break;
        }
        let from = out.bytes.len() as u32;
        out.bytes.extend_from_slice(uri);
        out.links.push((row, c0, c1, from, len));
        p = start + len as usize;
    }
}

#[cfg(test)]
mod tests {
    use super::super::cells::VERSION_PACKED;
    use super::super::{ClusterTable, ClusterTableReader, GridShmReader, GridShmWriter, SessionRegions, create_region};
    use crate::terminal::Terminal;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicU64, Ordering};

    const LINE: &[u8] = b"\x1b]8;;https://example.com\x1b\\Click me\x1b]8;;\x1b\\\n";

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

    fn pair(cols: u16, rows: u16, packed: bool) -> Pair {
        static N: AtomicU64 = AtomicU64::new(0);
        let id = 0xC8_0000_0000 + (std::process::id() as u64) * 1000 + N.fetch_add(1, Ordering::Relaxed);
        let regions = SessionRegions::new(super::super::session_shm_name(id), id, true);
        let fd = create_region(cols, rows).unwrap();
        let mut r = GridShmReader::from_fd(fd.as_raw_fd()).unwrap();
        let mut w = GridShmWriter::from_fd(fd).unwrap();
        w.attach_side_regions(regions.grid_name(), regions.owner());
        if packed {
            w.set_reader_frames(VERSION_PACKED);
        }
        r.attach_clusters(ClusterTableReader::open(&regions).unwrap());
        Pair { regions, w, r }
    }

    fn read(p: &Pair) -> (Vec<crate::grid::Cell>, Vec<(u16, u16, u16, String)>) {
        let (mut cells, mut wrapped, mut t) = (Vec::new(), Vec::new(), ClusterTable::default());
        p.r.read_with_clusters(&mut cells, &mut wrapped, &mut t).unwrap();
        (cells, t.links().map(|(r, a, b, u)| (r, a, b, u.to_string())).collect())
    }

    fn uri() -> String {
        "https://example.com".to_string()
    }

    #[test]
    fn osc8_the_links_on_screen_reach_the_reader_in_either_frame_layout() {
        for packed in [false, true] {
            let mut p = pair(20, 3, packed);
            let mut t = Terminal::new(20, 3);
            t.feed(LINE);
            p.w.publish(t.grid(), 0, 0);
            let (cells, links) = read(&p);
            assert_eq!(links, vec![(0, 0, 7, uri())], "packed={packed}");
            // no cell in a frame carries an id: it is a number in the
            // writer's pool, and a reader from before links refuses an
            // eight-byte cell with those bits set
            assert!(cells.iter().all(|c| c.attrs.link_id() == 0), "packed={packed}");
            let text: String = cells[..8].iter().map(|c| c.ch).collect();
            assert_eq!(text, "Click me", "packed={packed}");
        }
    }

    #[test]
    fn osc8_links_in_scrolled_back_history_are_published() {
        let mut p = pair(20, 2, false);
        let mut t = Terminal::new(20, 2);
        t.feed(LINE);
        t.feed(b"\r\nb\r\nc\r\nd");
        let off = t.grid().scrollback_len() as u16;
        p.w.publish(t.grid(), off, 0);
        let (_, links) = read(&p);
        assert_eq!(links, vec![(0, 0, 7, uri())]);
    }

    /// A frame that writes no table -- no clusters and no links, which
    /// is every frame of an image from before links -- leaves the last
    /// section in the region.  It belongs to another frame and must not
    /// be read as this one's; without the `seq` it states, it would be.
    #[test]
    fn osc8_a_section_an_earlier_frame_left_is_not_read_as_this_frames() {
        let mut p = pair(20, 3, false);
        let mut t = Terminal::new(20, 3);
        t.feed(LINE);
        p.w.publish(t.grid(), 0, 0);
        assert_eq!(read(&p).1.len(), 1);
        let plain = Terminal::new(20, 3);
        p.w.publish(plain.grid(), 0, 0);
        assert!(read(&p).1.is_empty());
    }

    /// The same text given another target is published again, though
    /// no character changed.
    #[test]
    fn osc8_a_new_target_on_the_same_text_is_published_again() {
        let mut p = pair(20, 3, false);
        let mut t = Terminal::new(20, 3);
        t.feed(LINE);
        assert!(p.w.publish_if_changed(t.grid(), 0, 0));
        t.feed(b"\x1b[H\x1b]8;;https://example.org\x1b\\Click me\x1b]8;;\x1b\\");
        assert!(p.w.publish_if_changed(t.grid(), 0, 0), "the target changed");
        assert_eq!(read(&p).1, vec![(0, 0, 7, "https://example.org".to_string())]);
    }
}

