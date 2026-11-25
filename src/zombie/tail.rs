//! Reading a growing file by the lines added since the last read.
//!
//! A transcript is append-only and can be hundreds of megabytes; each
//! check reads only what was added.  The first read, and any read after
//! the file was cut shorter or replaced (another inode), starts from the
//! last [`FIRST`] bytes and drops the partial line there.  More than
//! [`MAX_STEP`] added between two reads is treated the same way: the
//! old state is no longer worth folding forward byte by byte.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

pub const FIRST: u64 = 64 * 1024;
pub const MAX_STEP: u64 = 8 << 20;
/// A line longer than this is dropped rather than held: no record the
/// rules read is anywhere near it.
const MAX_LINE: usize = 4 << 20;

#[derive(Debug)]
pub struct Tail {
    path: PathBuf,
    ino: u64,
    pos: u64,
    carry: Vec<u8>,
    /// Skipping the rest of a line whose start was not read.
    skipping: bool,
    /// Bytes read so far.
    pub read_bytes: u64,
}

/// What a read hands over, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line<'a> {
    /// The read starts again near the end, after a first read, a cut, a
    /// new file or a big jump: whatever was folded from earlier lines
    /// is stale.  Comes before any line of that read.
    Restart,
    Text(&'a str),
}

impl Tail {
    pub fn new(path: impl Into<PathBuf>) -> Tail {
        Tail { path: path.into(), ino: 0, pos: 0, carry: Vec::new(), skipping: false, read_bytes: 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Hand every complete line added since the last read to `line`.
    pub fn read(&mut self, mut line: impl FnMut(Line)) -> io::Result<()> {
        let mut f = File::open(&self.path)?;
        let meta = f.metadata()?;
        let len = meta.len();
        let fresh = self.ino == 0 || meta.ino() != self.ino || len < self.pos || len - self.pos > MAX_STEP;
        if fresh {
            self.ino = meta.ino();
            self.pos = len.saturating_sub(FIRST);
            self.carry.clear();
            // the first line is whole only when the read starts at the top
            self.skipping = self.pos > 0;
            line(Line::Restart);
        }
        if len == self.pos {
            return Ok(());
        }
        f.seek(SeekFrom::Start(self.pos))?;
        let mut buf = Vec::with_capacity((len - self.pos) as usize);
        (&mut f).take(len - self.pos).read_to_end(&mut buf)?;
        self.pos += buf.len() as u64;
        self.read_bytes += buf.len() as u64;
        let mut rest = &buf[..];
        while let Some(nl) = rest.iter().position(|b| *b == b'\n') {
            let (head, tail) = rest.split_at(nl);
            rest = &tail[1..];
            if self.skipping {
                self.skipping = false;
                self.carry.clear();
                continue;
            }
            if self.carry.is_empty() {
                line(Line::Text(&String::from_utf8_lossy(head)));
            } else {
                self.carry.extend_from_slice(head);
                line(Line::Text(&String::from_utf8_lossy(&self.carry)));
                self.carry.clear();
            }
        }
        if !self.skipping {
            self.carry.extend_from_slice(rest);
            if self.carry.len() > MAX_LINE {
                self.carry.clear();
                self.skipping = true;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
