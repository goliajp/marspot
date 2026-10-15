//! The digest `publish_if_changed` compares to skip a publish.

use crate::grid::Grid;

/// FNV-1a over the full publish payload — header dims/cursor/flags
/// plus every cell + per-row wrapped flag.  Walked in the same order
/// `publish()` would write so two identical grids produce identical
/// hashes regardless of how they were reached.  The constants are the
/// canonical 64-bit FNV-1a (offset basis + prime) — fast on M-series,
/// good enough for the equality-only use we make of it (no
/// adversarial input here, only a same-process L3 hashing its own
/// terminal state).
pub(super) fn compute_publish_hash(grid: &Grid, view_offset: u16, flags: u32) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    #[inline(always)]
    fn mix(h: u64, x: u64) -> u64 {
        h.wrapping_mul(FNV_PRIME) ^ x
    }
    let cols = grid.cols();
    let rows = grid.rows();
    let (cur_c, cur_r) = grid.cursor();
    let mut h = FNV_OFFSET;
    h = mix(h, flags as u64);
    h = mix(h, cur_c as u64);
    h = mix(h, cur_r as u64);
    h = mix(h, view_offset as u64);
    h = mix(h, grid.scroll_push_count());
    h = mix(h, grid.scrollback_len() as u64);
    h = mix(h, cols as u64);
    h = mix(h, rows as u64);
    for row in 0..rows {
        for col in 0..cols {
            let cell = grid.cell_at_view(view_offset, col, row);
            h = mix(h, cell.ch as u64);
            h = mix(h, hash_cell_attrs(&cell.attrs));
            // a cluster is extended in place as codepoints join it: the
            // index stays, the text changes, and the table must go out
            if cell.is_cluster()
                && let Some(text) = grid.cluster_text(&cell)
            {
                for b in text.bytes() {
                    h = mix(h, b as u64);
                }
            }
        }
        h = mix(h, grid.wrapped_at_view(view_offset, row) as u64);
    }
    h
}

#[inline(always)]
fn hash_cell_attrs(a: &crate::grid::CellAttrs) -> u64 {
    let bools = (a.bold as u64)
        | ((a.italic as u64) << 1)
        | ((a.underline as u64) << 2)
        | ((a.reverse as u64) << 3)
        | ((a.dim as u64) << 4);
    bools
        .wrapping_add(hash_color(&a.fg) << 8)
        .wrapping_add(hash_color(&a.bg) << 32)
}

#[inline(always)]
fn hash_color(c: &crate::grid::Color) -> u64 {
    use crate::grid::ColorKind;
    match c.kind() {
        ColorKind::Default => 0,
        ColorKind::Indexed(i) => 0x100 | (i as u64),
        ColorKind::Rgb(r, g, b) => {
            0x1_0000_0000 | ((r as u64) << 16) | ((g as u64) << 8) | (b as u64)
        }
    }
}

