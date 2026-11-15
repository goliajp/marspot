//! What is filed against a line besides its cells: where a command
//! began, and the text of clusters too wide for a cell.
//!
//! A note is a record of its own, written right after the record of the
//! line it belongs to, through the same buffer and the same writer --
//! so it costs the parse thread no system call, and it is in the same
//! segment, under the same use number, as its line: the two go out of
//! date together.  It has no index entry.  The notes of line `i` are
//! the records between the end of `i`'s record and where line `i + 1`'s
//! starts (or, for the last line, where the segment's writing stopped);
//! a line with several notes of a kind is read as the last of them.
//!
//! A note record: length (u32), flags with `REC_NOTE` set, the kind,
//! a zero byte, the low half of the segment's use number, the payload.
//! The use number guards the bytes past the last line of a segment,
//! where an earlier use's notes may still be.

use super::*;

const MARK: u8 = 1;
const CLUSTERS: u8 = 2;
/// Bytes of a note before its payload: length, flags, kind, zero, use.
const NOTE_HEAD: usize = 4 + 3 + 4;

impl FileScrollback {
    /// Record a mark against the line most recently pushed.
    ///
    /// Called only for the lines that carry one -- one per command,
    /// against a stream that is millions of lines.
    pub(in crate::scrollback) fn mark_last_line(&mut self, mark: crate::grid::PromptMark) {
        self.write_note(MARK, |out| out.extend_from_slice(&mark.to_bytes()));
    }

    /// File the clusters of the line most recently pushed.
    ///
    /// Called only for the lines that have any, so a stream of plain
    /// text never reaches here.
    pub(in crate::scrollback) fn cluster_last_line(&mut self, clusters: &[(u16, String)]) {
        self.write_note(CLUSTERS, |out| {
            out.extend_from_slice(&(clusters.len() as u16).to_le_bytes());
            for (col, s) in clusters {
                out.extend_from_slice(&col.to_le_bytes());
                out.extend_from_slice(&(s.len() as u16).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        });
    }

    /// A note of `kind` after the last line's record, built by `payload`
    /// in the record scratch.  A note that does not fit in what is left
    /// of the segment is not written: the line reads without it.
    fn write_note(&mut self, kind: u8, payload: impl FnOnce(&mut Vec<u8>)) {
        if self.hot_lines == 0 {
            return;
        }
        let serial = self.segs.last().expect("a hot segment").seg.hdr.serial as u32;
        self.scratch.clear();
        self.scratch.extend_from_slice(&[0, 0, 0, 0, REC_NOTE, kind, 0]);
        self.scratch.extend_from_slice(&serial.to_le_bytes());
        payload(&mut self.scratch);
        let len = self.scratch.len() as u64;
        if self.hot_pos + len > self.hot_data_end() {
            return;
        }
        let rec_len = (self.scratch.len() - 4) as u32;
        self.scratch[..4].copy_from_slice(&rec_len.to_le_bytes());
        self.bin.borrow_mut().write(&self.scratch);
        self.hot_pos += len;
        self.has_unflushed.set(true);
    }

    /// Each note of local line `local` of segment `s`, in order, as
    /// (kind, payload).
    fn seg_notes(&self, s: usize, local: u64, mut each: impl FnMut(u8, &[u8])) -> Option<()> {
        // a segment changed from moments ago may still be in a buffer
        self.ensure_flushed();
        let t = &self.segs[s];
        let mut p = t.record_end(t.entry(local)?)?;
        let end = if local + 1 < self.seg_lines(s) { t.entry(local + 1)? } else { self.seg_end(s) };
        while p < end {
            let body = t.body(p)?;
            if let Some((kind, payload)) = note_of(body, t.seg.hdr.serial) {
                each(kind, payload);
            }
            p += 4 + body.len() as u64;
        }
        Some(())
    }

    /// The command mark on logical line `line_idx`, or `None` when there
    /// is no mark there to find.
    pub(in crate::scrollback) fn mark_at(&self, line_idx: usize) -> crate::grid::PromptMark {
        let none = crate::grid::PromptMark::None;
        match self.locate(line_idx as u64) {
            Some(tiers::Loc::Seg(s, local)) => {
                let mut mark = none;
                self.seg_notes(s, local, |kind, payload| {
                    if let (MARK, Ok(b)) = (kind, <[u8; 2]>::try_from(payload)) {
                        mark = crate::grid::PromptMark::from_bytes(b);
                    }
                });
                mark
            }
            Some(tiers::Loc::Legacy(k, local)) => self.legacy[k].extras.read_mark(local),
            None => none,
        }
    }

    /// The clusters on logical line `line_idx`, appended to `out`.
    pub(in crate::scrollback) fn clusters_at(&self, line_idx: usize, out: &mut Vec<(u16, String)>) -> bool {
        match self.locate(line_idx as u64) {
            Some(tiers::Loc::Seg(s, local)) => {
                let before = out.len();
                let mut last: Option<std::ops::Range<usize>> = None;
                self.seg_notes(s, local, |kind, payload| {
                    if kind == CLUSTERS {
                        if let Some(r) = last.take() {
                            out.truncate(r.start);
                        }
                        let start = out.len();
                        super::super::clusters::decode_into(payload, out);
                        last = Some(start..out.len());
                    }
                });
                out.len() > before
            }
            Some(tiers::Loc::Legacy(k, local)) => self.legacy[k].extras.read_clusters(local, out),
            None => false,
        }
    }
}

/// The kind and payload of a note of use `serial`, or `None` for a
/// record that is a line, or a note an earlier use left.
pub(super) fn note_of(body: &[u8], serial: u64) -> Option<(u8, &[u8])> {
    if body.len() < NOTE_HEAD - 4 || body[0] & REC_NOTE == 0 {
        return None;
    }
    let use_lo = u32::from_le_bytes(body[3..7].try_into().expect("four bytes"));
    (use_lo == serial as u32).then(|| (body[1], &body[7..]))
}

/// Where the records and notes of segment `t` end, given its `lines`:
/// past the last line's record and every note of this use after it.
pub(super) fn used_end(t: &tiers::SegTier, lines: u64) -> u64 {
    let start = match lines.checked_sub(1) {
        None => return crate::segment::HEADER_BYTES,
        Some(last) => t.entry(last).and_then(|off| t.record_end(off)),
    };
    let Some(mut p) = start else { return crate::segment::HEADER_BYTES };
    while let Some(body) = t.body(p) {
        if note_of(body, t.seg.hdr.serial).is_none() {
            break;
        }
        p += 4 + body.len() as u64;
    }
    p
}
