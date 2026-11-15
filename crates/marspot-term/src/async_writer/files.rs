//! What the writer thread does to its files.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The file at `live` renamed to `retired`, and a fresh one at `live`.
///
/// What `retired` was before is deleted behind: giving back a file of
/// 50 MiB took 6 to 46 ms, and a writer that waits for it writes
/// nothing meanwhile, so its producer fills the queue and waits too.
pub(super) fn rotated(live: &Path, retired: &Path) -> Option<std::fs::File> {
    delete_behind(retired);
    std::fs::rename(live, retired).ok()?;
    std::fs::OpenOptions::new().create(true).append(true).read(true).open(live).ok()
}

/// Who is told that a writer has room for a hand-over it refused.
static RETRY: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Have `wake` called, on a writer's thread, each time a writer that
/// refused a hand-over has written what kept it busy.  A process says
/// this once, with what makes its producers hand over again; in one
/// that never does, refused bytes wait for the next full buffer or
/// flush.
pub fn retry_with(wake: impl Fn() + Send + Sync + 'static) {
    let _ = RETRY.set(Box::new(wake));
}

pub(super) fn retry() {
    if let Some(wake) = RETRY.get() {
        wake();
    }
}

/// Who deletes what [`delete_behind`] has moved aside.
static DELETER: OnceLock<Box<dyn Fn(PathBuf) + Send + Sync>> = OnceLock::new();

/// Have `deleter` given every file [`delete_behind`] moves aside, to
/// delete on a thread nothing waits for.  A process says this once, if
/// it has such a thread; one that never does deletes where it stands.
pub fn delete_with(deleter: impl Fn(PathBuf) + Send + Sync + 'static) {
    let _ = DELETER.set(Box::new(deleter));
}

/// Delete the file at `path` without waiting for the system to take it
/// back, where the process has someone to do that: the file is renamed
/// at once, to [`doomed_path`], and handed over.
fn delete_behind(path: &Path) {
    let doomed = doomed_path(path);
    if std::fs::rename(path, &doomed).is_err() {
        return;
    }
    match DELETER.get() {
        Some(deleter) => deleter(doomed),
        None => {
            let _ = std::fs::remove_file(doomed);
        }
    }
}

/// Where a file waits to be deleted.
pub fn doomed_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Most buffers one write carries; more are written in turns.
const AT_ONCE: usize = 8;

/// Every one of `parts` to `file`, in order, in as few writes as the
/// kernel takes them in.
pub(super) fn write_all_of<'a>(file: &mut std::fs::File, parts: impl Iterator<Item = &'a [u8]>) {
    let mut held = [std::io::IoSlice::new(&[]); AT_ONCE];
    let mut n = 0;
    let mut flush = |held: &[std::io::IoSlice<'a>]| {
        let total: usize = held.iter().map(|p| p.len()).sum();
        if total == 0 {
            return;
        }
        // a short or failed vectored write leaves the rest to plain ones
        let mut written = file.write_vectored(held).unwrap_or(0);
        if written == total {
            return;
        }
        for part in held {
            let skip = written.min(part.len());
            written -= skip;
            let _ = file.write_all(&part[skip..]);
        }
    };
    for part in parts {
        held[n] = std::io::IoSlice::new(part);
        n += 1;
        if n == AT_ONCE {
            flush(&held[..n]);
            n = 0;
        }
    }
    flush(&held[..n]);
}
