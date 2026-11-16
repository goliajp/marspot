//! Filing a grid row into a record straight from the grid's own
//! eight-byte cells.
//!
//! The bytes are the format's (see `format.rs`), the same ones
//! [`super::format::encode_packed_with`] writes for the row expanded to
//! [`Cell`]s; only the reading side differs.  A run ends where the
//! attribute number or the cluster flag changes, a four-byte compare,
//! and a run's attributes are looked up once.

use super::format::{RUN_BYTES, RUN_SCALARS, RowHint};
use crate::grid::{CELL_MEM_BYTES, Cell, PackedCell, StyleTable};

/// The sixteen bytes a run header carries: the attributes as a `Cell`
/// holds them in memory, cluster flag included.
#[inline(always)]
fn attr_bytes(cell: PackedCell, styles: &StyleTable) -> [u8; CELL_MEM_BYTES - 4] {
    styles.get(cell.style_id()).mem_bytes(cell.is_cluster())
}

/// Append `line` to `out` as runs; `false`, with `out` as it was, when
/// that would not be smaller than the cells.
///
/// `canon` is room for a copy of the row, used only when the grid
/// numbers its cells by position.
pub(crate) fn encode_packed_cells(
    line: &[PackedCell],
    styles: &StyleTable,
    hint: RowHint,
    out: &mut Vec<u8>,
    canon: &mut Vec<PackedCell>,
) -> bool {
    if styles.is_positional() {
        canonical(line, styles, canon);
        return encode_runs(canon, styles, hint, out);
    }
    encode_runs(line, styles, hint, out)
}

/// `line` with each cell that has the same attributes as the one before
/// it given that cell's number: numbered by position, two neighbours in
/// one colour carry different numbers, and a run is decided by numbers.
#[cold]
fn canonical(line: &[PackedCell], styles: &StyleTable, canon: &mut Vec<PackedCell>) {
    canon.clear();
    let mut prev = PackedCell::BLANK;
    for &c in line {
        let id = c.style_id();
        let same = id != 0 && prev.style_id() != 0 && styles.get(id) == styles.get(prev.style_id());
        prev = if same { c.with_style(prev.style_id()) } else { c };
        canon.push(prev);
    }
}

/// The encoder: a run ends where the number or the cluster flag
/// changes.
#[inline(always)]
fn encode_runs(line: &[PackedCell], styles: &StyleTable, hint: RowHint, out: &mut Vec<u8>) -> bool {
    let start = out.len();
    let raw_len = line.len() * CELL_MEM_BYTES;
    let plain_from = (hint.plain_from as usize).min(line.len());
    debug_assert!(line[plain_from..].iter().all(|c| c.run_key() == 0));
    let mut wide = hint.wide;
    let mut i = 0;
    while i < line.len() {
        let key = line[i].run_key();
        let mut n = 1;
        while i + n < plain_from && line[i + n].run_key() == key {
            n += 1;
        }
        if i + n >= plain_from && key == 0 {
            n = line.len() - i;
        }
        let run = &line[i..i + n];
        out.extend_from_slice(&(n as u16).to_le_bytes());
        let kind = out.len();
        out.push(RUN_BYTES);
        out.extend_from_slice(&attr_bytes(line[i], styles));
        let body = out.len();
        loop {
            let mut any = 0u32;
            if wide {
                out.resize(body + 4 * n, 0);
                let (dst, _) = out[body..].as_chunks_mut::<4>();
                for (dst, c) in dst.iter_mut().zip(run) {
                    let ch = c.ch_bits();
                    any |= ch;
                    *dst = ch.to_ne_bytes();
                }
                if any >= 0x80 {
                    out[kind] = RUN_SCALARS;
                    break;
                }
            } else {
                out.extend(run.iter().map(|c| {
                    any |= c.ch_bits();
                    c.bits() as u8
                }));
                if any < 0x80 {
                    break;
                }
            }
            wide = !wide;
            out.truncate(body);
        }
        if out.len() - start >= raw_len {
            out.truncate(start);
            return false;
        }
        i += n;
    }
    true
}

/// Append `line` as the twenty bytes a cell each that a record holds
/// when runs would be no smaller.
pub(crate) fn append_cells(line: &[PackedCell], styles: &StyleTable, out: &mut Vec<u8>) {
    for &c in line {
        let cell: Cell = c.expand(styles);
        out.extend_from_slice(Cell::slice_as_bytes(std::slice::from_ref(&cell)));
    }
}

#[cfg(test)]
#[path = "format_packed_tests.rs"]
mod tests;
