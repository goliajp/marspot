//! The newest lines of a file-backed history, kept in memory.
//!
//! A line that has just scrolled off is not in the file yet -- it is in
//! the writer's buffer -- and it is the line most likely to be read
//! next, by a wheel turned a notch while output still flows.  The ring
//! answers for those without a flush.
//!
//! It holds each line as the record the file gets: runs of cells that
//! share their attributes.  It held the cells themselves once, twenty
//! bytes each in a slot as wide as the pane, and every line that
//! scrolled off was copied into it a second time at that size -- a ring
//! of 600 KiB a flood wrote through from end to end, for a tier that is
//! read when someone scrolls.  A record of plain text is a byte a cell;
//! the copy is of what was already built for the file, and reading pays
//! for the decode instead.

use std::cell::RefCell;

use super::*;
use crate::grid::Cell;

pub(super) struct Ring {
    capacity: usize,
    /// One record body per line, oldest at `head` once full: flags,
    /// columns, cells.  A slot keeps its allocation from line to line.
    recs: Vec<Vec<u8>>,
    head: usize,
    bytes: usize,
    /// Lines ever pushed: what a decoded row is remembered by, so a
    /// slot written again is not mistaken for the line it held.
    pushed: u64,
    /// The row last read, decoded.  A reader takes a row cell by cell.
    row: RefCell<(Option<u64>, Vec<Cell>)>,
}

impl Ring {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            recs: Vec::with_capacity(capacity),
            head: 0,
            bytes: 0,
            pushed: 0,
            row: RefCell::new((None, Vec::new())),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.recs.len()
    }

    /// What the records held add up to.
    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Add the newest line; `body` is its record without the length.
    pub(super) fn push(&mut self, body: &[u8]) {
        if self.capacity == 0 {
            return;
        }
        self.pushed += 1;
        self.bytes += body.len();
        if self.recs.len() < self.capacity {
            self.recs.push(body.to_vec());
            return;
        }
        let rec = &mut self.recs[self.head];
        self.head = (self.head + 1) % self.capacity;
        self.bytes -= rec.len();
        rec.clear();
        rec.extend_from_slice(body);
    }

    pub(super) fn clear(&mut self) {
        self.recs.clear();
        self.head = 0;
        self.bytes = 0;
        self.row.borrow_mut().0 = None;
    }

    /// The record of line `i`, counted from the oldest held.
    fn rec(&self, i: usize) -> &[u8] {
        &self.recs[(self.head + i) % self.recs.len()]
    }

    pub(super) fn wrapped(&self, i: usize) -> bool {
        self.rec(i).first().is_some_and(|flags| flags & REC_WRAPPED != 0)
    }

    /// The cells of line `i`, without the blank ones it ended in.
    pub(super) fn with_row<R>(&self, i: usize, read: impl FnOnce(&[Cell]) -> R) -> R {
        let seq = self.pushed - (self.recs.len() - i) as u64;
        let mut row = self.row.borrow_mut();
        if row.0 != Some(seq) {
            row.1 = decode_body(self.rec(i)).unwrap_or_default();
            row.0 = Some(seq);
        }
        read(&row.1)
    }
}

/// A record body with no cells: what stands in for a line whose record
/// cannot be read.
pub(super) const EMPTY_BODY: [u8; 3] = [0; 3];

/// The cells of a record body, or `None` for bytes that are not one.
pub(super) fn decode_body(body: &[u8]) -> Option<Vec<Cell>> {
    let (head, cells) = body.split_first_chunk::<3>()?;
    decode_record_cells(head[0], cells, u16::from_le_bytes([head[1], head[2]]) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{CellAttrs, Color};

    fn body(line: &[Cell], wrapped: bool) -> Vec<u8> {
        let mut out = vec![REC_PACKED | if wrapped { REC_WRAPPED } else { 0 }];
        out.extend_from_slice(&(line.len() as u16).to_le_bytes());
        assert!(encode_packed(line, &mut out), "a line of runs");
        out
    }

    fn text(s: &str) -> Vec<Cell> {
        s.chars().map(|ch| Cell { ch, ..Default::default() }).collect()
    }

    fn read(ring: &Ring, i: usize) -> String {
        ring.with_row(i, |cells| cells.iter().map(|c| c.ch).collect())
    }

    /// Past its capacity the ring holds the newest lines, oldest first,
    /// and a slot written again reads as its new line -- also to a
    /// reader that had the old one decoded.
    #[test]
    fn the_newest_lines_are_held_in_order() {
        let mut ring = Ring::new(3);
        for (n, line) in ["one", "two", "three"].into_iter().enumerate() {
            ring.push(&body(&text(line), n == 1));
        }
        assert_eq!((read(&ring, 0), read(&ring, 1), read(&ring, 2)), ("one".into(), "two".into(), "three".into()));
        assert!(ring.wrapped(1) && !ring.wrapped(0) && !ring.wrapped(2));
        // the row of the slot about to be reused is the one decoded
        assert_eq!(read(&ring, 0), "one");
        ring.push(&body(&text("four"), false));
        assert_eq!(ring.len(), 3);
        assert_eq!((read(&ring, 0), read(&ring, 1), read(&ring, 2)), ("two".into(), "three".into(), "four".into()));
        assert!(ring.wrapped(0));
    }

    /// Attributes and characters of every width come back as pushed.
    #[test]
    fn a_line_reads_back_cell_for_cell() {
        let red = CellAttrs { fg: Color::indexed(1), bold: true, ..Default::default() };
        let mut line = text("plain ");
        line.extend("漢字 🚀".chars().map(|ch| Cell { ch, attrs: red }));
        line.extend(text(" tail"));
        let mut ring = Ring::new(2);
        ring.push(&body(&line, false));
        ring.with_row(0, |cells| assert_eq!(cells, &line[..]));
    }

    /// The bytes held follow what is in the ring, not what has passed
    /// through it, and a cleared ring holds nothing.
    #[test]
    fn the_bytes_held_are_those_of_the_lines_held() {
        let mut ring = Ring::new(2);
        let (short, long) = (body(&text("ab"), false), body(&text("abcdefgh"), false));
        ring.push(&long);
        ring.push(&short);
        assert_eq!(ring.bytes(), long.len() + short.len());
        ring.push(&short);
        assert_eq!(ring.bytes(), 2 * short.len());
        ring.clear();
        assert_eq!((ring.len(), ring.bytes()), (0, 0));
        ring.push(&long);
        assert_eq!(read(&ring, 0), "abcdefgh");
    }

    /// Bytes that are not a record read as an empty line.
    #[test]
    fn a_record_that_cannot_be_read_is_a_blank_line() {
        let mut ring = Ring::new(2);
        ring.push(&[REC_PACKED, 5, 0, 9, 9]);
        ring.push(&EMPTY_BODY);
        assert_eq!((read(&ring, 0), read(&ring, 1)), (String::new(), String::new()));
    }
}
