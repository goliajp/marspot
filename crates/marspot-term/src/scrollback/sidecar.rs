//! Files beside `scrollback.bin` holding things derived from its lines.
//!
//! The scrollback record format cannot carry per-line extras: its one
//! spare byte is `wrapped`, whose readers decide on `body[0] != 0`, so
//! a bit borrowed there reads as a wrapped line to any binary that
//! predates the change -- silently, and only when reflowing. Adding a
//! field instead means a `FILE_VERSION` bump, which a rollback turns
//! into a quarantined pair. So the extras live in their own files.
//!
//! What is shared between them is not the record layout -- command
//! marks are two fixed bytes a line, cluster text and link targets are
//! neither fixed nor one per line -- but the rule that makes any of
//! them safe to read:
//!
//! **A sidecar states the epoch of the `.bin` it was built against,
//! and a sidecar whose epoch does not match is discarded whole.**
//!
//! That is what makes the lifecycle forgiving rather than exacting.
//! Reflow, quarantine, a crash-truncated tail and a fresh open all
//! produce a `.bin` with a new epoch, so a stale sidecar is refused
//! without anything having to remember to delete it. Forgetting a
//! cleanup costs a rebuild, not a wrong answer -- and a mark on the
//! wrong line is worse than no mark, because it is exactly what a
//! jump lands on.
//!
//! These files are caches. Every value in them is derived from bytes
//! the session already keeps: delete the lot and a bytelog replay
//! rebuilds them. Nothing here is the only copy of anything.

const MAGIC: u32 = 0x5350_5344; // "SPSD"
const VERSION: u32 = 1;
/// magic + version + kind + reserved + epoch.
pub(super) const HEADER_BYTES: u64 = 24;

/// Which sidecar this is, so a `.marks` file opened as `.links` is
/// refused rather than decoded as the wrong shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Marks = 1,
}

/// Open the sidecar at `path` for the `.bin` whose epoch is `epoch`,
/// resetting it to an empty, correctly-stamped file when what is there
/// belongs to anything else.
///
/// Returns the file positioned by nothing in particular: callers
/// address it by offset.
pub(super) fn open_checked(
    path: &std::path::Path,
    epoch: u64,
    kind: Kind,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let mut hdr = [0u8; HEADER_BYTES as usize];
    let usable = file.read_exact_at(&mut hdr, 0).is_ok()
        && u32::from_le_bytes(hdr[0..4].try_into().unwrap()) == MAGIC
        && u32::from_le_bytes(hdr[4..8].try_into().unwrap()) == VERSION
        && u32::from_le_bytes(hdr[8..12].try_into().unwrap()) == kind as u32
        && u64::from_le_bytes(hdr[16..24].try_into().unwrap()) == epoch;
    if usable {
        return Ok(file);
    }
    // Anything else -- absent, truncated, another kind, another epoch
    // -- is not this generation's and is replaced rather than read.
    file.set_len(0)?;
    let mut fresh = [0u8; HEADER_BYTES as usize];
    fresh[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    fresh[4..8].copy_from_slice(&VERSION.to_le_bytes());
    fresh[8..12].copy_from_slice(&(kind as u32).to_le_bytes());
    fresh[16..24].copy_from_slice(&epoch.to_le_bytes());
    file.write_all_at(&fresh, 0)?;
    Ok(file)
}

/// The epoch a sidecar file states, for tests and for diagnosing a
/// pair that disagrees with itself.
#[cfg(test)]
pub(super) fn stated_epoch(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).ok()?;
    let mut hdr = [0u8; HEADER_BYTES as usize];
    f.read_exact_at(&mut hdr, 0).ok()?;
    if u32::from_le_bytes(hdr[0..4].try_into().unwrap()) != MAGIC {
        return None;
    }
    Some(u64::from_le_bytes(hdr[16..24].try_into().unwrap()))
}

/// The sidecar path for a `.bin`: same name, different extension, so
/// the three files of one generation sort together and a stray one is
/// obvious in a directory listing.
pub(super) fn path_for(bin_path: &std::path::Path, ext: &str) -> std::path::PathBuf {
    let mut p = bin_path.as_os_str().to_owned();
    p.push(".");
    p.push(ext);
    std::path::PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "marspot-sidecar-{}-{}-{}",
            label,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("scrollback.bin")
    }

    fn put(f: &std::fs::File, idx: u64, v: u8) {
        use std::os::unix::fs::FileExt;
        f.write_all_at(&[v, 0], HEADER_BYTES + idx * 2).unwrap();
    }
    fn get(f: &std::fs::File, idx: u64) -> Option<u8> {
        use std::os::unix::fs::FileExt;
        let mut b = [0u8; 2];
        f.read_exact_at(&mut b, HEADER_BYTES + idx * 2).ok()?;
        Some(b[0])
    }

    #[test]
    fn a_fresh_sidecar_states_the_epoch_it_was_opened_for() {
        let bin = tmp("fresh");
        let p = path_for(&bin, "marks");
        let f = open_checked(&p, 0xABCD, Kind::Marks).unwrap();
        drop(f);
        assert_eq!(stated_epoch(&p), Some(0xABCD));
    }

    #[test]
    fn reopening_for_the_same_epoch_keeps_what_was_written() {
        let bin = tmp("same");
        let p = path_for(&bin, "marks");
        let f = open_checked(&p, 7, Kind::Marks).unwrap();
        put(&f, 3, 42);
        drop(f);
        let f = open_checked(&p, 7, Kind::Marks).unwrap();
        assert_eq!(get(&f, 3), Some(42), "the same generation keeps its marks");
    }

    /// The whole point. A record filed under the previous generation is
    /// not readable as this one's, and the proof is that it is gone
    /// rather than merely different.
    #[test]
    fn a_record_from_the_previous_epoch_cannot_be_read_by_this_one() {
        let bin = tmp("epoch");
        let p = path_for(&bin, "marks");
        let f = open_checked(&p, 1, Kind::Marks).unwrap();
        put(&f, 3, 42);
        drop(f);

        let f = open_checked(&p, 2, Kind::Marks).unwrap();
        assert_eq!(get(&f, 3), None, "line 3's mark belonged to epoch 1");
        assert_eq!(stated_epoch(&p), Some(2));
    }

    /// Without the epoch check -- matching on magic and version alone,
    /// which is what the first draft of this did -- the same read
    /// succeeds and returns the previous generation's mark. This test
    /// is here so that removing the epoch comparison above makes a
    /// test fail rather than quietly changing behaviour.
    #[test]
    fn matching_on_magic_alone_would_hand_back_the_stale_mark() {
        use std::os::unix::fs::FileExt;
        let bin = tmp("noepoch");
        let p = path_for(&bin, "marks");
        let f = open_checked(&p, 1, Kind::Marks).unwrap();
        put(&f, 3, 42);
        drop(f);

        // Open the way a magic-and-version-only check would: accept the
        // file as it stands.
        let f = std::fs::OpenOptions::new().read(true).open(&p).unwrap();
        let mut hdr = [0u8; HEADER_BYTES as usize];
        f.read_exact_at(&mut hdr, 0).unwrap();
        assert_eq!(u32::from_le_bytes(hdr[0..4].try_into().unwrap()), MAGIC);
        assert_eq!(
            get(&f, 3),
            Some(42),
            "this is the wrong answer the epoch exists to prevent"
        );
        assert_ne!(
            u64::from_le_bytes(hdr[16..24].try_into().unwrap()),
            2,
            "and the header is the only place that says so"
        );
    }

    #[test]
    fn a_sidecar_of_another_kind_is_not_decoded_as_this_one() {
        let bin = tmp("kind");
        let p = path_for(&bin, "marks");
        let f = open_checked(&p, 5, Kind::Marks).unwrap();
        put(&f, 1, 9);
        drop(f);
        // Rewrite the kind field to something this build does not know.
        {
            use std::os::unix::fs::FileExt;
            let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.write_all_at(&99u32.to_le_bytes(), 8).unwrap();
        }
        let f = open_checked(&p, 5, Kind::Marks).unwrap();
        assert_eq!(get(&f, 1), None, "a different kind is not this one's data");
    }

    #[test]
    fn a_truncated_header_is_replaced_rather_than_read() {
        let bin = tmp("trunc");
        let p = path_for(&bin, "marks");
        let f = open_checked(&p, 5, Kind::Marks).unwrap();
        put(&f, 1, 9);
        drop(f);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&p)
            .unwrap()
            .set_len(7)
            .unwrap();
        let f = open_checked(&p, 5, Kind::Marks).unwrap();
        assert_eq!(get(&f, 1), None);
        assert_eq!(stated_epoch(&p), Some(5));
    }

    #[test]
    fn the_path_sits_beside_the_bin_it_belongs_to() {
        let p = path_for(std::path::Path::new("/x/scrollback.bin"), "marks");
        assert_eq!(p, std::path::Path::new("/x/scrollback.bin.marks"));
    }
}
