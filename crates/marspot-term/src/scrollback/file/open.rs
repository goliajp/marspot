//! Opening a history: which files on disk are this session's lines,
//! and where writing goes on.
//!
//! What a process killed at any point leaves is read the same way:
//!
//! * a segment whose header is not whole is one a dead process was
//!   making ready: deleted;
//! * a segment whose header says it was never used is kept as the next
//!   one to use, if it is the size the next one should be;
//! * the lines of a segment are the run of index entries from its start
//!   that carry its use number -- found by halving, since the entries
//!   of this use come first and none after them can carry it -- and
//!   records that never got an entry are not lines;
//! * the hot segment is the one with the largest use number, and
//!   writing goes on after its last line and that line's notes;
//! * once two full segments are in use, everything older than the
//!   older of them is past the budget: deleted.

use std::sync::Arc;

use super::tiers::SegTier;
use super::*;
use crate::segment::{self, Header, Kind};

impl FileScrollback {
    /// Open or create the history kept beside `bin_path`.
    ///
    /// `InvalidData` when files of the format before segments are there
    /// and are not ones this build can read; the caller sets them aside
    /// and opens again.
    pub fn open(
        bin_path: std::path::PathBuf,
        idx_path: std::path::PathBuf,
        cols: usize,
        ram_capacity: usize,
    ) -> std::io::Result<Self> {
        Self::open_sized(bin_path, idx_path, cols, ram_capacity, Sizes::from_env())
    }

    pub(super) fn open_sized(
        bin_path: std::path::PathBuf,
        idx_path: std::path::PathBuf,
        cols: usize,
        ram_capacity: usize,
        sizes: Sizes,
    ) -> std::io::Result<Self> {
        let dir = bin_path.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
        std::fs::create_dir_all(&dir)?;
        let stem = bin_path.file_stem().and_then(|s| s.to_str()).unwrap_or("scrollback").to_string();
        let cell_abi = crate::grid::CELL_MEM_BYTES as u32;
        let listing = segment::list(&dir, &stem, Kind::Scrollback, cell_abi)?;
        for bad in &listing.bad {
            let _ = std::fs::remove_file(bad);
        }
        let mut next_slot =
            listing.used.iter().chain(&listing.spare).map(|s| s.slot + 1).max().unwrap_or(0);
        let mut next_serial = listing.used.last().map_or(1, |s| s.hdr.serial + 1);
        let mut used = listing.used;
        // past the budget: everything older than the older of the two
        // newest full segments
        let full_at: Vec<usize> =
            used.iter().enumerate().filter(|(_, s)| s.hdr.data == sizes.full).map(|(i, _)| i).collect();
        let over_budget = full_at.len() >= 2;
        if over_budget {
            let keep_from = full_at[full_at.len() - 2];
            for old in used.drain(..keep_from) {
                let _ = std::fs::remove_file(&old.path);
            }
        }
        let legacy = if over_budget {
            for p in legacy::all_paths(&bin_path, &idx_path) {
                let _ = std::fs::remove_file(p);
            }
            Vec::new()
        } else {
            legacy::open_all(&bin_path, &idx_path)?
        };
        let mut first = legacy.last().map_or(0, |l| l.first_line + l.lines);
        let mut segs = Vec::with_capacity(used.len() + 1);
        for seg in used {
            let mut t = SegTier::new(seg, first, 0, HEADER_BYTES);
            t.lines = valid_lines(&t);
            t.used_end = notes::used_end(&t, t.lines);
            first += t.lines;
            segs.push(t);
        }
        let mut spares = listing.spare;
        if segs.is_empty() {
            let seg = match spares.iter().position(|s| s.hdr.data == sizes.first) {
                Some(i) => spares.remove(i),
                None => {
                    let hdr = header(sizes.first, 0);
                    next_slot += 1;
                    segment::Prep::new(&dir, &stem, next_slot - 1, hdr).run()?
                }
            };
            let hdr = seg.hdr.used(next_serial, 0);
            std::os::unix::fs::FileExt::write_all_at(&*seg.file, &hdr.encode(), 0)?;
            next_serial += 1;
            segs.push(SegTier::new(Seg { hdr, ..seg }, first, 0, HEADER_BYTES));
        }
        let hot = segs.last().expect("a hot segment");
        let (hot_pos, hot_lines, hot_first_line) = (hot.used_end, hot.lines, hot.first_line);
        let main = crate::async_writer::Target::at(Arc::clone(&hot.seg.file), hot_pos);
        let side = crate::async_writer::Target::at(Arc::clone(&hot.seg.file), hot.seg.hdr.index_at() + hot_lines * 8);
        let bin = crate::async_writer::AsyncWriter::to(
            crate::async_writer::Files { main, side: Some(side) },
            BIN_BUF_BYTES,
            BIN_DEPTH,
        );
        let epoch = hot.seg.hdr.serial;

        // The RAM ring holds the tail of the hot segment.  A record that
        // cannot be read stands as a blank line: one row of black costs
        // less than refusing every other line the file holds.
        let mut ring = ring::Ring::new(ram_capacity);
        for local in hot_lines.saturating_sub(ram_capacity as u64)..hot_lines {
            let body = hot.entry(local).and_then(|off| hot.body(off)).filter(|b| ring::decode_body(b).is_some());
            ring.push(body.unwrap_or(&ring::EMPTY_BODY));
        }

        let mut sb = Self {
            dir,
            stem,
            cols,
            ram_capacity,
            bin: std::cell::RefCell::new(bin),
            has_unflushed: std::cell::Cell::new(false),
            ring,
            total_lines: hot_first_line + hot_lines,
            scratch: Vec::with_capacity(FILE_REC_HEADER_BYTES + cols.saturating_mul(crate::grid::CELL_MEM_BYTES)),
            legacy,
            segs,
            hot_pos,
            hot_lines,
            hot_first_line,
            mark: 0,
            line_mark: 0,
            next_serial,
            next_slot,
            sizes,
            spares: Vec::new(),
            pending: None,
            epoch,
            #[cfg(test)]
            hooks: Hooks::default(),
        };
        // a spare is kept only when it is the size the next change takes
        let wanted = sb.next_size();
        for s in spares.drain(..) {
            if Some(s.hdr.data) == wanted && sb.spares.is_empty() {
                sb.spares.push(s);
            } else {
                let _ = std::fs::remove_file(&s.path);
            }
        }
        sb.set_marks();
        Ok(sb)
    }
}

impl FileScrollback {
    /// How many lines, and how many bytes of records, the history kept
    /// beside `bin_path` holds on disk -- read from another process,
    /// changing nothing.
    pub fn on_disk(bin_path: &std::path::Path, idx_path: &std::path::Path) -> std::io::Result<(u64, u64)> {
        let dir = bin_path.parent().unwrap_or(std::path::Path::new("."));
        let stem = bin_path.file_stem().and_then(|s| s.to_str()).unwrap_or("scrollback");
        let listing = segment::list_for_reading(dir, stem, Kind::Scrollback, crate::grid::CELL_MEM_BYTES as u32)?;
        let (mut lines, mut bytes) = (0, 0);
        for l in legacy::open_all(bin_path, idx_path)? {
            lines += l.lines;
            bytes += l.bin.metadata()?.len().saturating_sub(FILE_HEADER_BYTES);
        }
        for seg in listing.used {
            let t = SegTier::new(seg, 0, 0, HEADER_BYTES);
            let n = valid_lines(&t);
            lines += n;
            bytes += notes::used_end(&t, n) - HEADER_BYTES;
        }
        Ok((lines, bytes))
    }
}

/// The header of an unused scrollback segment of `data` record bytes.
pub(super) fn header(data: u64, serial: u64) -> Header {
    Header {
        kind: Kind::Scrollback,
        cell_abi: crate::grid::CELL_MEM_BYTES as u32,
        data,
        index: Sizes::index_for(data),
        serial,
        base: 0,
    }
}

/// How many lines segment `t` holds: the entries from its start that
/// carry its use number, the last of them pointing at a whole record.
fn valid_lines(t: &SegTier) -> u64 {
    let cap = t.seg.hdr.index / 8;
    // entries of this use are a prefix: halve for its end
    let (mut lo, mut hi) = (0u64, cap);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if t.entry(mid).is_some() {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    // a record the index points at that was not written whole is the
    // write a kill cut short
    while lo > 0 && t.entry(lo - 1).and_then(|off| t.record_end(off)).is_none() {
        lo -= 1;
    }
    lo
}
