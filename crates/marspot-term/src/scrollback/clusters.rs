//! The cluster text of lines that have scrolled into the file.
//!
//! A cell on screen holds a pool index and the pool holds the text; a
//! cell in the file holds the base codepoint instead, because that is
//! what a binary without this feature reads out of the record and the
//! record format is not changing (a rollback must find its own history
//! where it left it). So the text of a cluster that has scrolled off
//! is keyed by *where it was*, not by what the cell says, and it lives
//! in a sidecar.
//!
//! Two files rather than one, for the same reason `.bin` has `.idx`:
//!
//! * `.clusters` -- one fixed [`SLOT_BYTES`] slot per line, so line
//!   `n`'s record is found by arithmetic and a run of lines is one
//!   read. A line with no clusters is an all-zero slot, which is a
//!   hole in a sparse file: the common case costs no disk at all.
//! * `.clustertext` -- the records the slots point at, appended.
//!
//! Both state the epoch of the `.bin` they describe, so reflow,
//! quarantine, rotation and a crash-truncated tail all make them
//! unreadable rather than wrong. See [`super::sidecar`].

use super::sidecar::{HEADER_BYTES, LineExtras};

/// `offset u64` + `len u32` + reserved `u32`.
///
/// Sixteen rather than twelve so the reserved word keeps the slot
/// aligned and leaves room for the one thing a slot might still have
/// to say (a continuation, if a line ever outgrows one record).
pub(super) const SLOT_BYTES: u64 = 16;

/// An offset of zero means "no clusters on this line": the blob file's
/// records all start after its header, so zero is not a value a real
/// record can have. That is what makes an unwritten slot -- a hole --
/// readable as an answer rather than as a missing one.
const NO_RECORD: u64 = 0;

impl LineExtras {
    /// File the clusters of local line `local`.
    ///
    /// Called only for lines that have some, so a stream of plain text
    /// never reaches here. Failure is silent and total: the slot is
    /// only written once its blob is on disk, so a half-written record
    /// reads as a line with no clusters rather than as a line with
    /// somebody else's.
    pub(super) fn put_clusters(&mut self, local: u64, clusters: &[(u16, String)]) {
        use std::os::unix::fs::FileExt;
        let (Some(slots), Some(text)) = (self.cluster_slots.as_ref(), self.cluster_text.as_ref())
        else {
            return;
        };
        let mut blob = Vec::with_capacity(2 + clusters.len() * 8);
        blob.extend_from_slice(&(clusters.len() as u16).to_le_bytes());
        for (col, s) in clusters {
            blob.extend_from_slice(&col.to_le_bytes());
            blob.extend_from_slice(&(s.len() as u16).to_le_bytes());
            blob.extend_from_slice(s.as_bytes());
        }
        let at = self.cluster_text_len;
        if text.write_all_at(&blob, at).is_err() {
            return;
        }
        self.cluster_text_len = at + blob.len() as u64;
        let mut slot = [0u8; SLOT_BYTES as usize];
        slot[0..8].copy_from_slice(&at.to_le_bytes());
        slot[8..12].copy_from_slice(&(blob.len() as u32).to_le_bytes());
        let _ = slots.write_all_at(&slot, HEADER_BYTES + local * SLOT_BYTES);
    }

    /// The clusters of local line `local`, appended to `out`.
    ///
    /// `out` is the caller's buffer so that walking a viewport does
    /// not allocate per row. Returns whether anything was added.
    pub(super) fn read_clusters(&self, local: u64, out: &mut Vec<(u16, String)>) -> bool {
        use std::os::unix::fs::FileExt;
        let (Some(slots), Some(text)) = (self.cluster_slots.as_ref(), self.cluster_text.as_ref())
        else {
            return false;
        };
        let mut slot = [0u8; SLOT_BYTES as usize];
        if slots
            .read_exact_at(&mut slot, HEADER_BYTES + local * SLOT_BYTES)
            .is_err()
        {
            return false;
        }
        let at = u64::from_le_bytes(slot[0..8].try_into().unwrap());
        let len = u32::from_le_bytes(slot[8..12].try_into().unwrap()) as usize;
        if at == NO_RECORD || len < 2 {
            return false;
        }
        let mut blob = vec![0u8; len];
        if text.read_exact_at(&mut blob, at).is_err() {
            return false;
        }
        decode_into(&blob, out)
    }
}

/// Parse one record, appending what it holds.
///
/// A record that disagrees with itself -- a length running past the
/// end, bytes that are not UTF-8 -- stops the walk and keeps what came
/// before it. These files are caches rebuilt from the bytelog, so the
/// useful half of a damaged record is worth more than refusing the
/// line, and the harm of being wrong is bounded: the cells it does not
/// cover read as their base codepoints, which is what they looked like
/// before any of this existed.
fn decode_into(blob: &[u8], out: &mut Vec<(u16, String)>) -> bool {
    let count = u16::from_le_bytes(blob[0..2].try_into().unwrap()) as usize;
    let mut p = 2usize;
    let before = out.len();
    for _ in 0..count {
        if p + 4 > blob.len() {
            break;
        }
        let col = u16::from_le_bytes(blob[p..p + 2].try_into().unwrap());
        let len = u16::from_le_bytes(blob[p + 2..p + 4].try_into().unwrap()) as usize;
        p += 4;
        if p + len > blob.len() {
            break;
        }
        let Ok(s) = std::str::from_utf8(&blob[p..p + len]) else {
            break;
        };
        p += len;
        out.push((col, s.to_string()));
    }
    out.len() > before
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "marspot-clusters-{}-{}-{}",
            label,
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("scrollback.bin")
    }

    fn one(col: u16, s: &str) -> Vec<(u16, String)> {
        vec![(col, s.to_string())]
    }

    #[test]
    fn a_filed_line_reads_back_exactly() {
        let bin = tmp("roundtrip");
        let mut e = LineExtras::open_hot(&bin, 1);
        let want = vec![
            (0u16, "e\u{301}".to_string()),
            (7u16, "\u{1F1EF}\u{1F1F5}".to_string()),
            (
                40u16,
                "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".to_string(),
            ),
        ];
        e.put_clusters(3, &want);
        let mut got = Vec::new();
        assert!(e.read_clusters(3, &mut got));
        assert_eq!(got, want);
    }

    /// The common case is a line with nothing on it, and it must be
    /// free: no record written, and the slot that was never written
    /// reads as "none" rather than as a failure.
    #[test]
    fn a_line_that_was_never_filed_has_no_clusters() {
        let bin = tmp("absent");
        let mut e = LineExtras::open_hot(&bin, 1);
        e.put_clusters(5, &one(0, "e\u{301}"));
        let mut got = Vec::new();
        assert!(!e.read_clusters(4, &mut got), "line 4 was never filed");
        assert!(!e.read_clusters(9999, &mut got), "nor was line 9999");
        assert!(got.is_empty());
    }

    /// Lines are addressed by arithmetic, so filing a far line must not
    /// make the near ones say anything, and the file must stay a hole
    /// rather than becoming megabytes of zeroes on disk.
    #[test]
    fn filing_a_distant_line_leaves_the_slots_before_it_empty() {
        let bin = tmp("sparse");
        let mut e = LineExtras::open_hot(&bin, 1);
        e.put_clusters(100_000, &one(2, "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}"));
        let mut got = Vec::new();
        assert!(!e.read_clusters(0, &mut got));
        assert!(!e.read_clusters(99_999, &mut got));
        assert!(e.read_clusters(100_000, &mut got));
        assert_eq!(got.len(), 1);

        let p = super::super::sidecar::path_for(&bin, "clusters");
        let md = std::fs::metadata(&p).unwrap();
        assert!(
            md.len() >= HEADER_BYTES + 100_000 * SLOT_BYTES,
            "the slot is addressed where it belongs"
        );
        #[cfg(target_os = "macos")]
        {
            use std::os::unix::fs::MetadataExt;
            assert!(
                md.blocks() * 512 < md.len() / 4,
                "a table of holes occupies {} bytes for a {}-byte span",
                md.blocks() * 512,
                md.len()
            );
        }
    }

    /// Two lines, filed in either order, keep their own text.
    #[test]
    fn two_lines_do_not_read_each_others_records() {
        let bin = tmp("two");
        let mut e = LineExtras::open_hot(&bin, 1);
        e.put_clusters(2, &one(0, "a\u{301}"));
        e.put_clusters(1, &one(0, "b\u{301}"));
        let mut got = Vec::new();
        assert!(e.read_clusters(1, &mut got));
        assert_eq!(got, one(0, "b\u{301}"));
        got.clear();
        assert!(e.read_clusters(2, &mut got));
        assert_eq!(got, one(0, "a\u{301}"));
    }

    /// The epoch is what makes a reflowed line safe: the sidecars of
    /// the generation before it are not readable as this one's, so
    /// line 3 cannot come back wearing line 3-of-last-time's marks.
    #[test]
    fn a_record_from_the_previous_epoch_is_not_this_lines() {
        let bin = tmp("epoch");
        let mut e = LineExtras::open_hot(&bin, 1);
        e.put_clusters(3, &one(0, "e\u{301}"));
        drop(e);
        let e = LineExtras::open_hot(&bin, 2);
        let mut got = Vec::new();
        assert!(
            !e.read_clusters(3, &mut got),
            "epoch 1's line 3 is not epoch 2's line 3"
        );
    }

    /// Reopening for the same generation keeps what was filed, and the
    /// next record goes after it rather than over it.
    #[test]
    fn reopening_the_same_generation_appends_rather_than_overwrites() {
        let bin = tmp("append");
        let mut e = LineExtras::open_hot(&bin, 9);
        e.put_clusters(1, &one(0, "first\u{301}"));
        drop(e);
        let mut e = LineExtras::open_hot(&bin, 9);
        e.put_clusters(2, &one(0, "second\u{301}"));
        let mut got = Vec::new();
        assert!(e.read_clusters(1, &mut got));
        assert_eq!(got, one(0, "first\u{301}"), "the earlier record survived");
        got.clear();
        assert!(e.read_clusters(2, &mut got));
        assert_eq!(got, one(0, "second\u{301}"));
    }

    /// A blob whose length field runs past what was stored keeps the
    /// clusters that did fit. The failure mode has to be "fewer
    /// clusters", never "somebody else's bytes".
    #[test]
    fn a_damaged_record_keeps_the_part_that_parses() {
        let good = {
            let mut b = Vec::new();
            b.extend_from_slice(&2u16.to_le_bytes());
            b.extend_from_slice(&0u16.to_le_bytes());
            b.extend_from_slice(&3u16.to_le_bytes());
            b.extend_from_slice("e\u{301}".as_bytes());
            b.extend_from_slice(&5u16.to_le_bytes());
            b.extend_from_slice(&400u16.to_le_bytes()); // a length that is a lie
            b.extend_from_slice("x".as_bytes());
            b
        };
        let mut out = Vec::new();
        assert!(decode_into(&good, &mut out));
        assert_eq!(out, vec![(0u16, "e\u{301}".to_string())]);
    }

    #[test]
    fn a_record_claiming_more_clusters_than_it_holds_stops_at_the_end() {
        let mut b = Vec::new();
        b.extend_from_slice(&9u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice("ab".as_bytes());
        let mut out = Vec::new();
        assert!(decode_into(&b, &mut out));
        assert_eq!(out, vec![(1u16, "ab".to_string())]);
    }
}
