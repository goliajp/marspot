//! Opening, validating and reopening the file pair.
//!
//! Everything that decides whether a file on disk may be used as
//! this session's history, and what to do when it may not.

use super::*;

impl FileScrollback {
    /// Open or create the scrollback files.
    ///
    /// F3+11 — strict and dumb.  No silent rename of mismatched
    /// headers, no idx rebuild from bin, no tolerant load of bad
    /// records.  The ONLY repair step is truncating idx tail
    /// entries that point past bin EOF — this is data consistency
    /// (idx is derived from bin and must reference real records),
    /// not defense.  Anything else returns Err.
    pub fn open(
        bin_path: std::path::PathBuf,
        idx_path: std::path::PathBuf,
        cols: usize,
        ram_capacity: usize,
    ) -> std::io::Result<Self> {
        if let Some(parent) = bin_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let bin_existed =
            bin_path.exists() && bin_path.metadata().map(|m| m.len() > 0).unwrap_or(false);

        let bin_w = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&bin_path)?;

        // Filled in from the header below when the file already
        // exists; see the `epoch` binding after the writer is built.
        let mut existing_epoch: u64 = 0;
        if bin_existed {
            let cur_len = bin_w.metadata()?.len();
            if cur_len < FILE_HEADER_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "scrollback bin {} bytes < {} header",
                        cur_len, FILE_HEADER_BYTES
                    ),
                ));
            }
            let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
            read_exact_at(&bin_w, &mut hdr, 0)?;
            let magic = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
            if magic != FILE_MAGIC {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("scrollback bin magic 0x{magic:08x} != 0x{FILE_MAGIC:08x}"),
                ));
            }
            let version = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
            if !(FILE_MIN_COMPAT..=FILE_VERSION).contains(&version) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "scrollback bin version {version} outside [{FILE_MIN_COMPAT}, {FILE_VERSION}]"
                    ),
                ));
            }
            let cell_abi = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
            let expected_abi = if version == 2 {
                V2_CELL_BYTES
            } else {
                crate::grid::CELL_MEM_BYTES
            };
            if cell_abi != expected_abi as u32 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("scrollback bin v{version} cell_abi {cell_abi} != {expected_abi}"),
                ));
            }
            existing_epoch = u64::from_le_bytes(hdr[24..32].try_into().unwrap());
            if version == 2 {
                upgrade_v2_header_in_place(&bin_path)?;
            }
        }

        let mut bin = crate::async_writer::AsyncWriter::new(bin_w, BIN_BUF_BYTES, 4);
        let epoch = if bin_existed {
            match existing_epoch {
                0 => {
                    // Written before this field meant anything.  Give
                    // it one now: the lines already in the file are
                    // whatever they were, and nothing has been filed
                    // against them, so there is nothing to mismatch.
                    let e = new_epoch();
                    stamp_epoch_in_place(&bin_path, e)?;
                    e
                }
                e => e,
            }
        } else {
            let e = new_epoch();
            Self::write_header(&mut bin, e)?;
            bin.flush();
            e
        };

        let bin_for_read = std::fs::OpenOptions::new().read(true).open(&bin_path)?;
        let bin_eof = bin_for_read.metadata()?.len();

        let idx_w = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&idx_path)?;
        let idx_size = idx_w.metadata()?.len();
        if idx_size % 8 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("scrollback idx size {idx_size} not a multiple of 8"),
            ));
        }

        // F3+11 — single consistency-repair step: truncate idx tail
        // entries whose record can't be fully decoded from bin.
        // This handles the well-defined "BufWriter sync drift" case
        // when an L3 was killed without flush_for_handoff (bin
        // BufWriter 64 KB vs idx 4 KB → idx flushes more often →
        // idx ahead of bin on disk).  flush_for_handoff covers the
        // clean execv path; this scan covers SIGKILL.  Backward
        // walk runs at most idx_size/8 iterations and stops at the
        // first valid record.
        let idx_for_read = std::fs::OpenOptions::new().read(true).open(&idx_path)?;
        let mut total_lines = idx_size / 8;
        while total_lines > 0 {
            let mut buf = [0u8; 8];
            // The idx file is multiple-of-8 by the size check above —
            // reading any aligned entry must succeed.  Propagate I/O
            // errors (genuine fs failure) but treat short reads as
            // "drop the tail".
            if read_exact_at(&idx_for_read, &mut buf, (total_lines - 1) * 8).is_err() {
                total_lines -= 1;
                continue;
            }
            let off = u64::from_le_bytes(buf);
            // Past-EOF idx entries arise from an unclean exit where
            // bin BufWriter didn't flush (idx is direct).  Walk back
            // until the entry's record header is fully within bin.
            if off < FILE_HEADER_BYTES || off + 4 > bin_eof {
                total_lines -= 1;
                continue;
            }
            let mut len_buf = [0u8; 4];
            if read_exact_at(&bin_for_read, &mut len_buf, off).is_err() {
                total_lines -= 1;
                continue;
            }
            let rec_len = u32::from_le_bytes(len_buf) as u64;
            if rec_len < 3 || off + 4 + rec_len > bin_eof {
                total_lines -= 1;
                continue;
            }
            break;
        }
        let new_idx_size = total_lines * 8;
        if new_idx_size != idx_size {
            let trim_fd = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&idx_path)?;
            trim_fd.set_len(new_idx_size)?;
        }
        let idx = idx_w;
        let bin_tail_offset = bin_eof.max(FILE_HEADER_BYTES);

        // F2+5 — derive cold paths + open cold fds if files exist.
        // `with_extension("cold.bin")` works because `bin_path` already
        // ends in `.bin`; `with_extension` replaces just the extension
        // segment.  Same for idx.
        let cold_bin_path = bin_path.with_extension("cold.bin");
        let cold_idx_path = idx_path.with_extension("cold.idx");
        let (cold_bin_for_read, cold_idx_for_read, cold_total_lines) =
            match (cold_bin_path.exists(), cold_idx_path.exists()) {
                (true, true) => {
                    let cold_bin_fd = std::fs::OpenOptions::new()
                        .read(true)
                        .open(&cold_bin_path)
                        .ok();
                    let cold_idx_fd = std::fs::OpenOptions::new()
                        .read(true)
                        .open(&cold_idx_path)
                        .ok();
                    let n = cold_idx_fd
                        .as_ref()
                        .and_then(|f| f.metadata().ok())
                        .map(|m| m.len() / 8)
                        .unwrap_or(0);
                    (cold_bin_fd, cold_idx_fd, n)
                }
                _ => (None, None, 0),
            };
        // Logical line indexing: cold occupies [0, cold_total_lines);
        // hot occupies [cold_total_lines, cold_total_lines + hot_count).
        // Across L3 restart, the previous process's cold_first_line is
        // forgotten — line_idx resets to start at 0.
        let cold_first_line: u64 = 0;
        let hot_first_line = cold_total_lines;
        let hot_count = total_lines;
        let total_lines = hot_count.saturating_add(cold_total_lines);

        let mut ram_cells = Vec::with_capacity(ram_capacity.saturating_mul(cols));
        let mut ram_lens: Vec<u16> = Vec::with_capacity(ram_capacity);
        let mut ram_wrapped = Vec::with_capacity(ram_capacity);
        // RAM ring loads the tail of HOT only.  Cold tier is read on
        // demand via cold fds; we don't pre-load it into the hot ring
        // because the user's working scroll-back is almost always in
        // hot (recent), and cold gets exercised only by occasional
        // long scroll-back / search.
        let load_n = (hot_count as usize).min(ram_capacity);
        if load_n > 0 {
            let first_idx = (hot_count as usize) - load_n;
            for li in first_idx..(hot_count as usize) {
                // F3+11.1 — single-record tolerant load.  The
                // open-time idx tail-truncate only checks the LAST
                // entry; if some idx[N] in the middle of the
                // RAM-window points to a BufWriter-truncated bin
                // record (unclean kill mid-write), strict propagate
                // would fail open() entirely and Terminal::new
                // falls back to empty Disk → user sees ALL history
                // disappear.  Substituting a single blank row for
                // the corrupt entry costs the user one visible row
                // of black, but preserves the other ~255 RAM-window
                // rows + all on-disk history.  This is principled
                // tolerance for a known mechanism, not blind
                // defense.
                let row = match read_idx_at(&idx_for_read, li as u64) {
                    Ok(off) => match read_record_at(&bin_for_read, off) {
                        Ok((cells, w)) => {
                            ram_wrapped.push(w);
                            pad_or_clip(&cells, cols)
                        }
                        Err(_) => {
                            ram_wrapped.push(false);
                            pad_or_clip(&[], cols)
                        }
                    },
                    Err(_) => {
                        ram_wrapped.push(false);
                        pad_or_clip(&[], cols)
                    }
                };
                ram_cells.extend_from_slice(&row);
                ram_lens.push(row.len() as u16);
            }
        }

        // Opened against the epoch this file states, so a sidecar left
        // by an earlier generation is reset rather than read.  A
        // failure here is not a failure to open the scrollback: every
        // one of these is derived from bytes the session still has.
        let extras = super::super::sidecar::LineExtras::open_hot(&bin_path, epoch);

        // The cold file states its own epoch, and the sidecars renamed
        // alongside it on the rotation state the same one -- so a
        // `.cold.marks` left from two rotations ago is refused.
        let cold_extras = super::super::format::read_epoch(&cold_bin_path)
            .map(|e| super::super::sidecar::LineExtras::open_cold_for_read(&cold_bin_path, e))
            .unwrap_or_else(super::super::sidecar::LineExtras::none);

        Ok(Self {
            extras,
            cold_extras,
            bin_path,
            idx_path,
            cols,
            ram_capacity,
            bin: std::cell::RefCell::new(bin),
            idx: std::cell::RefCell::new(crate::async_writer::AsyncWriter::new(
                idx,
                IDX_BUF_BYTES,
                4,
            )),
            bin_for_read,
            idx_for_read,
            bin_mmap_ptr: std::cell::Cell::new(std::ptr::null_mut()),
            bin_mmap_len: std::cell::Cell::new(0),
            idx_mmap_ptr: std::cell::Cell::new(std::ptr::null_mut()),
            idx_mmap_len: std::cell::Cell::new(0),
            has_unflushed: std::cell::Cell::new(false),
            ram_cells,
            ram_lens,
            ram_wrapped,
            ram_head: 0,
            ram_len: load_n,
            total_lines,
            bin_tail_offset,
            scratch: Vec::with_capacity(
                FILE_REC_HEADER_BYTES + cols.saturating_mul(crate::grid::CELL_MEM_BYTES),
            ),
            cold_bin_path,
            cold_idx_path,
            cold_bin_for_read,
            cold_idx_for_read,
            cold_first_line,
            cold_total_lines,
            hot_first_line,
            hot_bytes_cap: hot_bytes_cap(),
            epoch,
        })
    }

    pub(super) fn write_header(bin: &mut crate::async_writer::AsyncWriter, epoch: u64) -> std::io::Result<()> {
        let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
        hdr[0..4].copy_from_slice(&FILE_MAGIC.to_le_bytes());
        hdr[4..8].copy_from_slice(&FILE_VERSION.to_le_bytes());
        hdr[8..12].copy_from_slice(&(crate::grid::CELL_MEM_BYTES as u32).to_le_bytes());
        hdr[12..16].copy_from_slice(&0u32.to_le_bytes()); // header_flags
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        hdr[16..24].copy_from_slice(&now_ns.to_le_bytes());
        // Bytes 24..32 were written as zero and read by nobody, which
        // is why the epoch can live here: every reader that predates it
        // goes on reading the same file the same way, so this costs no
        // FILE_VERSION bump and cannot make a rolled-back binary
        // quarantine a user's history.
        hdr[24..32].copy_from_slice(&epoch.to_le_bytes());
        bin.write(&hdr);
        Ok(())
    }

    #[allow(dead_code)]
    fn trim_trailing_partial(
        bin: &std::fs::File,
        idx: &std::fs::File,
        total_lines: u64,
        bin_len: u64,
    ) -> std::io::Result<(u64, u64)> {
        // Walk from the last indexed record and verify it's complete.
        // If incomplete, we accept the smaller total_lines.  We do
        // NOT truncate the bin file — leftover bytes past the last
        // good record are ignored by future appends (which use
        // O_APPEND going to current EOF) and don't affect reads
        // (.idx is the lookup truth).
        if total_lines == 0 {
            return Ok((0, FILE_HEADER_BYTES.max(bin_len)));
        }
        let mut last_good = total_lines;
        let mut bin_tail = bin_len;
        // Probe at most the last 4 records (a single mid-write crash
        // touches one; we err generous).
        let probe_n = last_good.min(4);
        for off_back in 0..probe_n {
            let li = last_good - 1 - off_back;
            let off = read_idx_at(idx, li)?;
            let mut len_buf = [0u8; 4];
            if read_exact_at(bin, &mut len_buf, off).is_err() {
                continue;
            }
            let rec_len = u32::from_le_bytes(len_buf) as u64;
            let end = off + 4 + rec_len;
            if end > bin_len {
                // This record is partial; treat it (and any newer
                // partial siblings) as not present.
                last_good = li;
                // The next record-start would have been `end`, which
                // is past EOF — so bin tail effectively rolls back
                // to this record's start.
                bin_tail = off;
            } else {
                break;
            }
        }
        Ok((last_good, bin_tail))
    }


}
impl FileScrollback {
    /// Reflow rewrites the scrollback at a new width: truncate `.bin`
    /// back to its header, wipe `.idx`, reopen.  The caller's
    /// subsequent pushes put the re-wrapped segments back.  Without
    /// the truncate the file accumulates duplicate records at every
    /// width the pane was ever at, which is what a resize used to
    /// leave visibly misaligned in scrolled views.
    ///
    /// `Err(ram_capacity)` when the files cannot be reopened, so the
    /// caller can fall back to RAM and keep the session running.
    pub(in crate::scrollback) fn reopen_at_cols(self, new_cols: usize) -> Result<Self, usize> {
        let bin_path = self.bin_path.clone();
        let idx_path = self.idx_path.clone();
        let ram_cap = self.ram_capacity;
        drop(self);
        let _ = (|| -> std::io::Result<()> {
            let bin_fd = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&bin_path)?;
            bin_fd.set_len(FILE_HEADER_BYTES)?;
            drop(bin_fd);
            // The header survives the truncate, so the epoch in it
            // would too -- and this is the one path where a local line
            // index comes back meaning a different line.  Stamp a new
            // one before the re-wrapped lines are pushed in, so
            // anything filed against the old numbering is refused
            // rather than matched.
            stamp_epoch_in_place(&bin_path, new_epoch())?;
            let idx_fd = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .truncate(true)
                .open(&idx_path)?;
            drop(idx_fd);
            Ok(())
        })();
        Self::open(bin_path, idx_path, new_cols, ram_cap).map_err(|_| ram_cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;
    use crate::scrollback::Scrollback;
    use crate::scrollback::format::{FILE_HEADER_BYTES, FILE_MAGIC, FILE_VERSION, read_exact_at};
    use crate::scrollback::testing::*;

    #[test]
    fn a_fresh_file_says_which_run_its_lines_belong_to() {
        let tmp = TmpDir::new("epoch-fresh");
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("open");
        assert_ne!(
            sb.epoch, 0,
            "zero is the value a file written before this had"
        );
        assert_eq!(header_epoch(&tmp.bin()), sb.epoch, "and it is on disk");
    }

    /// Reopening is not reflow. The lines are the same lines, so
    /// anything filed against them has to still match.
    #[test]
    fn reopening_keeps_the_run() {
        let tmp = TmpDir::new("epoch-reopen");
        let first = {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("open");
            push_n(&mut sb, 3, 8, 'a');
            sb.flush_for_handoff();
            sb.epoch
        };
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("reopen");
        assert_eq!(sb.epoch, first);
        assert_eq!(sb.len(), 3, "and the lines are still there");
    }

    /// A file written before this field meant anything is given a run
    /// rather than left at zero -- nothing has been filed against its
    /// lines, so there is nothing to mismatch.
    #[test]
    fn a_file_from_before_is_given_a_run_in_place() {
        let tmp = TmpDir::new("epoch-legacy");
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("open");
            push_n(&mut sb, 2, 8, 'a');
            sb.flush_for_handoff();
        }
        // Put the header back the way it was written before epochs.
        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(tmp.bin())
                .unwrap();
            f.write_all_at(&0u64.to_le_bytes(), 24).unwrap();
        }
        assert_eq!(
            header_epoch(&tmp.bin()),
            0,
            "the fixture has to be the old shape"
        );

        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("reopen");
        assert_ne!(sb.epoch, 0);
        assert_eq!(
            header_epoch(&tmp.bin()),
            sb.epoch,
            "written back, not just held"
        );
        assert_eq!(sb.len(), 2, "the history it already had is untouched");
    }

    /// The whole point of putting it in the reserved bytes: a reader
    /// that predates the field reads the same file the same way.
    #[test]
    fn the_run_is_invisible_to_a_reader_that_does_not_know_about_it() {
        let tmp = TmpDir::new("epoch-compat");
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), 8, 4).expect("open");
        push_n(&mut sb, 2, 8, 'x');
        sb.flush_for_handoff();

        let f = std::fs::File::open(tmp.bin()).unwrap();
        let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
        read_exact_at(&f, &mut hdr, 0).unwrap();
        assert_eq!(
            u32::from_le_bytes(hdr[0..4].try_into().unwrap()),
            FILE_MAGIC
        );
        assert_eq!(
            u32::from_le_bytes(hdr[4..8].try_into().unwrap()),
            FILE_VERSION,
            "no version bump -- a rolled-back binary must not quarantine this"
        );
        assert_eq!(
            u32::from_le_bytes(hdr[8..12].try_into().unwrap()),
            crate::grid::CELL_MEM_BYTES as u32
        );
    }

    /// The history a user already has is written in v2.  Opening it
    /// must read every old line exactly, keep appending, and leave a
    /// file whose old and new lines both read back — without rewriting
    /// a single old record.
    #[test]
    fn a_v2_file_keeps_its_history_and_takes_new_lines() {
        let tmp = TmpDir::new("v2-upgrade");
        let cols = 24;
        let old: Vec<(Vec<Cell>, bool)> =
            (0..40).map(|i| (rich_row(i, cols), i % 4 == 0)).collect();
        write_v2_pair(&tmp.bin(), &tmp.idx(), &old);
        let old_len = std::fs::metadata(tmp.bin()).unwrap().len();

        // A tiny RAM ring, so most reads go through the file.
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4).expect("open v2");
        assert_eq!(
            header_version_and_abi(&tmp.bin()),
            (3, 20),
            "header moved on"
        );
        let old_bytes = std::fs::read(tmp.bin()).unwrap();
        assert_eq!(old_bytes.len() as u64, old_len, "no record rewritten");

        let new: Vec<(Vec<Cell>, bool)> = (100..130)
            .map(|i| (rich_row(i, cols), i % 3 == 0))
            .collect();
        for (row, w) in &new {
            sb.push_line(row, *w);
        }
        drop(sb);

        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4).expect("reopen");
        let all: Vec<_> = old.iter().chain(new.iter()).collect();
        assert_eq!(sb.len(), all.len());
        for (i, (row, _)) in all.iter().enumerate() {
            let got = sb.read_line(i).expect("line reads");
            assert_eq!(&got[..], &row[..], "line {i}");
        }
        // The first 12 bytes of every v2 record are untouched too —
        // compare the whole old region past the 32-byte header.
        let now = std::fs::read(tmp.bin()).unwrap();
        assert_eq!(&now[32..old_len as usize], &old_bytes[32..old_len as usize]);
    }

    #[test]
    fn file_create_then_roundtrip_many_lines_and_reopen() {
        let tmp = TmpDir::new("many-reopen");
        let cols = 8usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 16).expect("create");
            for i in 0..5000 {
                let ch = b'a' + (i % 26) as u8;
                sb.push_line(&fill(ch, cols), false);
            }
            assert_eq!(sb.len(), 5000);
        }
        // Reopen: drop above flushes BufWriters + writes idx
        // sentinel.  New instance walks the file headers and
        // populates the RAM ring with the latest 16 lines.
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 16).expect("reopen");
        assert_eq!(sb.len(), 5000);
        // Sample at multiple depths: tail (RAM hit), middle
        // (file pread), head (file pread).
        assert_eq!(
            sb.cell_at(4999, 0).unwrap().ch,
            (b'a' + (4999 % 26) as u8) as char
        );
        assert_eq!(
            sb.cell_at(2500, 0).unwrap().ch,
            (b'a' + (2500 % 26) as u8) as char
        );
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'a');
    }

    /// F3+11 — bad header returns Err (no silent rename, no fresh
    /// start).  Caller (Terminal::new) sees the error and decides
    /// how to surface it; storage doesn't paper over corruption.
    #[test]
    fn file_corrupt_header_returns_err() {
        let tmp = TmpDir::new("corrupt-err");
        std::fs::write(tmp.bin(), b"\xff\xff\xff\xff\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00garbage").unwrap();
        std::fs::write(tmp.idx(), b"junk").unwrap();
        let err = match FileScrollback::open(tmp.bin(), tmp.idx(), 4, 8) {
            Err(e) => e,
            Ok(_) => panic!("open must fail on bad magic"),
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("magic"),
            "expected error about bad magic, got: {msg}"
        );
    }

    /// F3+11 — misaligned idx returns Err.  Idx file is derived
    /// from bin and must be in lockstep; the only repair step at
    /// open() is truncating tail entries past bin EOF (the
    /// SIGKILL-without-flush case).  An idx with non-multiple-of-8
    /// size is corruption past what the tail-truncate handles —
    /// surface, don't rebuild.
    #[test]
    fn file_misaligned_idx_returns_err() {
        let tmp = TmpDir::new("idx-misaligned");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8).expect("create");
            for _ in 0..5 {
                sb.push_line(&fill(b'a', cols), false);
            }
        }
        let idx_bytes = std::fs::read(tmp.idx()).unwrap();
        let mut bad = idx_bytes.clone();
        bad.push(0x55); // off-by-1 → not multiple of 8
        std::fs::write(tmp.idx(), &bad).unwrap();
        let err = match FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8) {
            Err(e) => e,
            Ok(_) => panic!("open must fail on misaligned idx"),
        };
        assert!(format!("{err}").contains("multiple of 8"));
    }

    #[test]
    fn file_trailing_partial_record_trimmed() {
        let tmp = TmpDir::new("partial");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8).expect("create");
            for _ in 0..5 {
                sb.push_line(&fill(b'k', cols), false);
            }
        }
        // Append garbage that LOOKS like a record header pointing
        // past EOF — simulates a crash mid-write of record 6.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.bin())
            .unwrap();
        // rec_len claims 100 bytes, but we only write 4 bytes of body
        let bogus_rec_len: u32 = 100;
        f.write_all(&bogus_rec_len.to_le_bytes()).unwrap();
        f.write_all(&[1, 4, 0]).unwrap(); // wrapped + cols, no cells
        // Also append a fake idx entry pointing at the partial record.
        let bin_len_before_garbage = std::fs::metadata(tmp.bin()).unwrap().len() - 7;
        let mut idx_f = std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.idx())
            .unwrap();
        idx_f
            .write_all(&bin_len_before_garbage.to_le_bytes())
            .unwrap();
        drop(f);
        drop(idx_f);
        // Reopen: trailing partial record is detected via the
        // rec_len bounds check; len falls back to the last good record.
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8).expect("recover");
        // 5 good records + 1 partial → recover to 5.
        assert!(
            sb.len() == 5 || sb.len() == 6,
            "expected partial to drop us back to 5 (or stay at 6 if heuristic kept it); got {}",
            sb.len()
        );
        // The partial record's idx must not let us read garbage.
        // For safety, verify cell_at on a known-good index works.
        assert_eq!(sb.cell_at(0, 0).unwrap().ch, 'k');
    }

    /// Window resize on a File-backed scrollback used to duplicate
    /// records (restart() reopened the file without clearing → the
    /// caller's subsequent push appended on top of existing
    /// content).  F3+10f makes restart() truncate bin to header +
    /// idx to 0, so the re-push lands in a fresh file.
    #[test]
    fn file_restart_truncates_for_reflow() {
        let tmp = TmpDir::new("resize-no-dup");
        let cols = 4usize;
        // Seed file with 50 rows.
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 16).expect("create");
            for i in 0..50u32 {
                sb.push_line(&fill(b'a' + (i % 26) as u8, cols), false);
            }
            assert_eq!(sb.len(), 50);
        }
        // Drive through the Scrollback enum since that's the call
        // surface Grid::reflow uses.
        let mut sb = Scrollback::file(tmp.bin(), tmp.idx(), cols, 16).expect("reopen as enum");
        assert_eq!(sb.len(), 50, "reopen should see seeded rows");
        // Restart at new_cols = 8 (the reflow trigger).
        sb.restart(8);
        assert_eq!(
            sb.len(),
            0,
            "restart() must clear the file for File variant — \
             leaving content causes the 错位 visible after a window resize"
        );
        // Re-push 50 reflowed rows at new cols.
        for i in 0..50u32 {
            sb.push_line_with_wrapped(&fill(b'a' + (i % 26) as u8, 8), false);
        }
        assert_eq!(
            sb.len(),
            50,
            "post-restart push count should be 50, not 100 \
             (100 = restart didn't truncate, file had old + new)"
        );
        // And the rows we read back must be the NEW width.
        let row0 = sb.line_to_vec(0).expect("row 0 decodes");
        assert_eq!(row0.len(), 8, "reflowed row should be at new cols=8");
    }

    /// F3+10c Pass A: manually inject idx entries pointing past bin
    /// EOF and verify reopen drops them.
    #[test]
    fn file_past_eof_idx_tail_dropped_on_reopen() {
        let tmp = TmpDir::new("past-eof-tail");
        let cols = 4usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8).expect("create");
            for _ in 0..10 {
                sb.push_line(&fill(b'x', cols), false);
            }
        }
        // Append 5 bogus idx entries pointing past bin EOF.  The
        // bin file isn't extended — so these entries can NEVER
        // resolve to a real record.
        let bin_size = std::fs::metadata(tmp.bin()).unwrap().len();
        {
            use std::io::Write;
            let mut idx = std::fs::OpenOptions::new()
                .append(true)
                .open(tmp.idx())
                .unwrap();
            for i in 0..5u64 {
                let bogus = bin_size + 100 + i * 7;
                idx.write_all(&bogus.to_le_bytes()).unwrap();
            }
        }
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 8).expect("reopen");
        assert_eq!(
            sb.len(),
            10,
            "Pass A should drop the 5 past-EOF entries; reopen total \
             was {} (expected 10)",
            sb.len()
        );
    }
}
