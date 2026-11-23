//! Reading a file another program may put anything in the place of.

use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The first `max` bytes of the file at `path` and how many it has; None
/// when it is not a regular file.  Never through a link in its place,
/// never waiting for a pipe's other end.
pub(in crate::plugins) fn read(path: &Path, max: u64) -> Option<(Vec<u8>, u64)> {
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path).ok()?;
    let len = f.metadata().ok().filter(std::fs::Metadata::is_file)?.len();
    let mut bytes = Vec::new();
    f.take(max).read_to_end(&mut bytes).ok()?;
    Some((bytes, len))
}
