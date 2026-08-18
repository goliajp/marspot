//! Per-session append-only byte log.
//!
//! Records every byte read from a session's PTY master.  Nothing in the
//! running system reads it back — reattach restores from `state.bin`
//! instead — so this is a write-only forensic record, kept for the
//! "the scrollback is gone but the raw bytes survived" case that
//! `examples/rebuild_scrollback_from_bytelog.rs` exists to handle.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::time::Instant;

    /// Point the state root at a scratch dir for one test.
    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("marspot-bytelog-test-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: nextest gives each test its own process, so no
        // other thread can be reading the environment concurrently.
        // Mirrors the `set_var("HOME")` pattern in `grid_links`.
        unsafe { std::env::set_var("MARSPOT_STATE_DIR", &dir) };
        dir
    }

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
        let dir = sandbox("rotate");
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
        let mut last = None;
        for i in 0..n_chunks {
            if let Some(at) = text.find(&format!("<{i}>")) {
                if let Some(prev) = last {
                    assert!(at > prev, "chunk {i} out of order across the seam");
                }
                last = Some(at);
            }
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
        assert!(
            trigger.as_millis() < 100,
            "the rotating append took {trigger:?} — data is being copied, not renamed"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A legacy oversized log (written by the copy-compaction scheme,
    /// which allowed a single file up to the full ceiling) is retired
    /// on open instead of being grown further.
    #[test]
    fn legacy_oversized_segment_rotates_on_open() {
        let dir = sandbox("legacy");
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
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A log that has never rotated still reads back as one segment.
    #[test]
    fn single_segment_before_rotation() {
        let dir = sandbox("single");
        let id = 9;
        let mut log = ByteLog::open(id).unwrap();
        log.append(b"hello ").unwrap();
        log.append(b"world").unwrap();
        log.flush();
        assert_eq!(ByteLog::segment_paths(id).len(), 1);
        assert_eq!(read_all(id), b"hello world");
        let _ = std::fs::remove_dir_all(dir);
    }
}
