//! Frames of eight-byte cells, written here byte by byte from the
//! layout `cells.rs` describes, read by the reader every build after
//! that layout ships.

use std::sync::atomic::Ordering;

use super::cells::{CELL_SIZE_PACKED, CELL_SIZE_WIDE, FrameBuf, VERSION_PACKED};
use super::*;
use crate::grid::{Cell, CellAttrs, Color, Grid, PackedCell, PackedStyle};

fn dup_fd(fd: RawFd) -> RawFd {
    let d = unsafe { libc::dup(fd) };
    assert!(d >= 0, "dup failed");
    d
}

/// Write one frame of eight-byte cells into the writer's region the
/// way a session does: odd sequence, header, cells, the count and the
/// values, even sequence.  `count` overrides the count written.
fn write_packed(w: &GridShmWriter, cols: u16, rows: u16, cells: &[u64], styles: &[u64], count: Option<u32>) {
    unsafe {
        let h = w.header();
        let s = (*h).seq.load(Ordering::Relaxed);
        (*h).seq.store(s + 1, Ordering::Relaxed);
        (*h).version = VERSION_PACKED;
        (*h).cell_size = CELL_SIZE_PACKED;
        (*h).cols = cols as u32;
        (*h).rows = rows as u32;
        (*h).cursor_col = 1;
        (*h).cursor_row = 0;
        let base = w.base.add(HEADER_BYTES);
        for (i, c) in cells.iter().enumerate() {
            (base.add(i * 8) as *mut u64).write_unaligned(*c);
        }
        let at = base.add(cells.len() * 8);
        (at as *mut u32).write_unaligned(count.unwrap_or(styles.len() as u32));
        for (i, v) in styles.iter().enumerate() {
            (at.add(8 + i * 8) as *mut u64).write_unaligned(*v);
        }
        (*h).seq.store(s + 2, Ordering::Release);
    }
}

fn red_bold() -> CellAttrs {
    CellAttrs { fg: Color::indexed(1), bold: true, ..Default::default() }
}

/// A frame of eight-byte cells reads back as the cells it holds, as
/// they are and expanded, and loads into a mirror grid as them.
#[test]
fn a_frame_of_eight_byte_cells_reads_back() {
    let (cols, rows) = (4u16, 2u16);
    let writer = GridShmWriter::create(cols, rows).expect("region");
    let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
    let red = PackedStyle::pack(&red_bold());
    let blue = PackedStyle::pack(&CellAttrs { bg: Color::rgb(0, 0, 200), ..Default::default() });
    // the session's numbers: 0 default, 3 red, 1 blue, 2 unused
    let styles = [0, blue.0, 0, red.0];
    let cells: Vec<u64> = vec![
        PackedCell::new('a', 3).bits(),
        PackedCell::new('b', 0).bits(),
        PackedCell::new('漢', 1).bits(),
        PackedCell::new('\0', 1).bits(),
        PackedCell::BLANK.bits(),
        PackedCell::new('z', 3).bits(),
        PackedCell::BLANK.bits(),
        PackedCell::BLANK.bits(),
    ];
    write_packed(&writer, cols, rows, &cells, &styles, None);
    let mut buf = FrameBuf::default();
    let snap = reader.read_into(&mut buf, &mut Vec::new(), &mut ClusterTable::default()).expect("a frame");
    assert_eq!((snap.cols, snap.rows, snap.cursor_col), (4, 2, 1));
    assert!(buf.is_packed);
    let got = buf.expanded();
    assert_eq!(got[0], Cell { ch: 'a', attrs: red_bold() });
    assert_eq!(got[1], Cell::from('b'));
    assert_eq!(got[2].attrs, blue.unpack());
    assert_eq!((got[2].ch, got[3].ch), ('漢', '\0'));
    assert_eq!(got[5], Cell { ch: 'z', attrs: red_bold() });
    let mut mirror = Grid::new(cols, rows);
    let mut remap = Vec::new();
    mirror.load_frame(&buf.packed, &buf.styles, &mut remap);
    mirror.check_styles().unwrap();
    for (i, c) in got.iter().enumerate() {
        assert_eq!(mirror.cell(i as u16 % cols, i as u16 / cols), *c, "cell {i}");
    }
    // the plain reader and the peek read the same frame expanded
    let mut plain = Vec::new();
    reader.read(&mut plain, &mut Vec::new()).expect("a frame");
    assert_eq!(plain, got);
}

/// Bits that no build writes come back blank, and a table too big for
/// the region is not read at all; nothing a writer can put in the
/// region makes the reader read past it or hold an invalid cell.
#[test]
fn a_frame_that_is_not_one_reads_as_blanks_not_as_garbage() {
    let (cols, rows) = (3u16, 1u16);
    let writer = GridShmWriter::create(cols, rows).expect("region");
    let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
    let bad_style = 3u64; // a colour tag no build writes
    let cells = [
        0xD800,                                   // a surrogate is not a character
        PackedCell::new('x', 5).bits(),           // a number past the table
        PackedCell::new('y', 1).bits() | 1 << 25, // a reserved bit
    ];
    write_packed(&writer, cols, rows, &cells, &[0, bad_style], None);
    let mut buf = FrameBuf::default();
    reader.read_into(&mut buf, &mut Vec::new(), &mut ClusterTable::default()).expect("a frame");
    assert!(buf.packed.iter().all(|c| *c == PackedCell::BLANK), "{:?}", buf.packed);
    assert_eq!(buf.styles[1], PackedStyle::DEFAULT);
    // the table's own size is checked before anything is copied
    for count in [0, u32::MAX] {
        write_packed(&writer, cols, rows, &cells, &[0], Some(count));
        assert!(reader.read_into(&mut buf, &mut Vec::new(), &mut ClusterTable::default()).is_none());
    }
}

/// Frames change layout one to the next, and the buffer of the layout
/// no longer used is given back.
#[test]
fn the_layout_can_change_from_frame_to_frame() {
    let (cols, rows) = (5u16, 2u16);
    let mut writer = GridShmWriter::create(cols, rows).expect("region");
    let reader = GridShmReader::from_fd(dup_fd(writer.fd())).expect("reader");
    let mut grid = Grid::new(cols, rows);
    grid.set_cell(0, 0, Cell::from('w'));
    let mut buf = FrameBuf::default();
    let mut table = ClusterTable::default();
    writer.publish(&grid, 0, 0);
    reader.read_into(&mut buf, &mut Vec::new(), &mut table).expect("a frame");
    assert!(!buf.is_packed && buf.wide[0].ch == 'w');
    write_packed(&writer, cols, rows, &[PackedCell::new('p', 0).bits(); 10], &[0], None);
    reader.read_into(&mut buf, &mut Vec::new(), &mut table).expect("a frame");
    assert!(buf.is_packed && buf.packed[0].ch() == 'p');
    assert_eq!(buf.wide.capacity(), 0, "the twenty-byte buffer is kept");
    unsafe {
        (*writer.header()).version = VERSION;
        (*writer.header()).cell_size = CELL_SIZE_WIDE;
    }
    writer.last_publish_hash = 0;
    writer.publish(&grid, 0, 0);
    reader.read_into(&mut buf, &mut Vec::new(), &mut table).expect("a frame");
    assert!(!buf.is_packed && buf.wide[0].ch == 'w');
    assert_eq!(buf.packed.capacity(), 0, "the eight-byte buffers are kept");
}

/// Which regions a build attaches to: eight-byte cells only with the
/// version that says so, and no version past the newest it knows.
#[test]
fn eight_byte_cells_are_taken_only_with_their_version() {
    for (version, cell_size, ok) in [
        (VERSION, CELL_SIZE_WIDE, true),
        (VERSION_PACKED, CELL_SIZE_WIDE, true),
        (VERSION_PACKED, CELL_SIZE_PACKED, true),
        (VERSION, CELL_SIZE_PACKED, false),
        (VERSION_PACKED + 1, CELL_SIZE_PACKED, false),
        (VERSION_PACKED, 16, false),
    ] {
        let writer = GridShmWriter::create(2, 2).expect("region");
        unsafe {
            (*writer.header()).version = version;
            (*writer.header()).cell_size = cell_size;
        }
        let got = GridShmReader::from_fd(dup_fd(writer.fd()));
        assert_eq!(got.is_ok(), ok, "version {version}, {cell_size}-byte cells");
    }
}
