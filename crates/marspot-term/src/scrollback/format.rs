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
//
// v4 (2026-11-12) — a line may be stored as runs instead of as cells.
// A run is the attributes once, then one byte per cell when every
// character in it is below 0x80 and four otherwise.  A line of plain
// text costs about a byte a cell instead of twenty: the hot file used
// to fill after 6.4 million cells (about 7 MB of `cat` output), and
// every fill is a rotation — an unlink and a rename of 128 MB on the
// parse thread, a third of that thread's time on a bulk `cat`.
//
// The record header is unchanged; bit 1 of the byte that carries
// `wrapped` says the body is runs.  A record written before v4 has
// that bit clear and is read as it always was, so a v2 or v3 file has
// its header moved up in place and keeps every line.  Rollback is as
// for v3: an older binary quarantines the pair.
pub(super) const FILE_VERSION: u32 = 4;
pub(super) const FILE_MIN_COMPAT: u32 = 2;
/// Cell width of the records a v2 file holds.
pub(super) const V2_CELL_BYTES: usize = 13;
pub(super) const FILE_HEADER_BYTES: u64 = 32;
pub(super) const FILE_REC_HEADER_BYTES: usize = 4 + 1 + 2; // rec_len + flags + cols
/// Record flag: the line continues on the next one.
pub(super) const REC_WRAPPED: u8 = 1 << 0;
/// Record flag: the body is runs, not cells.
pub(super) const REC_PACKED: u8 = 1 << 1;
/// A run's header: cell count, payload kind, the cells' attributes.
const RUN_HEADER_BYTES: usize = 2 + 1 + RUN_ATTR_BYTES;
const RUN_ATTR_BYTES: usize = crate::grid::CELL_MEM_BYTES - 4;
/// Run payload: one byte per cell, each a character below 0x80.
const RUN_BYTES: u8 = 0;
/// Run payload: four bytes per cell, the cell's `ch` as stored.
const RUN_SCALARS: u8 = 1;

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
    let body = read_body_at(bin, offset)?;
    if body.len() < 3 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "scrollback rec too short",
        ));
    }
    let wrapped = body[0] & REC_WRAPPED != 0;
    let cols = u16::from_le_bytes([body[1], body[2]]) as usize;
    let cells = decode_record_cells(body[0], &body[3..], cols).ok_or_else(|| {
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

/// The record at `offset` without its length: flags, columns, cells.
pub(super) fn read_body_at(bin: &std::fs::File, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    read_exact_at(bin, &mut len_buf, offset)?;
    let mut body = vec![0u8; u32::from_le_bytes(len_buf) as usize];
    read_exact_at(bin, &mut body, offset + 4)?;
    Ok(body)
}

/// A record's cells, at whatever width the record was written.
///
/// The width is not stored; it is what the body length says it is.
/// 20 bytes per cell since v3, 13 before it, and the two only agree at
/// `cols == 0`, where there are no cells.  A body that is neither is
/// not a record this code wrote — `None`, never a guess.
pub(super) fn decode_record_cells(
    flags: u8,
    cell_bytes: &[u8],
    cols: usize,
) -> Option<Vec<crate::grid::Cell>> {
    if flags & REC_PACKED != 0 {
        return decode_packed(cell_bytes, cols);
    }
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

/// Append `line` to `out` as runs of cells that share their attributes.
///
/// `false`, with `out` as it was, when the runs would not be smaller
/// than the cells themselves — a line that changes colour every cell.
pub(super) fn encode_packed(line: &[crate::grid::Cell], out: &mut Vec<u8>) -> bool {
    const N: usize = crate::grid::CELL_MEM_BYTES;
    let start = out.len();
    let (cells, _) = crate::grid::Cell::slice_as_bytes(line).as_chunks::<N>();
    let raw_len = cells.len() * N;
    let attrs_of =
        |c: &[u8; N]| u128::from_ne_bytes(*c.last_chunk::<RUN_ATTR_BYTES>().expect("a cell"));
    let mut i = 0;
    while i < cells.len() {
        let attrs = attrs_of(&cells[i]);
        // a line is at most u16::MAX cells, so a run is too
        let head = out.len();
        out.extend_from_slice(&[0, 0, RUN_BYTES]);
        out.extend_from_slice(&attrs.to_ne_bytes());
        // One pass, on the guess that the run is plain text: its low
        // bytes go straight out, and only a run that turns out to hold
        // a character past 0x7F is written again at four bytes a cell.
        let body = out.len();
        let rest = &cells[i..];
        out.resize(body + rest.len(), 0);
        let mut any = 0u32;
        let mut n = 0;
        for (dst, c) in out[body..].iter_mut().zip(rest) {
            if attrs_of(c) != attrs {
                break;
            }
            any |= u32::from_ne_bytes(*c.first_chunk::<4>().expect("a cell"));
            *dst = c[0];
            n += 1;
        }
        out.truncate(body + n);
        if any >= 0x80 {
            out.truncate(body);
            out[head + 2] = RUN_SCALARS;
            for c in &rest[..n] {
                out.extend_from_slice(&c[..4]);
            }
        }
        out[head..head + 2].copy_from_slice(&(n as u16).to_le_bytes());
        if out.len() - start >= raw_len {
            out.truncate(start);
            return false;
        }
        i += n;
    }
    true
}

/// The cells of a body written by [`encode_packed`].  `None` for
/// anything that is not exactly `cols` cells of well-formed runs.
fn decode_packed(mut body: &[u8], cols: usize) -> Option<Vec<crate::grid::Cell>> {
    const N: usize = crate::grid::CELL_MEM_BYTES;
    let mut cells = Vec::with_capacity(cols);
    while cells.len() < cols {
        let (head, rest) = body.split_first_chunk::<RUN_HEADER_BYTES>()?;
        let n = u16::from_le_bytes([head[0], head[1]]) as usize;
        let width = match head[2] {
            RUN_BYTES => 1,
            RUN_SCALARS => 4,
            _ => return None,
        };
        if n == 0 || cells.len() + n > cols || rest.len() < n * width {
            return None;
        }
        let (payload, rest) = rest.split_at(n * width);
        let mut mem = [0u8; N];
        mem[4..].copy_from_slice(&head[3..]);
        for ch in payload.chunks_exact(width) {
            mem[..4].fill(0);
            mem[..width].copy_from_slice(ch);
            cells.push(crate::grid::Cell::from_mem_bytes(&mem));
        }
        body = rest;
    }
    body.is_empty().then_some(cells)
}

/// Move an older file's header to the current version without touching
/// a single record.
///
/// Eight bytes: version and cell_abi.  Written through its own
/// non-append descriptor — a positional write on an `O_APPEND` fd is
/// not guaranteed to land at the position asked for — and synced,
/// because the next record appended is 20 bytes per cell and a v2
/// header in front of it would be a lie on the next open.
pub(super) fn upgrade_header_in_place(bin_path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::OpenOptions::new().write(true).open(bin_path)?;
    let mut patch = [0u8; 8];
    patch[0..4].copy_from_slice(&FILE_VERSION.to_le_bytes());
    patch[4..8].copy_from_slice(&(crate::grid::CELL_MEM_BYTES as u32).to_le_bytes());
    f.write_all_at(&patch, 4)?;
    f.sync_data()?;
    crate::lx_info!(
        "scrollback.upgraded_header",
        "header moved to the current version in place; existing records stay as written",
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

/// The epoch a `.bin` states, or `None` when it is not one of ours.
/// Used to open a sidecar against a file this process did not open as
/// its writer -- the cold tier.
pub(super) fn read_epoch(bin_path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(bin_path).ok()?;
    let mut hdr = [0u8; FILE_HEADER_BYTES as usize];
    f.read_exact_at(&mut hdr, 0).ok()?;
    if u32::from_le_bytes(hdr[0..4].try_into().unwrap()) != FILE_MAGIC {
        return None;
    }
    Some(u64::from_le_bytes(hdr[24..32].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body whose length fits neither width is not a record this code
    /// wrote, and is refused rather than decoded at a guessed width.
    #[test]
    fn a_record_of_neither_width_is_refused() {
        let body_20 = vec![0u8; 3 * crate::grid::CELL_MEM_BYTES];
        assert_eq!(decode_record_cells(0, &body_20, 3).map(|v| v.len()), Some(3));
        let body_13 = vec![0u8; 3 * V2_CELL_BYTES];
        assert_eq!(decode_record_cells(0, &body_13, 3).map(|v| v.len()), Some(3));
        assert!(decode_record_cells(0, &[0u8; 50], 3).is_none());
        assert!(decode_record_cells(0, &[], 3).is_none());
        assert_eq!(decode_record_cells(0, &[], 0).map(|v| v.len()), Some(0));
        assert!(decode_record_cells(0, &[0u8; 20], 0).is_none());
    }

    fn cell(ch: char, fg: u8, bold: bool) -> crate::grid::Cell {
        let mut c = crate::grid::Cell { ch, ..Default::default() };
        c.attrs.fg = crate::grid::Color::indexed(fg);
        c.attrs.bold = bold;
        c
    }

    fn round_trip(line: &[crate::grid::Cell]) -> (usize, Vec<crate::grid::Cell>) {
        let mut body = Vec::new();
        assert!(encode_packed(line, &mut body), "worth packing");
        let back = decode_record_cells(REC_PACKED, &body, line.len()).expect("decodes");
        (body.len(), back)
    }

    /// Every kind of cell a line can hold comes back as it went in:
    /// plain text, a colour change mid-line, a wide character with its
    /// trailing cell, a character outside the BMP.
    #[test]
    fn a_packed_line_reads_back_cell_for_cell() {
        let mut line: Vec<_> = "[INFO]".chars().map(|c| cell(c, 1, true)).collect();
        line.extend(" request 42 ".chars().map(|c| cell(c, 7, false)));
        line.extend([cell('\u{4F60}', 7, false), cell('\0', 7, false)]);
        line.extend([cell('\u{1F680}', 2, false), cell('\0', 2, false), cell(' ', 2, false)]);
        let (len, back) = round_trip(&line);
        assert_eq!(back, line);
        assert!(len < line.len() * crate::grid::CELL_MEM_BYTES / 3);
    }

    /// Plain text is a byte a cell plus one run header.
    #[test]
    fn a_line_of_plain_text_costs_a_byte_a_cell() {
        let line: Vec<_> = (0..120u8).map(|i| cell((b' ' + i % 90) as char, 7, false)).collect();
        let (len, back) = round_trip(&line);
        assert_eq!(back, line);
        assert_eq!(len, RUN_HEADER_BYTES + 120);
    }

    /// A line that would be larger as runs is left for the caller to
    /// store as cells, and the buffer is as it was.
    #[test]
    fn a_line_that_changes_every_cell_is_not_packed() {
        let line: Vec<_> = (0..40u8).map(|i| cell('x', i, false)).collect();
        let mut body = vec![9u8; 5];
        assert!(!encode_packed(&line, &mut body));
        assert_eq!(body, vec![9u8; 5]);
    }

    /// Runs that do not add up to the line, or that run past the body,
    /// are refused rather than padded or cut.
    #[test]
    fn malformed_runs_are_refused() {
        let line: Vec<_> = "hello".chars().map(|c| cell(c, 7, false)).collect();
        let mut body = Vec::new();
        assert!(encode_packed(&line, &mut body));
        assert!(decode_record_cells(REC_PACKED, &body, 4).is_none());
        assert!(decode_record_cells(REC_PACKED, &body, 6).is_none());
        assert!(decode_record_cells(REC_PACKED, &body[..body.len() - 1], 5).is_none());
        let mut bad_kind = body.clone();
        bad_kind[2] = 9;
        assert!(decode_record_cells(REC_PACKED, &bad_kind, 5).is_none());
        let mut extra = body.clone();
        extra.push(0);
        assert!(decode_record_cells(REC_PACKED, &extra, 5).is_none());
    }
}
