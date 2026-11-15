//! Fixed-size files that a history is written into in place.
//!
//! A pane's history used to be files that grew until they were full,
//! were renamed aside, and were deleted two generations later.  Every
//! one of those calls -- a write that gives a file more blocks, a
//! rename, an unlink of a file with data in it -- waits whenever the
//! volume commits a batch of such changes, and a flood that deleted a
//! full file every few hundred milliseconds made the volume commit
//! about once a second, with each write behind it waiting ~10 ms.  A
//! write into blocks a file already has does not wait for that.
//!
//! So a history is a few segments: files whose size is set, and whose
//! blocks are given, when they are made -- on a thread nobody waits
//! for -- and which are then only ever written in place.  When the
//! history has used up its budget, the oldest full segment is used
//! again from its start, behind a new header.
//!
//! Each segment starts with a 64-byte header.  Its use number is what
//! tells the bytes of this use from the ones an earlier use left:
//! every entry a use writes carries it, and the history of a segment is
//! the run of entries from its start that carry its header's number.
//! Nothing is truncated, so nothing has to be.

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

pub(crate) const HEADER_BYTES: u64 = 64;
const MAGIC: u32 = 0x5350_5305;
const VERSION: u32 = 5;
/// Where the header states how far a file whose writes say so has been
/// written: outside what the checksum covers, as it changes with every
/// write.
pub(crate) const END_AT: u64 = 56;

/// Which history a segment belongs to: a file of one kind is never read
/// as the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Scrollback = 1,
    Bytelog = 2,
}

/// What a segment's header says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Header {
    pub(crate) kind: Kind,
    /// Width of a cell in the records, for a scrollback; zero otherwise.
    pub(crate) cell_abi: u32,
    /// Bytes of data after the header.
    pub(crate) data: u64,
    /// Bytes of index after the data.
    pub(crate) index: u64,
    /// Which use of the segment this is: across the files of one
    /// history, a larger number is a newer use.  Zero for a segment
    /// made ready and never used.
    pub(crate) serial: u64,
    /// For a bytelog, where in the whole stream its first byte is.
    pub(crate) base: u64,
}

impl Header {
    pub(crate) fn encode(&self) -> [u8; HEADER_BYTES as usize] {
        let mut h = [0u8; HEADER_BYTES as usize];
        h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        h[4..8].copy_from_slice(&VERSION.to_le_bytes());
        h[8..12].copy_from_slice(&(self.kind as u32).to_le_bytes());
        h[12..16].copy_from_slice(&self.cell_abi.to_le_bytes());
        h[16..24].copy_from_slice(&self.data.to_le_bytes());
        h[24..32].copy_from_slice(&self.index.to_le_bytes());
        h[32..40].copy_from_slice(&self.serial.to_le_bytes());
        h[40..48].copy_from_slice(&self.base.to_le_bytes());
        let sum = checksum(&h[..48]);
        h[48..56].copy_from_slice(&sum.to_le_bytes());
        h[56..64].copy_from_slice(&HEADER_BYTES.to_le_bytes());
        h
    }

    /// The header `h` states, or `None` for bytes that are not a whole
    /// header of `kind` this build can read.
    pub(crate) fn decode(h: &[u8; HEADER_BYTES as usize], kind: Kind, cell_abi: u32) -> Option<Self> {
        let word = |at: usize| u32::from_le_bytes(h[at..at + 4].try_into().expect("four bytes"));
        let long = |at: usize| u64::from_le_bytes(h[at..at + 8].try_into().expect("eight bytes"));
        if word(0) != MAGIC || word(4) != VERSION || word(8) != kind as u32 || word(12) != cell_abi {
            return None;
        }
        if long(48) != checksum(&h[..48]) {
            return None;
        }
        let hdr = Self { kind, cell_abi, data: long(16), index: long(24), serial: long(32), base: long(40) };
        (hdr.data > 0).then_some(hdr)
    }

    /// Bytes the file takes.
    pub(crate) fn file_bytes(&self) -> u64 {
        HEADER_BYTES + self.data + self.index
    }

    /// Where the index starts.
    pub(crate) fn index_at(&self) -> u64 {
        HEADER_BYTES + self.data
    }

    /// This header for the next use of its segment.
    pub(crate) fn used(self, serial: u64, base: u64) -> Self {
        Self { serial, base, ..self }
    }
}

/// FNV-1a: a header torn by a kill is told from a whole one.
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// One segment file, open for reading and writing.
pub(crate) struct Seg {
    pub(crate) slot: u32,
    pub(crate) path: PathBuf,
    pub(crate) file: Arc<std::fs::File>,
    pub(crate) hdr: Header,
}

/// What a segment of the history named `stem` in its slot is called.
pub(crate) fn path(dir: &Path, stem: &str, slot: u32) -> PathBuf {
    dir.join(format!("{stem}.seg.{slot}"))
}

fn slot_of(stem: &str, name: &str) -> Option<u32> {
    name.strip_prefix(stem)?.strip_prefix(".seg.")?.parse().ok()
}

/// The segments of a history as they are on disk.
#[derive(Default)]
pub(crate) struct Listing {
    /// Segments in use, oldest use first.
    pub(crate) used: Vec<Seg>,
    /// Segments made ready and never used.
    pub(crate) spare: Vec<Seg>,
    /// Files named as segments that are not whole ones: made by a
    /// process that died before it had written their header.
    pub(crate) bad: Vec<PathBuf>,
}

/// Every segment of the history named `stem` in `dir`.
pub(crate) fn list(dir: &Path, stem: &str, kind: Kind, cell_abi: u32) -> std::io::Result<Listing> {
    list_with(dir, stem, kind, cell_abi, true)
}

/// [`list`], opened for reading only: by a process other than the
/// writer's, which must change nothing.
pub(crate) fn list_for_reading(dir: &Path, stem: &str, kind: Kind, cell_abi: u32) -> std::io::Result<Listing> {
    list_with(dir, stem, kind, cell_abi, false)
}

fn list_with(dir: &Path, stem: &str, kind: Kind, cell_abi: u32, writable: bool) -> std::io::Result<Listing> {
    let mut out = Listing::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(slot) = name.to_str().and_then(|n| slot_of(stem, n)) else { continue };
        let path = entry.path();
        let file = match std::fs::OpenOptions::new().read(true).write(writable).open(&path) {
            Ok(f) => f,
            // deleted since the directory was read
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        let mut h = [0u8; HEADER_BYTES as usize];
        let hdr = file.read_exact_at(&mut h, 0).ok().and_then(|()| Header::decode(&h, kind, cell_abi));
        let fits = |hdr: &Header| file.metadata().is_ok_and(|m| m.len() >= hdr.file_bytes());
        match hdr {
            Some(hdr) if fits(&hdr) => {
                let seg = Seg { slot, path, file: Arc::new(file), hdr };
                if hdr.serial == 0 { out.spare.push(seg) } else { out.used.push(seg) }
            }
            _ => out.bad.push(path),
        }
    }
    by_key(&mut out.used, |s| s.hdr.serial);
    by_key(&mut out.spare, |s| u64::from(s.slot));
    Ok(out)
}

/// Sort the handful of segments a history has: an insertion sort, as
/// the library's sorts cost kilobytes of code for each type they sort.
fn by_key(segs: &mut [Seg], key: impl Fn(&Seg) -> u64) {
    for i in 1..segs.len() {
        let mut j = i;
        while j > 0 && key(&segs[j - 1]) > key(&segs[j]) {
            segs.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// What making a segment ready does, in order.  A process can die after
/// any of them; the next open deletes what it finds before the last.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Step {
    Created,
    Allocated,
    Sized,
    HeaderWritten,
}

pub(crate) const STEPS: [Step; 4] = [Step::Created, Step::Allocated, Step::Sized, Step::HeaderWritten];

/// A segment to make ready.
pub(crate) struct Prep {
    pub(crate) path: PathBuf,
    pub(crate) slot: u32,
    pub(crate) hdr: Header,
    /// How many of [`STEPS`] to do: all of them, outside a test.
    pub(crate) upto: usize,
    /// Waited on before anything is done, by a test that holds the
    /// preparation up.
    #[cfg(test)]
    pub(crate) gate: Option<std::sync::mpsc::Receiver<()>>,
}

impl Prep {
    pub(crate) fn new(dir: &Path, stem: &str, slot: u32, hdr: Header) -> Self {
        Self {
            path: path(dir, stem, slot),
            slot,
            hdr,
            upto: STEPS.len(),
            #[cfg(test)]
            gate: None,
        }
    }

    /// Make the segment: created, its blocks given, its size set and an
    /// unused header written.  A segment that cannot be given its blocks
    /// -- a disk that is full -- is deleted again and reported.
    pub(crate) fn run(self) -> std::io::Result<Seg> {
        #[cfg(test)]
        if let Some(gate) = &self.gate {
            let _ = gate.recv();
        }
        let hdr = self.hdr.used(0, 0);
        let done = |step: Step| STEPS.iter().position(|s| *s == step).is_some_and(|i| i + 1 >= self.upto);
        let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&self.path)?;
        if done(Step::Created) {
            return Err(std::io::Error::other("stopped after creating"));
        }
        let made = (|| {
            allocate(&file, hdr.file_bytes())?;
            if done(Step::Allocated) {
                return Err(std::io::Error::other("stopped after allocating"));
            }
            file.set_len(hdr.file_bytes())?;
            if done(Step::Sized) {
                return Err(std::io::Error::other("stopped after sizing"));
            }
            file.write_all_at(&hdr.encode(), 0)
        })();
        if let Err(e) = made {
            // a test that stops part way leaves the file as a dead
            // process would
            if self.upto == STEPS.len() {
                drop(file);
                let _ = std::fs::remove_file(&self.path);
            }
            return Err(e);
        }
        Ok(Seg { slot: self.slot, path: self.path, file: Arc::new(file), hdr })
    }
}

/// Give `file` its blocks for `len` bytes, so that writing into them
/// later asks the volume for nothing.
#[cfg(target_os = "macos")]
fn allocate(file: &std::fs::File, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let mut store = libc::fstore_t {
        fst_flags: libc::F_ALLOCATECONTIG | libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: len as libc::off_t,
        fst_bytesalloc: 0,
    };
    // SAFETY: a valid descriptor and a valid `fstore_t`.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) } == 0 {
        return Ok(());
    }
    // contiguous was too much to ask: any blocks will do
    store.fst_flags = libc::F_ALLOCATEALL;
    // SAFETY: as above.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PREALLOCATE, &mut store) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(not(target_os = "macos"))]
fn allocate(file: &std::fs::File, len: u64) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: a valid descriptor.
    match unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len as libc::off_t) } {
        0 => Ok(()),
        e => Err(std::io::Error::from_raw_os_error(e)),
    }
}

/// A segment being made ready on another thread, and whoever waits for
/// it.
#[derive(Default)]
pub(crate) struct Pending {
    made: Mutex<Option<std::io::Result<Seg>>>,
    changed: Condvar,
}

impl Pending {
    /// Have `prep` run where nobody waits for it.
    pub(crate) fn start(prep: Prep) -> Arc<Self> {
        let pending = Arc::new(Self::default());
        let to = Arc::clone(&pending);
        crate::async_writer::behind(Box::new(move || {
            let made = prep.run();
            *to.made.lock().unwrap_or_else(|p| p.into_inner()) = Some(made);
            to.changed.notify_all();
        }));
        pending
    }

    /// Whether the preparation is done.
    #[cfg(test)]
    pub(crate) fn is_made(&self) -> bool {
        self.made.lock().unwrap_or_else(|p| p.into_inner()).is_some()
    }

    /// The segment, if it is ready.
    pub(crate) fn take(&self) -> Option<std::io::Result<Seg>> {
        self.made.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    /// The segment, once it is ready.
    pub(crate) fn wait(&self) -> std::io::Result<Seg> {
        let mut made = self.made.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(m) = made.take() {
                return m;
            }
            made = self.changed.wait(made).unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// The size after `data`, growing to `full`: twice as large each time.
pub(crate) fn grown(data: u64, full: u64) -> u64 {
    data.saturating_mul(2).min(full)
}

/// How much of a segment of `data` bytes is left when the next one is
/// asked for: half of it, and never more than what a flood writes while
/// a segment is made ready, with room for the slowest seen (24 MiB at
/// 0.83 MiB a millisecond is 29 ms; the slowest of 304 was 17 ms).
pub(crate) fn ask_ahead(data: u64) -> u64 {
    (data / 2).min(24 << 20)
}

/// Delete `paths` and let go of `held` where nobody waits for it.
pub(crate) fn delete_behind(paths: Vec<PathBuf>, held: impl Send + 'static) {
    crate::async_writer::behind(Box::new(move || {
        drop(held);
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr() -> Header {
        Header { kind: Kind::Scrollback, cell_abi: 20, data: 4096, index: 512, serial: 7, base: 0 }
    }

    /// A header reads back as written, and any byte of it changed --
    /// a torn write -- makes it not a header at all.
    #[test]
    fn a_header_reads_back_and_a_torn_one_does_not() {
        let h = hdr().encode();
        assert_eq!(Header::decode(&h, Kind::Scrollback, 20), Some(hdr()));
        assert_eq!(Header::decode(&h, Kind::Bytelog, 20), None, "another kind");
        assert_eq!(Header::decode(&h, Kind::Scrollback, 13), None, "another cell width");
        for at in 0..48 {
            let mut torn = h;
            torn[at] ^= 0x10;
            assert_eq!(Header::decode(&torn, Kind::Scrollback, 20), None, "byte {at}");
        }
        let mut end = h;
        end[END_AT as usize] = 0x99;
        assert_eq!(Header::decode(&end, Kind::Scrollback, 20), Some(hdr()), "the written end is not checked");
    }

    #[test]
    fn a_segment_name_says_its_slot_and_history() {
        assert_eq!(slot_of("scrollback", "scrollback.seg.12"), Some(12));
        assert_eq!(slot_of("scrollback", "bytelog.seg.1"), None);
        assert_eq!(slot_of("scrollback", "scrollback.bin"), None);
    }
}
