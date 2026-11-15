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
// inline: with the in-RAM history as a second caller it stopped being
// inlined into the file's push, about 0.3 instructions a byte of ascii
#[inline(always)]
pub(crate) fn encode_packed_cells(
    line: &[PackedCell],
    styles: &StyleTable,
    hint: RowHint,
    out: &mut Vec<u8>,
) -> bool {
    if styles.is_positional() {
        // numbers name positions: two cells in the same attributes
        // carry different ones, so a run is decided by the values
        let key = |c: PackedCell| styles.get(c.style_id()).0 << 1 | c.is_cluster() as u64;
        return encode_runs(line, styles, hint, out, key);
    }
    encode_runs(line, styles, hint, out, PackedCell::run_key)
}

/// The encoder, with `key` deciding which neighbours share a run: equal
/// keys exactly when equal attributes and cluster flags, and 0 for a
/// plain cell.
#[inline(always)]
fn encode_runs(
    line: &[PackedCell],
    styles: &StyleTable,
    hint: RowHint,
    out: &mut Vec<u8>,
    key_of: impl Fn(PackedCell) -> u64,
) -> bool {
    let start = out.len();
    let raw_len = line.len() * CELL_MEM_BYTES;
    let plain_from = (hint.plain_from as usize).min(line.len());
    debug_assert!(line[plain_from..].iter().all(|c| c.run_key() == 0));
    let mut wide = hint.wide;
    let mut i = 0;
    while i < line.len() {
        let key = key_of(line[i]);
        let mut n = 1;
        while i + n < plain_from && key_of(line[i + n]) == key {
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
