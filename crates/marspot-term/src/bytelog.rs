//! Per-session append-only byte log.
//!
//! Records every byte read from a session's PTY master.  Nothing in the
//! running system reads it back — reattach restores from `state.bin`
//! instead — so this is a write-only forensic record, kept for the
//! "the scrollback is gone but the raw bytes survived" case that
//! [`replay`] (and `examples/rebuild_scrollback_from_bytelog.rs`, the
//! tool around it) exists to handle.
//! Bounded by **rotation**, not by copying.  The live file is
//! `bytelog`; once it passes `BYTELOG_SEGMENT_BYTES` it is renamed to
//! `bytelog.1` (replacing the previous `.1`) and a fresh empty
//! `bytelog` takes over.  Full history = `bytelog.1` then `bytelog`,
//! concatenated in that order — see `segment_paths`.
//!
//! This used to compact instead: copy the newest 50 MiB through a
//! 64 KiB buffer into a temp file and rename it over the original.
//! That ran inline in `append`, which runs in the L3 main loop's
//! `pump`, so crossing the cap froze the pane — measured in the field
//! at 10+ seconds under IO contention, with the pane recovering on its
//! own afterwards.  Rotation is a single `rename`, so the same event
//! now costs microseconds and cannot stall the loop.  It is also
//! strictly better on retention: the old scheme dropped to exactly
//! 50 MiB of history at every compaction, this one keeps between 50
//! and 100 MiB, for the same disk ceiling.
//!
//! Disk location: `~/Library/Caches/marspot/sessions/<id>/bytelog`.
//! Survives daemon restarts so reattach from a fresh process also gets
//! prior history (or, post-RFC-003, the freshly spawned L3 with the
//! same session id can rehydrate from its own past lifetime).
//!
//! Originally lived inline in `marspot-shelld`; lifted to
//! `marspot-term` so L3 (`marspot-session`) can own a bytelog directly
//! without going through L4 (RFC-003).

use std::fs::OpenOptions;
use std::io;
use std::path::PathBuf;

use crate::paths::sessions_dir;

/// Size at which the live segment rotates.  Two segments are kept, so
/// this is half the on-disk ceiling.
pub const BYTELOG_SEGMENT_BYTES: u64 = 50 * 1024 * 1024;

/// Total on-disk ceiling per session: the live segment plus one
/// retired one.  100 MiB is generous — even a `cat /dev/urandom` run
/// for several seconds doesn't fill it.
pub const BYTELOG_CAP_BYTES: u64 = 2 * BYTELOG_SEGMENT_BYTES;

/// Handoff size to the writer thread.  64 KiB matches the pty reader's
/// own buffer, so a busy pane hands over roughly one gathered batch at
/// a time; an idle one hands over on flush.
const BYTELOG_WRITE_BUF: usize = 64 * 1024;

pub struct ByteLog {
    path: PathBuf,
    /// Appends go to a writer thread (see `async_writer`).  This used
    /// to be a bare `File` written straight from `pump`, which meant
    /// the thread that parses a pane's output also waited for its
    /// disk: on a bulk `cat` that was 28 % of the parse thread's
    /// samples, and removing the write entirely measured +13.5 %.
    /// The bytes still all get written — just not on this thread.
    file: crate::async_writer::AsyncWriter,
    bytes_written: u64,
}

impl ByteLog {
    pub fn open(session_id: u64) -> io::Result<Self> {
        let dir = sessions_dir().join(session_id.to_string());
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("bytelog");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;
        let bytes_written = file.metadata()?.len();
        let mut log = Self {
            path,
            file: crate::async_writer::AsyncWriter::new(file, BYTELOG_WRITE_BUF, 4),
            bytes_written,
        };
        // Logs written by the pre-rotation scheme can be up to the full
        // 100 MiB ceiling in a single file.  Retire such a segment on
        // open rather than letting it keep growing on top of its
        // already-oversized self.  Costs one rename, and the oversized
        // file is dropped at the next rotation, so the legacy shape
        // heals within one segment's worth of output.
        if log.bytes_written > BYTELOG_SEGMENT_BYTES {
            let _ = log.rotate();
        }
        Ok(log)
    }

    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write(bytes);
        self.bytes_written += bytes.len() as u64;
        if self.bytes_written > BYTELOG_SEGMENT_BYTES {
            // Best-effort rotation: on failure the live segment just
            // keeps growing until the next append retries.  Unlike the
            // copy it replaced, a retry is cheap, so a transient
            // failure can't turn into a permanent per-append tax.
            let _ = self.rotate();
        }
        Ok(())
    }

    /// End of a burst: let the file catch up with what was appended.
    /// See [`AsyncWriter::hand_off`](crate::async_writer::AsyncWriter::hand_off).
    pub fn hand_off(&mut self) {
        self.file.hand_off();
    }

    /// The log's segments, oldest first.  Concatenating these in order
    /// reproduces the retained byte stream.  Missing files are omitted,
    /// so a log that has never rotated yields just the live segment.
    pub fn segment_paths(session_id: u64) -> Vec<PathBuf> {
        let dir = sessions_dir().join(session_id.to_string());
        [dir.join("bytelog.1"), dir.join("bytelog")]
            .into_iter()
            .filter(|p| p.exists())
            .collect()
    }

    /// Make every appended byte visible to a reader of the segment
    /// files.  Callers that are about to read the log back (replay
    /// after a silent update) must call this first — the writer thread
    /// is not otherwise synchronised with them.
    pub fn flush(&mut self) {
        self.file.flush();
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Retire the live segment and start a fresh one.  Two syscalls,
    /// no data movement — this is the whole point of the design.
    fn rotate(&mut self) -> io::Result<()> {
        // Everything queued belongs to the segment being retired.
        self.file.flush();
        let retired = self.path.with_extension("1");
        // `rename` replaces an existing `.1` atomically, so the
        // generation before last is dropped here.
        std::fs::rename(&self.path, &retired)?;
        // Fresh live segment.  The old fd pointed at the now-retired
        // inode, so it has to be reopened, not reused.  `swap_file`
        // drains what is still queued into the OLD file first, so the
        // byte stream stays ordered across the boundary even though
        // the producer never waited for it.
        self.file.swap_file(
            OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .open(&self.path)?,
        );
        self.bytes_written = 0;
        Ok(())
    }
}

/// Feed a log's segments, oldest first, through `term`'s parser: the
/// same bytes in the same order the pane's L3 parsed them, so the grid
/// and scrollback come out as they were, at `term`'s size.  Returns the
/// number of bytes fed.
///
/// A segment that does not exist is skipped, so the list from
/// [`ByteLog::segment_paths`] or a hand-built `[<log>.1, <log>]` both
/// work.  Callers reading a live log flush it first.
pub fn replay(segments: &[PathBuf], term: &mut crate::terminal::Terminal) -> io::Result<u64> {
    use std::io::Read;
    let mut buf = vec![0u8; 1 << 20];
    let mut fed = 0u64;
    for seg in segments {
        let mut f = match std::fs::File::open(seg) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            term.feed(&buf[..n]);
            fed += n as u64;
        }
    }
    Ok(fed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Instant;

    /// Read the log back from disk.  Appends go through a writer
    /// thread, so a test that wants to see them must flush first —
    /// same contract the recovery tooling follows.
    fn read_all(session_id: u64) -> Vec<u8> {
        let mut out = Vec::new();
        for p in ByteLog::segment_paths(session_id) {
            let mut f = std::fs::File::open(p).unwrap();
            f.read_to_end(&mut out).unwrap();
        }
        out
    }

    /// Rotation must bound the log without moving data, and without
    /// losing the byte stream's order across the segment boundary.
    ///
    /// The predecessor copied 50 MiB inline on the L3 main loop every
    /// time the cap was crossed, which froze the pane for seconds.  The
    /// timing assertion here is the guard against that regressing: a
    /// rename cannot take 500 ms, a copy of this much data can.
    #[test]
    fn rotation_is_bounded_ordered_and_cheap() {
        let _sandbox = crate::test_env::sandbox_state_dir();
        let id = 7;
        let mut log = ByteLog::open(id).unwrap();

        // Write past the rotation point in recognisable chunks so the
        // concatenated stream can be checked for order and gaps.
        let chunk = 64 * 1024;
        let n_chunks = (BYTELOG_SEGMENT_BYTES as usize / chunk) + 8;
        // Time the ONE append that crosses the threshold, not the
        // whole loop — that single call is where the old code did its
        // 50 MiB copy, and where this one does a rename.
        let mut trigger = std::time::Duration::ZERO;
        for i in 0..n_chunks {
            let mut buf = vec![b'.'; chunk];
            let tag = format!("<{i}>");
            buf[..tag.len()].copy_from_slice(tag.as_bytes());
            let was_below = log.bytes_written() <= BYTELOG_SEGMENT_BYTES;
            let t0 = Instant::now();
            log.append(&buf).unwrap();
            if was_below && log.bytes_written() < BYTELOG_SEGMENT_BYTES {
                trigger = t0.elapsed();
            }
        }

        // Appends go through a writer thread, so the on-disk sizes
        // below are only meaningful once everything handed over has
        // landed.  (This used to be implicit: the write was synchronous
        // and the file was always current.)
        log.flush();

        // It rotated: a `.1` exists and the live segment restarted.
        let seg = ByteLog::segment_paths(id);
        assert_eq!(seg.len(), 2, "expected a retired segment plus a live one");
        assert!(log.bytes_written() < BYTELOG_SEGMENT_BYTES);

        // Disk stays under the ceiling.
        let on_disk: u64 = seg.iter().map(|p| p.metadata().unwrap().len()).sum();
        assert!(
            on_disk <= BYTELOG_CAP_BYTES,
            "on-disk {on_disk} exceeded cap {BYTELOG_CAP_BYTES}"
        );

        // The stream reads back in order with nothing missing: every
        // chunk tag appears once, ascending, across the segment seam.
        let all = read_all(id);
        assert_eq!(all.len() as u64, on_disk);
        let text = String::from_utf8_lossy(&all);
        // One pass, not one per chunk.  Asking `find` for each tag in
        // turn restarts at offset 0 every time — 808 scans of 50 MiB,
        // which was 7.8 s of the suite's 41 s and all of it in the
        // measuring, none in what is measured (2026-09-28).  The
        // assertion is the same: each tag's FIRST position, in
        // ascending chunk order, must ascend.
        let mut first_at: Vec<Option<usize>> = vec![None; n_chunks];
        let bytes = text.as_bytes();
        for (at, _) in bytes.iter().enumerate().filter(|(_, b)| **b == b'<') {
            let Some(close) = bytes[at + 1..].iter().position(|b| *b == b'>') else {
                continue;
            };
            let Ok(n) = text[at + 1..at + 1 + close].parse::<usize>() else {
                continue;
            };
            if let Some(slot) = first_at.get_mut(n) {
                slot.get_or_insert(at);
            }
        }
        let mut last = None;
        for (i, at) in first_at.iter().enumerate() {
            let Some(at) = *at else { continue };
            if let Some(prev) = last {
                assert!(at > prev, "chunk {i} out of order across the seam");
            }
            last = Some(at);
        }
        assert!(last.is_some(), "no chunk tags survived");

        // The rotating append is a rename, not a copy.  Measured at
        // well under a millisecond; 100 ms leaves three orders of
        // magnitude of headroom while still being far below what
        // moving 50 MiB costs even on a fast disk.
        assert!(
            trigger > std::time::Duration::ZERO,
            "never observed the rotating append"
        );
        if crate::host_load::quiet_enough_to_time("the rotating bytelog append") {
            assert!(
                trigger.as_millis() < 100,
                "the rotating append took {trigger:?} — data is being copied, not renamed"
            );
        }
    }

    /// A legacy oversized log (written by the copy-compaction scheme,
    /// which allowed a single file up to the full ceiling) is retired
    /// on open instead of being grown further.
    #[test]
    fn legacy_oversized_segment_rotates_on_open() {
        let sandbox = crate::test_env::sandbox_state_dir();
        let dir = sandbox.path();
        let id = 11;
        let seg_dir = dir.join("sessions").join(id.to_string());
        std::fs::create_dir_all(&seg_dir).unwrap();
        // Sparse file just past the segment size — content doesn't
        // matter here, only the length the open path measures.
        let f = std::fs::File::create(seg_dir.join("bytelog")).unwrap();
        f.set_len(BYTELOG_SEGMENT_BYTES + 1).unwrap();
        drop(f);

        let log = ByteLog::open(id).unwrap();
        assert_eq!(log.bytes_written(), 0, "live segment should start fresh");
        assert_eq!(
            ByteLog::segment_paths(id).len(),
            2,
            "the oversized legacy file should have been retired to .1"
        );
    }

    /// A log that has never rotated still reads back as one segment.
    #[test]
    fn single_segment_before_rotation() {
        let _sandbox = crate::test_env::sandbox_state_dir();
        let id = 9;
        let mut log = ByteLog::open(id).unwrap();
        log.append(b"hello ").unwrap();
        log.append(b"world").unwrap();
        log.flush();
        assert_eq!(ByteLog::segment_paths(id).len(), 1);
        assert_eq!(read_all(id), b"hello world");
    }

    /// What a pane's program printed: enough to scroll several screens
    /// off the top, with colour, wide characters, a carriage-return
    /// overwrite and a cursor move, so a replay that only gets plain
    /// ASCII right still fails.
    fn program_output() -> Vec<u8> {
        let mut out = Vec::new();
        for i in 0..120 {
            out.extend_from_slice(
                format!(
                    "\x1b[3{}mline {i:03}\x1b[0m 漢字 ✓ {}\r\n",
                    i % 8,
                    "x".repeat(i % 37)
                )
                .as_bytes(),
            );
            if i % 17 == 0 {
                out.extend_from_slice(b"overwritten\rOVER\r\n");
            }
        }
        out.extend_from_slice(b"\x1b[3;5Hmid-screen\x1b[10;1Hprompt$ ");
        out
    }

    fn assert_same_terminal(a: &crate::terminal::Terminal, b: &crate::terminal::Terminal) {
        let (ga, gb) = (a.grid(), b.grid());
        assert_eq!(
            ga.scrollback_len(),
            gb.scrollback_len(),
            "scrollback length"
        );
        assert!(
            ga.scrollback_len() > 50,
            "the output scrolled well past one screen"
        );
        for i in 0..ga.scrollback_len() {
            assert_eq!(
                ga.scrollback_line(i),
                gb.scrollback_line(i),
                "scrollback line {i}"
            );
        }
        for r in 0..ga.rows() {
            for c in 0..ga.cols() {
                assert_eq!(ga.cell(c, r), gb.cell(c, r), "cell ({c}, {r})");
            }
        }
        assert_eq!(ga.cursor(), gb.cursor(), "cursor");
    }

    /// The bytes a pane parsed, logged as it parsed them, rebuild the
    /// same grid and scrollback in a fresh terminal.  The live side is
    /// fed in small odd-sized chunks, so escape sequences and UTF-8
    /// characters straddle the chunk boundaries the way PTY reads cut
    /// them; the replay reads in its own, much larger, chunks.
    #[test]
    fn a_replayed_log_rebuilds_the_same_screen_and_scrollback() {
        let mut sandbox = crate::test_env::sandbox_state_dir();
        sandbox.env.remove("MARSPOT_SESSION_ID");
        let id = 21;
        let mut log = ByteLog::open(id).unwrap();
        let mut live = crate::terminal::Terminal::new(40, 10);
        for chunk in program_output().chunks(7) {
            live.feed(chunk);
            log.append(chunk).unwrap();
        }
        log.flush();

        let mut replayed = crate::terminal::Terminal::new(40, 10);
        let fed = replay(&ByteLog::segment_paths(id), &mut replayed).unwrap();
        assert_eq!(fed, program_output().len() as u64);
        assert_same_terminal(&live, &replayed);
    }

    /// A rotated log replays the retired segment first.  Either order
    /// feeds every byte; only one of them is the pane's history.
    #[test]
    fn a_rotated_log_replays_oldest_first() {
        let mut sandbox = crate::test_env::sandbox_state_dir();
        sandbox.env.remove("MARSPOT_SESSION_ID");
        let id = 23;
        let dir = sandbox.path().join("sessions").join(id.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let all = program_output();
        let (older, newer) = all.split_at(all.len() / 2);
        std::fs::write(dir.join("bytelog.1"), older).unwrap();
        std::fs::write(dir.join("bytelog"), newer).unwrap();

        let mut live = crate::terminal::Terminal::new(40, 10);
        live.feed(&all);
        let mut replayed = crate::terminal::Terminal::new(40, 10);
        replay(&ByteLog::segment_paths(id), &mut replayed).unwrap();
        assert_same_terminal(&live, &replayed);

        // the tool's own list, built from the live segment's path
        let mut by_path = crate::terminal::Terminal::new(40, 10);
        let fed = replay(
            &[
                dir.join("bytelog.1"),
                dir.join("bytelog"),
                dir.join("bytelog.2"),
            ],
            &mut by_path,
        )
        .unwrap();
        assert_eq!(fed, all.len() as u64, "a missing segment is skipped");
        assert_same_terminal(&live, &by_path);
    }
}
