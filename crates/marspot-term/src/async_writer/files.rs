//! What the writer thread does to its files.

use std::io::Write;

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
