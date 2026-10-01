//! Appending lines, and rotating when the hot file is full.
//!
//! The write side runs on the parse thread, so what is here is the
//! per-line cost of keeping history: no allocation per line, no
//! fsync, and a rotation that hands the old pair over whole.

use super::*;

impl FileScrollback {
    /// F2+5 — rotate the current hot pair into cold and reopen a
    /// fresh hot pair.  Called by push_line when an incoming record
    /// would push hot past `hot_bytes_cap`.  Any existing cold pair
    /// is overwritten — that's the "delete" tier; once rotated past,
    /// older history is unrecoverable.  RAM ring contents are
    /// preserved (in-memory state is independent of the file).
    fn rotate_to_cold(&mut self) -> std::io::Result<()> {
        // Capture the count of rows about to leave hot for cold.
        let hot_count_before = self.total_lines - self.hot_first_line;
        if hot_count_before == 0 {
            // Nothing to rotate (the very first push exceeded cap on
            // an empty file — degenerate).  Just continue writing.
            return Ok(());
        }
        // 1) Flush writer buffers so the rename captures complete data.
        self.bin.borrow_mut().flush();
        self.idx.borrow_mut().flush();
        // 2) Drop everything that holds an fd / mmap to the hot
        //    files, so rename / unlink can succeed on platforms that
        //    care (we're macOS so unlink-while-open is OK, but a
        //    clean ordering makes the contract obvious).  We replace
        //    fields with placeholders we re-overwrite below.
        //
        //    munmap any mmaps held on the hot files; their underlying
        //    inode is about to change identity (rename → cold path,
        //    then create fresh hot at the old path).  Reusing the old
        //    mmap pointers would point at the renamed file, which is
        //    now logically cold — not what cell_at wants for hot reads.
        unsafe {
            let bp = self.bin_mmap_ptr.get();
            let bl = self.bin_mmap_len.get();
            if !bp.is_null() && bl > 0 {
                libc::munmap(bp as *mut _, bl);
            }
            self.bin_mmap_ptr.set(std::ptr::null_mut());
            self.bin_mmap_len.set(0);
            let ip = self.idx_mmap_ptr.get();
            let il = self.idx_mmap_len.get();
            if !ip.is_null() && il > 0 {
                libc::munmap(ip as *mut _, il);
            }
            self.idx_mmap_ptr.set(std::ptr::null_mut());
            self.idx_mmap_len.set(0);
        }
        // Drop the cold fds before rename overwrites their inodes.
        self.cold_bin_for_read = None;
        self.cold_idx_for_read = None;
        // Drop hot writers + readers (they hold fds on the soon-to-be-
        // renamed inode).  We rebuild them after rename.
        // Take ownership of the writers' inner BufWriters so they drop here.
        let _ = std::mem::replace(
            &mut *self.bin.borrow_mut(),
            crate::async_writer::AsyncWriter::new(dev_null_file()?, BIN_BUF_BYTES, 4),
        );
        let _ = std::mem::replace(
            &mut *self.idx.borrow_mut(),
            crate::async_writer::AsyncWriter::new(dev_null_file()?, IDX_BUF_BYTES, 4),
        );
        // Replace read fds with placeholders; we rebuild them post-rename.
        self.bin_for_read = dev_null_file()?;
        self.idx_for_read = dev_null_file()?;
        // 3) Delete any stale cold pair (defence in depth: rename
        //    on most filesystems would clobber, but be explicit).
        let _ = std::fs::remove_file(&self.cold_bin_path);
        let _ = std::fs::remove_file(&self.cold_idx_path);
        // 4) Rename hot → cold.
        std::fs::rename(&self.bin_path, &self.cold_bin_path)?;
        std::fs::rename(&self.idx_path, &self.cold_idx_path)?;
        // 5) Reopen fresh hot pair.
        let bin_w = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.bin_path)?;
        let mut bin = crate::async_writer::AsyncWriter::new(bin_w, BIN_BUF_BYTES, 4);
        self.epoch = new_epoch();
        Self::write_header(&mut bin, self.epoch)?;
        bin.flush();
        let idx_w = std::fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(&self.idx_path)?;
        // 6) Reinstall read fds for hot.
        let bin_for_read = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.bin_path)?;
        let idx_for_read = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.idx_path)?;
        *self.bin.borrow_mut() = bin;
        *self.idx.borrow_mut() = crate::async_writer::AsyncWriter::new(idx_w, IDX_BUF_BYTES, 4);
        self.bin_for_read = bin_for_read;
        self.idx_for_read = idx_for_read;
        self.bin_tail_offset = FILE_HEADER_BYTES;
        self.has_unflushed.set(false);
        // 7) Open the (newly renamed) cold read fds.
        self.cold_bin_for_read = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.cold_bin_path)
            .ok();
        self.cold_idx_for_read = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.cold_idx_path)
            .ok();
        // 8) Logical boundary bookkeeping.  Previous-hot's rows now
        //    live in cold; bump cold_first_line to where they were
        //    in logical-idx terms, and shift hot_first_line up so
        //    the next push lands at total_lines (no gap).
        self.cold_first_line = self.hot_first_line;
        self.cold_total_lines = hot_count_before;
        self.hot_first_line = self.total_lines;
        Ok(())
    }

    /// Append one line.  Hot path.
    pub fn push_line(&mut self, line: &[crate::grid::Cell], wrapped: bool) {
        // F3+11.1 — trim trailing cells equal to `Cell::default()`
        // before encoding.  This is pure disk-size optimisation; the
        // semantic is unchanged from the dumb-store model: every
        // push call produces exactly one record, with cols equal to
        // the trimmed cell count (may be 0 for fully-default rows).
        // Read path: cell_at returns None for col >= cols, which the
        // viewport renderer maps to Cell::default() — visually
        // identical to having stored the trailing default cells.
        // Critical perf: a 200-col grid where claudecode TUI uses
        // the first ~40 cols means 80% disk savings, and 80% of the
        // Grid::resize reflow read volume.  Without this, resize on
        // a session with megabytes of history takes seconds.
        let default_cell = crate::grid::Cell::default();
        let trimmed_len = line
            .iter()
            .rposition(|c| *c != default_cell)
            .map(|i| i + 1)
            .unwrap_or(0);
        let line = &line[..trimmed_len];
        let cols_u16 = line.len() as u16;
        let cell_bytes = crate::grid::Cell::slice_as_bytes(line);
        let rec_len = (1 + 2 + cell_bytes.len()) as u32;
        let total_bytes = 4 + rec_len as usize;
        // The record is the header plus the row's own memory.  This was
        // an encode loop — 13 bytes per cell, a match per colour — and
        // it was the hottest thing the parse thread did once the disk
        // writes moved off it; replacing it with this copy measured
        // +13–21 % on file-backed parse.  See FILE_VERSION for how v2
        // files keep reading.
        self.scratch.clear();
        self.scratch.extend_from_slice(&rec_len.to_le_bytes());
        self.scratch.push(wrapped as u8);
        self.scratch.extend_from_slice(&cols_u16.to_le_bytes());
        self.scratch.extend_from_slice(cell_bytes);

        // F2+5 — hot → cold rotation when the next record would
        // overflow the cap.  Failure to rotate is a real I/O error
        // (fs::rename ENOSPC etc.); panic rather than push past the
        // cap silently.
        if self.bin_tail_offset.saturating_add(total_bytes as u64) > self.hot_bytes_cap {
            self.rotate_to_cold().expect("scrollback hot→cold rotation");
        }
        let rec_offset = self.bin_tail_offset;

        // Bin first, then idx — `open()` recovers from the bin-
        // BufWriter-ahead-of-idx case via the tail-truncate scan.
        // I/O failure here = disk full / fs error; panic loudly so
        // the user knows storage broke instead of silently losing
        // history.
        self.bin.borrow_mut().write(&self.scratch);
        self.bin_tail_offset += total_bytes as u64;
        // Both writers are buffered; `ensure_flushed` / handoff /
        // Drop push them out in order (bin first) so the index never
        // references bytes the data file does not have.  open()'s
        // tail-truncate scan reconciles whatever an unclean kill
        // left behind, as it always has.
        self.idx.borrow_mut().write(&rec_offset.to_le_bytes());
        self.has_unflushed.set(true);

        self.push_into_ring(line, wrapped);
        self.total_lines += 1;
    }

    /// Cheap idempotent flush: a cold-read path calls this before
    /// consulting mmap / pread so any line that aged out of the RAM
    /// ring but still sits in the BufWriter user-space buffer is
    /// fully visible on disk.  Called at most once per cold-read
    /// burst because `has_unflushed` clears here and only `push_line`
    /// re-sets it.
    pub(super) fn ensure_flushed(&self) {
        if !self.has_unflushed.get() {
            return;
        }
        // Order matters: data before the index that points at it.
        self.bin.borrow_mut().flush();
        self.idx.borrow_mut().flush();
        self.has_unflushed.set(false);
    }

    /// F3+10 — call before L3 `execv` (Drop won't run, so the
    /// BufWriter tail would otherwise be lost — that's the original
    /// execv-gap bug).  Same flush path as `Drop`, just exposed as a
    /// public hook so `do_l3_execv_swap` can invoke it explicitly.
    /// snapshot v3's index-based dedup makes this a safety net rather
    /// than a strict correctness requirement (the snapshot already
    /// carries the BufWriter tail in its RAM-ring section), but
    /// flushing keeps the on-disk file in sync so cold reads of
    /// recent rows hit the file directly instead of having to wait
    /// for the next push_line to spill.
    pub fn flush_for_handoff(&self) {
        self.bin.borrow_mut().flush();
        self.has_unflushed.set(false);
    }

    /// Ensure `self.bin_mmap_*` covers at least `needed_len` bytes.
    /// First call mmaps the file; later calls remap when the file
    /// has grown.  Called from cold-read paths after `ensure_flushed`
    /// guarantees the kernel sees a consistent file.
    pub(super) fn ensure_bin_mmap_covers(&self, needed_len: u64) -> std::io::Result<()> {
        let cur_len = self.bin_mmap_len.get();
        if cur_len as u64 >= needed_len && !self.bin_mmap_ptr.get().is_null() {
            return Ok(());
        }
        // Remap to the file's current size (which may be larger than
        // needed_len — that's fine, virtual reservation only).
        let file_len = self.bin_for_read.metadata()?.len();
        if file_len == 0 {
            return Ok(());
        }
        // Unmap old if any.
        let old_ptr = self.bin_mmap_ptr.get();
        if !old_ptr.is_null() && cur_len > 0 {
            unsafe {
                libc::munmap(old_ptr as *mut libc::c_void, cur_len);
            }
        }
        // Map new.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                file_len as libc::size_t,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                std::os::unix::io::AsRawFd::as_raw_fd(&self.bin_for_read),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            self.bin_mmap_ptr.set(std::ptr::null_mut());
            self.bin_mmap_len.set(0);
            return Err(std::io::Error::last_os_error());
        }
        self.bin_mmap_ptr.set(ptr as *mut u8);
        self.bin_mmap_len.set(file_len as usize);
        Ok(())
    }

    pub(super) fn ensure_idx_mmap_covers(&self, needed_len: u64) -> std::io::Result<()> {
        let cur_len = self.idx_mmap_len.get();
        if cur_len as u64 >= needed_len && !self.idx_mmap_ptr.get().is_null() {
            return Ok(());
        }
        let file_len = self.idx_for_read.metadata()?.len();
        if file_len == 0 {
            return Ok(());
        }
        let old_ptr = self.idx_mmap_ptr.get();
        if !old_ptr.is_null() && cur_len > 0 {
            unsafe {
                libc::munmap(old_ptr as *mut libc::c_void, cur_len);
            }
        }
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                file_len as libc::size_t,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                std::os::unix::io::AsRawFd::as_raw_fd(&self.idx_for_read),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            self.idx_mmap_ptr.set(std::ptr::null_mut());
            self.idx_mmap_len.set(0);
            return Err(std::io::Error::last_os_error());
        }
        self.idx_mmap_ptr.set(ptr as *mut u8);
        self.idx_mmap_len.set(file_len as usize);
        Ok(())
    }

    /// Read the byte offset of line `idx` from the idx mmap, falling
    /// back to pread if mmap isn't covering yet.
    fn push_into_ring(&mut self, line: &[crate::grid::Cell], wrapped: bool) {
        if self.ram_capacity == 0 {
            return;
        }
        // Straight into the ring.  This used to go through
        // `pad_or_clip`, which returns a `Vec` — one allocation and one
        // extra copy of the row, per pushed line, on the parse thread.
        // The rule for hot paths is zero allocations; a bulk
        // `cat` pushes a quarter of a million lines through here.
        let cols = self.cols;
        let take = line.len().min(cols);
        if self.ram_len < self.ram_capacity {
            self.ram_cells.extend_from_slice(&line[..take]);
            // The slot still has to BE `cols` wide for the indexing
            // arithmetic; it just does not have to be written twice.
            self.ram_cells.resize(
                self.ram_cells.len() + (cols - take),
                crate::grid::Cell::default(),
            );
            self.ram_lens.push(take as u16);
            self.ram_wrapped.push(wrapped);
            self.ram_len += 1;
            return;
        }
        let slot = self.ram_head;
        self.ram_head = (self.ram_head + 1) % self.ram_capacity;
        let start = slot * cols;
        self.ram_cells[start..start + take].copy_from_slice(&line[..take]);
        self.ram_lens[slot] = take as u16;
        self.ram_wrapped[slot] = wrapped;
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::Cell;
    use crate::scrollback::Scrollback;
    use crate::scrollback::format::FILE_HEADER_BYTES;
    use crate::scrollback::testing::*;

    /// RFC-004 amendment — CSI 3 J reaches the persistent tier:
    /// clear() truncates the hot pair (and drops cold), so a reopen
    /// sees zero history, and pushes after clear persist normally.
    #[test]
    fn clear_truncates_persistent_file() {
        let dir = std::env::temp_dir().join(format!("marspot-sb-clear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("scrollback.bin");
        let idx = dir.join("scrollback.idx");
        let mut sb = Scrollback::file(bin.clone(), idx.clone(), 4, 8).expect("open");
        for i in 0..20u32 {
            sb.push_line_with_wrapped(&fill(b'a' + (i % 26) as u8, 4), false);
        }
        assert_eq!(sb.len(), 20);
        sb.clear();
        assert_eq!(sb.len(), 0, "cleared in-process");
        // Post-clear pushes work and are the ONLY surviving content.
        sb.push_line_with_wrapped(&fill(b'z', 4), false);
        sb.flush_for_handoff();
        assert_eq!(sb.len(), 1);
        drop(sb);
        let sb2 = Scrollback::file(bin, idx, 4, 8).expect("reopen");
        assert_eq!(
            sb2.len(),
            1,
            "reopen must see only post-clear content — clear must persist"
        );
        drop(sb2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F3+11.1 — push trims trailing default cells (perf:  80%+
    /// disk savings on a TUI grid where most of the row is
    /// padding).  Trim is purely a serialised-size optimisation:
    /// the prefix decodes back at its original columns; trimmed-
    /// tail columns decode as None which the viewport renderer
    /// maps to Cell::default() (visually identical to having
    /// stored them).
    #[test]
    fn push_line_trims_default_tail() {
        use std::io::{Read, Seek, SeekFrom};
        let tmp = TmpDir::new("trim-tail");
        let cols = 200usize;
        let prefix_len = 30usize;
        let mut row = Vec::with_capacity(cols);
        for i in 0..prefix_len {
            row.push(Cell {
                ch: (b'a' + (i % 26) as u8) as char,
                ..Default::default()
            });
        }
        for _ in prefix_len..cols {
            row.push(Cell::default());
        }
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4).expect("create");
        sb.push_line(&row, false);
        for _ in 0..6 {
            sb.push_line(&fill(b'.', cols), false);
        }
        drop(sb);
        let mut f = std::fs::File::open(tmp.bin()).expect("open bin");
        f.seek(SeekFrom::Start(FILE_HEADER_BYTES)).unwrap();
        let mut rec_len_buf = [0u8; 4];
        f.read_exact(&mut rec_len_buf).unwrap();
        let rec_len = u32::from_le_bytes(rec_len_buf) as usize;
        let expected_trimmed = 1 + 2 + prefix_len * crate::grid::CELL_MEM_BYTES;
        assert_eq!(
            rec_len, expected_trimmed,
            "first record should hold ONLY the prefix (trim default \
             tail); got rec_len={rec_len}, expected={expected_trimmed}"
        );
        let sb2 = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4).expect("reopen");
        for c in 0..prefix_len {
            let want = (b'a' + (c % 26) as u8) as char;
            assert_eq!(sb2.cell_at(0, c).unwrap().ch, want, "prefix col {c}");
        }
        // Trimmed-tail cells: cell_at returns None (the renderer's
        // higher-level cell_at_view maps None → Cell::default()).
        for c in prefix_len..cols {
            assert!(
                sb2.cell_at(0, c).is_none(),
                "trimmed tail col {c} should be None (renderer fills \
                 default), got Some"
            );
        }
    }

    /// F2+5 — rotation: when hot exceeds cap, rename hot → cold and
    /// open fresh hot.  Verify (a) cold files appear on disk,
    /// (b) cell_at for early-pushed rows still works through the cold
    /// tier, (c) cell_at for post-rotation rows works through hot,
    /// (d) a second rotation drops the OLDEST cold (delete tier).
    #[test]
    fn rotation_writes_cold_and_keeps_old_rows_readable() {
        let tmp = TmpDir::new("rotate");
        let cols = 8usize;
        // Tiny cap: header (32) + a handful of records.  Each record is
        // 4 + 1 + 2 + cols×13 = 7 + 104 = 111 bytes.
        // Cap at ~600 B → rotates after ~5 rows.
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "0") }; // 0 MB still ≥ default
        // 0 MB cap is degenerate; manually patch via direct construction
        // is not exposed — so set a non-zero cap that's still tiny.
        // 1 MB = 1048576 bytes; cap≥1MB won't trigger.  We instead set
        // an explicit small value via env override interpretation:
        // hot_bytes_cap() floors at value*1MB, so 0 effectively disables
        // rotation.  We want a small >0 trigger, so... use the unit
        // size 1MB and feed lots of rows.
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::set_var("MARSPOT_SCROLLBACK_HOT_CAP_MB", "1") };
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 4).expect("create");
        // 1 MB / ~111 B = ~9450 rows before rotation.  Push 12 000
        // distinct rows so at least one rotation happens.  Each row's
        // first cell encodes its sequence number (mod 26) so we can
        // verify the row's identity on read-back.
        let total_push = 12_000usize;
        for i in 0..total_push {
            let mut row: Vec<Cell> = Vec::with_capacity(cols);
            row.push(Cell {
                ch: (b'A' + (i % 26) as u8) as char,
                ..Default::default()
            });
            for _ in 1..cols {
                row.push(Cell {
                    ch: '.',
                    ..Default::default()
                });
            }
            sb.push_line(&row, false);
        }
        // Sanity: rotation must have happened.
        let cold_bin = tmp.bin().with_extension("cold.bin");
        let cold_idx_p = tmp.bin().with_extension("cold.idx");
        // bin_path stem is "scrollback" so .cold.bin file ought to exist.
        // (TmpDir::bin returns ".bin"; with_extension("cold.bin") yields
        // ".cold.bin".)
        assert!(
            cold_bin.exists(),
            "rotation should have produced a cold .bin at {:?}",
            cold_bin
        );
        // The idx path was passed separately; we apply with_extension on
        // it to derive its cold sibling.  Test helper paths align.
        let cold_idx = tmp.idx().with_extension("cold.idx");
        assert!(cold_idx.exists(), "cold .idx missing at {:?}", cold_idx);
        let _ = cold_idx_p; // silence unused name
        // (a) Earliest row (idx 0) should still resolve — comes from cold.
        let got0 = sb
            .cell_at(0, 0)
            .expect("cell_at(0, 0) returned None — early row should be in cold");
        assert_eq!(got0.ch, 'A', "cold row 0 first cell mismatch");
        // (b) Latest row (idx total-1) should resolve — comes from hot.
        let got_last = sb
            .cell_at(total_push - 1, 0)
            .expect("cell_at(last, 0) None — should be in hot");
        let want_last = (b'A' + ((total_push - 1) % 26) as u8) as char;
        assert_eq!(got_last.ch, want_last, "hot tail row mismatch");
        // (c) Mid-range row: hopefully also reachable (either in cold's
        // tail or hot's head depending on rotation point).  We just
        // require it round-trips correctly.
        let mid = total_push / 2;
        let got_mid = sb
            .cell_at(mid, 0)
            .expect("cell_at(mid, 0) None — mid row should be reachable in hot or cold");
        let want_mid = (b'A' + (mid % 26) as u8) as char;
        assert_eq!(got_mid.ch, want_mid, "mid row mismatch");
        // SAFETY: test/example code, single-threaded at this point (state-dir
        // mutations additionally serialized by the suite's state-dir lock).
        unsafe { std::env::remove_var("MARSPOT_SCROLLBACK_HOT_CAP_MB") };
    }

    /// A2: after a cold read mmaps the bin, subsequent pushes that
    /// grow the file beyond the current mmap_len must trigger a
    /// remap on the NEXT cold read.  Without remap, reading the
    /// newly-cold lines would access unmapped memory or stale length.
    #[test]
    fn file_a2_mmap_remap_on_file_growth() {
        let tmp = TmpDir::new("a2-remap");
        let cols = 4usize;
        let ram_capacity = 4;
        let mut sb =
            FileScrollback::open(tmp.bin(), tmp.idx(), cols, ram_capacity).expect("create");
        // First batch: push 20 lines, cold-read to mmap them.
        for i in 0..20 {
            sb.push_line(&fill(b'a' + (i % 26) as u8, cols), false);
        }
        let first_old_idx = 5;
        let _ = sb.cell_at(first_old_idx, 0); // triggers initial mmap
        let mmap_len_first = sb.bin_mmap_len.get();
        assert!(mmap_len_first > 0, "first mmap should be non-empty");
        // Second batch: push another 50 lines so the file grows
        // past mmap_len_first.
        for i in 20..70 {
            sb.push_line(&fill(b'a' + (i % 26) as u8, cols), false);
        }
        // Cold-read a line that landed in the second batch (now
        // aged out of ring of 4).
        let cold_after_growth = 30usize;
        let want_ch = (b'a' + (cold_after_growth % 26) as u8) as char;
        assert_eq!(
            sb.cell_at(cold_after_growth, 0)
                .expect("cell after growth")
                .ch,
            want_ch,
            "remap on growth should let us read freshly-cold lines"
        );
        assert!(
            sb.bin_mmap_len.get() > mmap_len_first,
            "mmap should have remapped to cover new file size: was {}, now {}",
            mmap_len_first,
            sb.bin_mmap_len.get()
        );
    }

    // ─── F3+10g: autotest harness for the bug classes that ate the
    //         user's history.  Each test reproduces one specific
    //         failure mode (torn write, resize duplication, past-EOF
    //         orphans, trailing-blank navigability) without going
    //         through the full marspot binary — these are the
    //         scenarios the user can't reliably eyeball.  See
    //         `[[project-scrollback-execv-gap]]` for the failure
    //         catalogue.  ──────────────────────────────────────────

    /// Torn write: BufWriter dropped without flush (the L3 self-execv
    /// scenario where Drop never runs).  Previously this caused idx
    /// to point past bin EOF after reopen → tolerant load surfaced
    /// blank rows → user saw "history is gone".  After F3+10c's Pass
    /// A, reopen drops past-EOF idx tail, every surfaced row decodes.
    #[test]
    fn file_torn_write_no_past_eof_orphans() {
        let tmp = TmpDir::new("torn-write");
        let cols = 8usize;

        // Push enough lines that BufWriter is mid-buffer when we
        // forget — 1000 × 8 cells writes ~100 KB to bin, well past
        // the 64 KB BufWriter auto-flush boundary so several
        // chunks have already hit disk and some tail is unflushed.
        let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 256).expect("create");
        for i in 0..1000u32 {
            let ch = b'a' + (i % 26) as u8;
            sb.push_line(&fill(ch, cols), false);
        }
        let pushed = sb.len();
        assert_eq!(pushed, 1000);

        // SAFETY: leak the FileScrollback — `Drop` (which flushes
        // BufWriters) never runs.  This is exactly what `libc::execv`
        // does to the old L3 process image.  The fd handles leak too;
        // tmp dir cleanup at the end of the test reclaims everything.
        std::mem::forget(sb);

        // Writes are handed to a writer thread, and `forget` does not
        // stop it — it only skips the `Drop` that would have flushed.
        // Give it a moment to drain what was already handed over, so
        // this test measures the buffered tail that was lost rather
        // than a race against a thread still writing.
        //
        // The real `execv` is harsher: it replaces the process image,
        // so in-flight buffers die with the thread.  That is exactly
        // why `flush_for_handoff` exists and why the L3 swap path
        // calls it — this test covers the case where nothing flushed
        // at all, which is the original execv-gap bug.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Reopen.  Post-F3+10c, every surfaced row must decode to
        // its expected character — no blank-by-tolerant-load rows
        // leaking through.
        let sb2 =
            FileScrollback::open(tmp.bin(), tmp.idx(), cols, 256).expect("reopen after torn close");
        let total = sb2.len();
        assert!(total <= pushed, "reopen total {total} > pushed {pushed}");
        // BufWriter auto-flush ensures a substantial prefix survived.
        assert!(total >= 500, "reopen total {total} suspiciously low");
        for i in 0..total {
            let line = sb2
                .read_line(i)
                .unwrap_or_else(|| panic!("row {i} should decode"));
            assert_eq!(line.len(), cols, "row {i} width mismatch");
            let expected = (b'a' + (i % 26) as u8) as char;
            assert_eq!(
                line[0].ch, expected,
                "row {i} decoded as {:?}, expected {:?} \
                 (would indicate past-EOF idx leaked as blank)",
                line[0].ch, expected,
            );
        }
    }

    /// Round-trip: push N lines, drop cleanly, reopen, verify ALL
    /// N decode to expected content.  Foundation correctness check
    /// — any regression to the BufWriter / Drop / open() machinery
    /// trips this first.
    #[test]
    fn file_clean_close_round_trips_all_rows() {
        let tmp = TmpDir::new("clean-roundtrip");
        let cols = 8usize;
        {
            let mut sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 32).expect("create");
            for i in 0..500u32 {
                sb.push_line(&fill(b'a' + (i % 26) as u8, cols), false);
            }
        } // <- clean Drop runs flush_for_handoff equivalent
        let sb = FileScrollback::open(tmp.bin(), tmp.idx(), cols, 32).expect("reopen");
        assert_eq!(sb.len(), 500);
        for i in 0..500 {
            let line = sb.read_line(i).expect("row decodes");
            assert_eq!(line.len(), cols);
            let expected = (b'a' + (i as u32 % 26) as u8) as char;
            assert_eq!(line[0].ch, expected, "row {i} content");
        }
    }
}
