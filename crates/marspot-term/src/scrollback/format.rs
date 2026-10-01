//! The bytes on disk, and the functions that read and write them.
//!
//! Split out of `scrollback.rs` so the format can be read on its own:
//! every constant here is part of a file other binaries also open, so
//! changing one is a compatibility decision rather than a local one.

pub(super) const FILE_MAGIC: u32 = 0x5350_5301;
// F3+10i — bump v1 → v2 to force-discard scrollback files written by
// the mmap-write / blank-row-pollution era.  User authorised the
// destruction ("新的历史没问题,老的全都不要了都可以"): pre-v2 files
// can carry past-EOF idx tails + spinner blank pushes interleaved
// mid-history that surface as blank rows in scrolled views.
// `Scrollback::file` (RFC-004 A.4) handles every InvalidData reject
// from open() — bad magic, version outside compat, cell-ABI drift,
// truncated header, ragged idx — by renaming the pair to
// `.corrupt-<ts>` and reopening fresh, so incompatible files are
// quarantined (not silently shadowed) and the session keeps disk
// persistence.  Future scrollback shape changes (e.g. adding a
// record-level field) only need VERSION++ without touching
// MIN_COMPAT, preserving back-compat.
//
// v3 (2026-09-17) — a cell is stored as its own 20 in-memory bytes
// (`grid::Cell::slice_as_bytes`) instead of being encoded to 13, which
// took the encode loop off the parse thread: +13–21 % file-backed
// parse throughput, measured on mini against the 13-byte encoder.
//
// A v2 file is NOT rewritten.  Every record already states its body
// length and its column count, so the width of its cells is
// `(rec_len - 3) / cols` — 13 or 20, and the two only coincide at
// cols = 0, where there are no cells to read.  Opening a v2 file
// rewrites its header in place (version and cell_abi, eight bytes)
// and appends 20-byte records after the 13-byte ones; the reader
// decodes each record at its own width.  Nothing is copied, nothing
// is lost, and startup costs one small write.
//
// The one direction this does not cover is rollback: a binary from
// before v3 sees version 3 and quarantines the pair (renamed, not
// deleted — see `Scrollback::file`).
pub(super) const FILE_VERSION: u32 = 3;
pub(super) const FILE_MIN_COMPAT: u32 = 2;
/// Cell width of the records a v2 file holds.
pub(super) const V2_CELL_BYTES: usize = 13;
pub(super) const FILE_HEADER_BYTES: u64 = 32;
pub(super) const FILE_REC_HEADER_BYTES: usize = 4 + 1 + 2; // rec_len + wrapped + cols

/// F2+5 — hot/cold scrollback rotation cap.  When `scrollback.bin`
/// would exceed this many bytes, the writer flushes + closes, renames
/// the current pair to `scrollback.cold.bin` / `scrollback.cold.idx`
/// (overwriting any prior cold pair — that's the "delete" tier), and
/// opens a fresh empty hot pair.  Older logical line indices stay
/// addressable through the cold pair until the next rotation evicts
/// it.  Tunable via `MARSPOT_SCROLLBACK_HOT_CAP_MB`; default 128 MB
/// → per-pane disk budget ≤ 256 MB (hot + cold), 9 panes ≤ 2.3 GB
/// total.  At F2+4 trim's ~400 B/row average that's ~330 k rows hot
/// + 330 k rows cold = ~9 hrs busy claudecode history per pane.
pub(super) const HOT_BYTES_CAP_DEFAULT: u64 = 128 * 1024 * 1024;

pub(super) fn hot_bytes_cap() -> u64 {
    std::env::var("MARSPOT_SCROLLBACK_HOT_CAP_MB")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|mb| mb.saturating_mul(1024 * 1024))
        .unwrap_or(HOT_BYTES_CAP_DEFAULT)
}

/// Placeholder File handle used during `rotate_to_cold` to hold the
/// `File`-typed fields while the real files are being renamed +
/// reopened.  Opening `/dev/null` gives us a real `File` so the
/// fields stay non-Option without needing `Option<File>` and the
/// associated unwraps everywhere on the hot read paths.
pub(super) fn dev_null_file() -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).open("/dev/null")
}

pub(super) fn read_exact_at(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, offset)
}

pub(super) fn read_idx_at(idx: &std::fs::File, line_idx: u64) -> std::io::Result<u64> {
    let mut buf = [0u8; 8];
    read_exact_at(idx, &mut buf, line_idx * 8)?;
    Ok(u64::from_le_bytes(buf))
}

pub(super) fn read_record_at(
    bin: &std::fs::File,
    offset: u64,
) -> std::io::Result<(Vec<crate::grid::Cell>, bool)> {
    let mut len_buf = [0u8; 4];
    read_exact_at(bin, &mut len_buf, offset)?;
    let rec_len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; rec_len];
    read_exact_at(bin, &mut body, offset + 4)?;
    if body.len() < 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scrollback rec too short",
        ));
    }
    let wrapped = body[0] != 0;
    let cols = u16::from_le_bytes([body[1], body[2]]) as usize;
    let cells = decode_record_cells(&body[3..], cols).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "scrollback rec at {offset}: {} cell bytes for {cols} cols is neither {V2_CELL_BYTES} nor {} per cell",
                body.len() - 3,
                crate::grid::CELL_MEM_BYTES
            ),
        )
    })?;
    Ok((cells, wrapped))
}

/// A record's cells, at whatever width the record was written.
///
/// The width is not stored; it is what the body length says it is.
/// 20 bytes per cell since v3, 13 before it, and the two only agree at
/// `cols == 0`, where there are no cells.  A body that is neither is
/// not a record this code wrote — `None`, never a guess.
pub(super) fn decode_record_cells(
    cell_bytes: &[u8],
    cols: usize,
) -> Option<Vec<crate::grid::Cell>> {
    let mut cells = Vec::with_capacity(cols);
    if cols == 0 {
        return (cell_bytes.is_empty()).then_some(cells);
    }
    if cell_bytes.len() == cols * crate::grid::CELL_MEM_BYTES {
        for chunk in cell_bytes.chunks_exact(crate::grid::CELL_MEM_BYTES) {
            cells.push(crate::grid::Cell::from_mem_bytes(chunk.try_into().ok()?));
        }
        return Some(cells);
    }
    if cell_bytes.len() == cols * V2_CELL_BYTES {
        for chunk in cell_bytes.chunks_exact(V2_CELL_BYTES) {
            let ch =
                char::from_u32(u32::from_le_bytes(chunk[0..4].try_into().ok()?)).unwrap_or(' ');
            let attrs = crate::terminal::deserialize_attrs_pub(&chunk[4..]);
            cells.push(crate::grid::Cell { ch, attrs });
        }
        return Some(cells);
    }
    None
}

/// Move a v2 file's header to v3 without touching a single record.
///
/// Eight bytes: version and cell_abi.  Written through its own
/// non-append descriptor — a positional write on an `O_APPEND` fd is
/// not guaranteed to land at the position asked for — and synced,
/// because the next record appended is 20 bytes per cell and a v2
/// header in front of it would be a lie on the next open.
pub(super) fn upgrade_v2_header_in_place(bin_path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::OpenOptions::new().write(true).open(bin_path)?;
    let mut patch = [0u8; 8];
    patch[0..4].copy_from_slice(&FILE_VERSION.to_le_bytes());
    patch[4..8].copy_from_slice(&(crate::grid::CELL_MEM_BYTES as u32).to_le_bytes());
    f.write_all_at(&patch, 4)?;
    f.sync_data()?;
    crate::lx_info!(
        "scrollback.upgraded_v2",
        "header moved to v3 in place; existing 13-byte records stay as written",
        bin = bin_path.display()
    );
    Ok(())
}

/// Put an epoch into a header that was written before the field meant
/// anything.  Same shape as the v2 upgrade above: one 8-byte write at a
/// fixed offset, no record touched.
pub(super) fn stamp_epoch_in_place(bin_path: &std::path::Path, epoch: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::OpenOptions::new().write(true).open(bin_path)?;
    f.write_all_at(&epoch.to_le_bytes(), 24)?;
    f.sync_data()?;
    Ok(())
}

pub(super) fn pad_or_clip(line: &[crate::grid::Cell], cols: usize) -> Vec<crate::grid::Cell> {
    if line.len() == cols {
        return line.to_vec();
    }
    let mut out = Vec::with_capacity(cols);
    let take = line.len().min(cols);
    out.extend_from_slice(&line[..take]);
    out.resize(cols, crate::grid::Cell::default());
    out
}

/// A value that changes whenever a local line index in this file stops
/// meaning the line it used to mean.
///
/// A local line index -- the k-th record in one file -- is stable under
/// everything except reflow: `restart` truncates the file back to its
/// header and pushes the re-wrapped lines in, so index 0 is a different
/// line than it was a moment ago.  Anything filed against a line (the
/// text of a grapheme cluster too wide for its cell, where a command
/// began, what a hyperlink pointed at) has to be able to tell the two
/// runs apart, or it attaches one line's note to another -- not a
/// crash, a quiet wrong character, and only where a note happened to
/// exist before.
///
/// The value is a nanosecond timestamp with a per-process counter
/// mixed in, so two reflows in the same nanosecond still differ.  Not
/// random: a readable time is worth having when asking which run a
/// record came from, and a collision costs one discarded sidecar.
pub(super) fn new_epoch() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // The low bits of a nanosecond clock are the noisy ones, so the
    // counter goes in the top where it cannot be swallowed.
    ns ^ (SEQ.fetch_add(1, Ordering::Relaxed) << 48)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body whose length fits neither width is not a record this code
    /// wrote, and is refused rather than decoded at a guessed width.
    #[test]
    fn a_record_of_neither_width_is_refused() {
        let body_20 = vec![0u8; 3 * crate::grid::CELL_MEM_BYTES];
        assert_eq!(decode_record_cells(&body_20, 3).map(|v| v.len()), Some(3));
        let body_13 = vec![0u8; 3 * V2_CELL_BYTES];
        assert_eq!(decode_record_cells(&body_13, 3).map(|v| v.len()), Some(3));
        assert!(decode_record_cells(&[0u8; 50], 3).is_none());
        assert!(decode_record_cells(&[], 3).is_none());
        assert_eq!(decode_record_cells(&[], 0).map(|v| v.len()), Some(0));
        assert!(decode_record_cells(&[0u8; 20], 0).is_none());
    }
}
