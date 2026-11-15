//! Per-session byte log.
//!
//! Records every byte read from a session's PTY master, for the "the
//! scrollback is gone but the raw bytes survived" case that [`replay`]
//! (and `examples/rebuild_scrollback_from_bytelog.rs`, the tool around
//! it) exists to handle.  It is also read while the pane runs: how much
//! has been logged is how L1 tells a pane that is talking from one that
//! is silent and how a scripted op sees output arrive ([`stream_len`]),
//! and the last bytes logged are what a screen is rebuilt from when no
//! frame is published ([`tail`]).
//!
//! Kept in segments (`crate::segment`), `bytelog.seg.<slot>` in the
//! session's directory: 1 MiB, then twice as large each time up to 50
//! MiB, made ready on a thread nobody waits for; once two of 50 MiB are
//! in use the smaller ones are deleted and the older full one is used
//! again whenever the newer is full -- 50 to 100 MiB of the newest
//! bytes, as the rotating pair kept.  No byte written to it ever waits
//! for a file to be created, renamed, deleted or grown.
//!
//! A segment's header says where in the whole stream its first byte is,
//! and, after every write, how far it has been written (`segment::END_AT`,
//! stated by the writer thread once the bytes are in).  A segment used
//! again starts over behind a new header whose written end is its start,
//! so what an earlier use left there is never read.
//!
//! The files of the rotating pair (`bytelog.1`, then `bytelog`) are read
//! where they are, as the oldest bytes, and deleted once two full
//! segments are in use.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::async_writer::{AsyncWriter, Files, Target};
use crate::paths::sessions_dir;
use crate::segment::{self, HEADER_BYTES, Header, Kind, Pending, Seg};

mod read;
pub use read::{Piece, pieces, replay, stream_len, tail};

/// Bytes of a full segment.  Two of them are the budget.
pub const BYTELOG_SEGMENT_BYTES: u64 = 50 * 1024 * 1024;

/// Total on-disk ceiling per session.
pub const BYTELOG_CAP_BYTES: u64 = 2 * BYTELOG_SEGMENT_BYTES;

/// Bytes of a pane's first segment.
const FIRST_SEGMENT_BYTES: u64 = 1024 * 1024;

/// Handoff size to the writer thread.  64 KiB matches the pty reader's
/// own buffer, so a busy pane hands over roughly one gathered batch at
/// a time; an idle one hands over on flush.
const BYTELOG_WRITE_BUF: usize = 64 * 1024;

const STEM: &str = "bytelog";

pub struct ByteLog {
    dir: PathBuf,
    /// Appends go to a writer thread (see `async_writer`): the thread
    /// that parses a pane's output does not wait for its disk.
    file: AsyncWriter,
    /// Files of the rotating pair, oldest first, until the budget passes
    /// them.
    legacy: Vec<PathBuf>,
    /// Segments in use, oldest first; the last is written.
    segs: Vec<Seg>,
    /// Where the next byte goes in the last segment, and where its data
    /// ends.
    hot_pos: u64,
    hot_end: u64,
    /// Past this the next append asks for the next segment.
    mark: u64,
    /// Bytes logged in this process.
    bytes_written: u64,
    next_serial: u64,
    next_slot: u32,
    full: u64,
    spares: Vec<Seg>,
    pending: Option<Arc<Pending>>,
}

fn header(data: u64) -> Header {
    Header { kind: Kind::Bytelog, cell_abi: 0, data, index: 0, serial: 0, base: 0 }
}

/// Where segment `seg`'s written bytes end, as its header states it.
pub(crate) fn written_end(seg: &Seg) -> u64 {
    use std::os::unix::fs::FileExt;
    let mut b = [0u8; 8];
    let end = seg.file.read_exact_at(&mut b, segment::END_AT).map_or(HEADER_BYTES, |()| u64::from_le_bytes(b));
    end.clamp(HEADER_BYTES, seg.hdr.file_bytes())
}

/// The files of the rotating pair in `dir`, oldest first.
fn legacy_paths(dir: &Path) -> [PathBuf; 2] {
    [dir.join("bytelog.1"), dir.join("bytelog")]
}

impl ByteLog {
    pub fn open(session_id: u64) -> io::Result<Self> {
        Self::open_in(&sessions_dir().join(session_id.to_string()), FIRST_SEGMENT_BYTES, BYTELOG_SEGMENT_BYTES)
    }

    fn open_in(dir: &Path, first: u64, full: u64) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // a segment the rotating pair's last process was still deleting
        let _ = std::fs::remove_file(dir.join("bytelog.1.old"));
        let listing = segment::list(dir, STEM, Kind::Bytelog, 0)?;
        for bad in &listing.bad {
            let _ = std::fs::remove_file(bad);
        }
        let mut next_slot = listing.used.iter().chain(&listing.spare).map(|s| s.slot + 1).max().unwrap_or(0);
        let mut next_serial = listing.used.last().map_or(1, |s| s.hdr.serial + 1);
        let mut segs = listing.used;
        let full_at: Vec<usize> = segs.iter().enumerate().filter(|(_, s)| s.hdr.data == full).map(|(i, _)| i).collect();
        let mut legacy: Vec<PathBuf> = legacy_paths(dir).into_iter().filter(|p| p.exists()).collect();
        if full_at.len() >= 2 {
            for old in segs.drain(..full_at[full_at.len() - 2]) {
                let _ = std::fs::remove_file(&old.path);
            }
            for p in legacy.drain(..) {
                let _ = std::fs::remove_file(p);
            }
        }
        let mut spares = listing.spare;
        if segs.is_empty() {
            let seg = match spares.iter().position(|s| s.hdr.data == first) {
                Some(i) => spares.remove(i),
                None => {
                    next_slot += 1;
                    segment::Prep::new(dir, STEM, next_slot - 1, header(first)).run()?
                }
            };
            let base = legacy.iter().filter_map(|p| p.metadata().ok()).map(|m| m.len()).sum();
            let hdr = seg.hdr.used(next_serial, base);
            std::os::unix::fs::FileExt::write_all_at(&*seg.file, &hdr.encode(), 0)?;
            next_serial += 1;
            segs.push(Seg { hdr, ..seg });
        }
        let hot = segs.last().expect("a segment to write");
        let hot_pos = written_end(hot);
        let mut main = Target::at(Arc::clone(&hot.file), hot_pos);
        main.publish = Some(segment::END_AT);
        let hot_end = hot.hdr.index_at();
        let mut log = Self {
            dir: dir.to_path_buf(),
            file: AsyncWriter::to(Files { main, side: None }, BYTELOG_WRITE_BUF, 3),
            legacy,
            segs,
            hot_pos,
            hot_end,
            mark: 0,
            bytes_written: 0,
            next_serial,
            next_slot,
            full,
            spares: Vec::new(),
            pending: None,
        };
        let wanted = log.next_size();
        for s in spares {
            if Some(s.hdr.data) == wanted && log.spares.is_empty() {
                log.spares.push(s);
            } else {
                let _ = std::fs::remove_file(&s.path);
            }
        }
        log.set_mark();
        Ok(log)
    }

    pub fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        self.bytes_written += bytes.len() as u64;
        while self.hot_pos + bytes.len() as u64 > self.mark {
            let room = (self.hot_end - self.hot_pos) as usize;
            self.at_mark();
            if bytes.len() <= room {
                break;
            }
            let (now, later) = bytes.split_at(room);
            self.file.write(now);
            self.hot_pos += room as u64;
            bytes = later;
            self.change();
        }
        self.file.write(bytes);
        self.hot_pos += bytes.len() as u64;
        Ok(())
    }

    /// The data bytes of the segment the next change takes, or `None`
    /// when it uses the older full one again.
    fn next_size(&self) -> Option<u64> {
        let hot = self.segs.last().expect("a segment to write").hdr.data;
        if hot < self.full {
            return Some(segment::grown(hot, self.full));
        }
        (self.segs.iter().filter(|s| s.hdr.data == self.full).count() < 2).then_some(self.full)
    }

    fn set_mark(&mut self) {
        let ask = self.pending.is_none()
            && self.next_size().is_some_and(|data| !self.spares.iter().any(|s| s.hdr.data == data));
        let data = self.hot_end - HEADER_BYTES;
        self.mark = if ask { self.hot_end - segment::ask_ahead(data) } else { self.hot_end };
    }

    #[cold]
    fn at_mark(&mut self) {
        if self.pending.is_none()
            && let Some(data) = self.next_size()
            && !self.spares.iter().any(|s| s.hdr.data == data)
        {
            self.pending = Some(Pending::start(self.prep(data)));
        }
        self.set_mark();
    }

    fn prep(&mut self, data: u64) -> segment::Prep {
        let slot = self.next_slot;
        self.next_slot += 1;
        segment::Prep::new(&self.dir, STEM, slot, header(data))
    }

    /// Go on in the next segment, waiting for it if it is not ready: the
    /// bytes inside the budget are not given up for it.  A segment that
    /// could not be made (a full disk) leaves the log going round in the
    /// segments it has.
    fn change(&mut self) {
        let hot = self.segs.last().expect("a segment to write").hdr;
        let stream_now = hot.base + (self.hot_pos - HEADER_BYTES);
        let next = match self.next_size() {
            None => Ok(None),
            Some(data) => match self.spares.iter().position(|s| s.hdr.data == data) {
                Some(i) => Ok(Some(self.spares.remove(i))),
                None => {
                    let pending = self.pending.take().unwrap_or_else(|| Pending::start(self.prep(data)));
                    pending.take().unwrap_or_else(|| pending.wait()).map(Some)
                }
            },
        };
        let seg = match next {
            Ok(Some(seg)) => seg,
            Ok(None) => {
                let i = self.segs.iter().position(|s| s.hdr.data == self.full).expect("two full segments");
                self.segs.remove(i)
            }
            Err(e) => {
                crate::lx_warn!("bytelog.segment.not_made", &format!("{e}"), dir = self.dir.display());
                self.segs.remove(0)
            }
        };
        let hdr = seg.hdr.used(self.next_serial, stream_now);
        self.next_serial += 1;
        let file = Arc::clone(&seg.file);
        self.file.cut(move |files| {
            let _ = std::os::unix::fs::FileExt::write_all_at(&*file, &hdr.encode(), 0);
            let mut main = Target::at(file, HEADER_BYTES);
            main.publish = Some(segment::END_AT);
            files.main = main;
        });
        self.segs.push(Seg { hdr, ..seg });
        self.hot_pos = HEADER_BYTES;
        self.hot_end = hdr.index_at();
        self.evict();
        self.set_mark();
    }

    /// Once two full segments are in use, let go of everything older
    /// than the older of them.
    fn evict(&mut self) {
        let full = self.full;
        if self.segs.iter().filter(|s| s.hdr.data == full).count() < 2 {
            return;
        }
        let keep_from = self.segs.iter().position(|s| s.hdr.data == full).expect("two full segments");
        let gone: Vec<Seg> = self.segs.drain(..keep_from).collect();
        let wanted = self.next_size();
        let (keep, spare_gone): (Vec<Seg>, Vec<Seg>) =
            std::mem::take(&mut self.spares).into_iter().partition(|s| Some(s.hdr.data) == wanted);
        self.spares = keep;
        let mut paths: Vec<PathBuf> = std::mem::take(&mut self.legacy);
        paths.extend(gone.iter().chain(&spare_gone).map(|s| s.path.clone()));
        if !paths.is_empty() {
            segment::delete_behind(paths, (gone, spare_gone));
        }
    }

    /// End of a burst: let the file catch up with what was appended.
    /// See [`AsyncWriter::hand_off`](crate::async_writer::AsyncWriter::hand_off).
    pub fn hand_off(&mut self) {
        self.file.hand_off();
    }

    /// Make every appended byte visible to a reader of the segments.
    /// Callers that are about to read the log back (replay after a
    /// silent update) must call this first — the writer thread is not
    /// otherwise synchronised with them.
    pub fn flush(&mut self) {
        self.file.flush();
    }

    /// Bytes appended by this process.
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// The session directory the log is kept in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests;
