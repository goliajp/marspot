//! Reading a log back: from another process, while it is written.
//!
//! Nothing here writes or locks.  A segment the writer uses again while
//! it is read holds other bytes at the same places; a read that sees its
//! header's use number change across it is not used.

use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;

use super::{STEM, legacy_paths, written_end};
use crate::segment::{self, HEADER_BYTES, Kind};

/// A stretch of the log in one file: bytes `from..to`, and for a
/// segment the use those bytes were written under.
pub struct Piece {
    file: std::fs::File,
    from: u64,
    to: u64,
    serial: Option<u64>,
}

impl Piece {
    /// Bytes this piece holds.
    pub fn len(&self) -> u64 {
        self.to - self.from
    }

    pub fn is_empty(&self) -> bool {
        self.to == self.from
    }

    /// Whether the segment is still in the use this piece was read
    /// under.
    fn same_use(&self) -> bool {
        let Some(serial) = self.serial else { return true };
        let mut b = [0u8; 8];
        self.file.read_exact_at(&mut b, 32).is_ok() && u64::from_le_bytes(b) == serial
    }

    /// Bytes `at..at + buf.len()` of the piece, counted from its start.
    fn read(&self, at: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, self.from + at)?;
        if self.same_use() {
            Ok(())
        } else {
            Err(io::Error::other("the segment was used again while it was read"))
        }
    }
}

/// The log of the session directory `dir`, oldest bytes first: the
/// rotating pair's files, then the segments in the order of their use.
/// Concatenated, the pieces are the retained byte stream.
pub fn pieces(dir: &Path) -> io::Result<Vec<Piece>> {
    // Newest first, each taken only where the stream it holds ends
    // where the next one's begins: a segment the writer has let go of
    // and not yet deleted holds older bytes that no longer join on.
    let mut segs = Vec::new();
    let mut joins_at: Option<u64> = None;
    for seg in segment::list_for_reading(dir, STEM, Kind::Bytelog, 0)?.used.into_iter().rev() {
        let to = written_end(&seg);
        let end = seg.hdr.base + (to - HEADER_BYTES);
        if joins_at.is_some_and(|at| at != end) {
            break;
        }
        joins_at = Some(seg.hdr.base);
        let file = std::sync::Arc::try_unwrap(seg.file).map_err(|_| io::Error::other("a segment shared"))?;
        segs.push(Piece { file, from: HEADER_BYTES, to, serial: Some(seg.hdr.serial) });
    }
    let mut out = Vec::new();
    for p in legacy_paths(dir) {
        match std::fs::File::open(&p) {
            Ok(file) => {
                let to = file.metadata()?.len();
                out.push(Piece { file, from: 0, to, serial: None });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    // the rotating pair's files are the bytes before the first segment
    let legacy_len: u64 = out.iter().map(Piece::len).sum();
    if joins_at.is_some_and(|at| at != legacy_len) {
        out.clear();
    }
    out.extend(segs.into_iter().rev());
    Ok(out)
}

/// How many bytes the session in `dir` has logged: where the newest
/// segment's written end is in the whole stream.  Grows with every byte
/// the pane says, and only then.  `NotFound` for a session with no log.
pub fn stream_len(dir: &Path) -> io::Result<u64> {
    let listing = segment::list_for_reading(dir, STEM, Kind::Bytelog, 0)?;
    if let Some(seg) = listing.used.last() {
        return Ok(seg.hdr.base + (written_end(seg) - HEADER_BYTES));
    }
    let mut total = None;
    for p in legacy_paths(dir) {
        match p.metadata() {
            Ok(m) => *total.get_or_insert(0) += m.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    total.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
}

/// The last `n` bytes the session in `dir` logged, or all of them when
/// it has logged fewer.
pub fn tail(dir: &Path, n: u64) -> io::Result<Vec<u8>> {
    // a segment used again in the middle of the read is read again
    let mut last = Err(io::Error::from(io::ErrorKind::NotFound));
    for _ in 0..3 {
        last = tail_once(&pieces(dir)?, n);
        if last.is_ok() {
            break;
        }
    }
    last
}

fn tail_once(pieces: &[Piece], n: u64) -> io::Result<Vec<u8>> {
    if pieces.is_empty() {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }
    let total: u64 = pieces.iter().map(Piece::len).sum();
    let mut skip = total.saturating_sub(n);
    let mut out = Vec::with_capacity((total - skip) as usize);
    for p in pieces {
        if skip >= p.len() {
            skip -= p.len();
            continue;
        }
        let start = out.len();
        out.resize(start + (p.len() - skip) as usize, 0);
        p.read(skip, &mut out[start..])?;
        skip = 0;
    }
    Ok(out)
}

/// Feed the log's pieces, oldest first, through `term`'s parser: the
/// same bytes in the same order the pane's L3 parsed them, so the grid
/// and scrollback come out as they were, at `term`'s size.  Returns the
/// number of bytes fed.  Callers reading a live log flush it first.
pub fn replay(pieces: &[Piece], term: &mut crate::terminal::Terminal) -> io::Result<u64> {
    let mut buf = vec![0u8; 1 << 20];
    let mut fed = 0u64;
    for p in pieces {
        let mut at = 0;
        while at < p.len() {
            let n = (p.len() - at).min(buf.len() as u64) as usize;
            p.read(at, &mut buf[..n])?;
            term.feed(&buf[..n]);
            at += n as u64;
            fed += n as u64;
        }
    }
    Ok(fed)
}
