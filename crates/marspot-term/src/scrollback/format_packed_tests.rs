use super::*;
use crate::grid::{CLUSTER_INDEX_BASE, CellAttrs, Color, FLAG_CLUSTER, Grid};
use crate::scrollback::format::encode_packed_with;

fn next(s: &mut u64) -> u64 {
    *s ^= *s << 13;
    *s ^= *s >> 7;
    *s ^= *s << 17;
    *s
}

fn random_cell(s: &mut u64, palette: u64) -> Cell {
    let r = next(s);
    let ch = match r % 7 {
        0 => ' ',
        1 => '\0',
        2 => char::from_u32(0x4E00 + (r >> 8) as u32 % 500).unwrap(),
        3 => char::from_u32(0x80 + (r >> 8) as u32 % 0x700).unwrap(),
        _ => (b'!' + (r >> 8) as u8 % 90) as char,
    };
    let mut c = Cell::from(ch);
    let k = next(s) % palette;
    if k != 0 {
        c.attrs.fg = Color::indexed(k as u8);
        c.attrs.bg = if k.is_multiple_of(3) { Color::rgb(k as u8, 1, 2) } else { Color::DEFAULT };
        c.attrs.bold = k.is_multiple_of(2);
        c.attrs.dim = k.is_multiple_of(5);
    }
    if r.is_multiple_of(97) {
        // a cell that still says it is a cluster: what reaches the
        // file has to say so too, whatever its character
        c.ch = char::from_u32(CLUSTER_INDEX_BASE + 3).unwrap();
        c.attrs._pad[0] = FLAG_CLUSTER;
    }
    c
}

/// For any row, under any true hint, the record body filed from the
/// grid's own cells is byte for byte the one filed from the same row
/// expanded.
#[test]
fn a_row_files_the_same_bytes_from_either_form() {
    let mut s = 0x5EED_0123_4567_89ABu64;
    let cols = 61u16;
    let (mut shared, mut positional) = (0, 0);
    for round in 0..if cfg!(miri) { 40 } else { 4000 } {
        // a fresh grid each time, so both numberings come round
        let mut g = Grid::new(cols, 1);
        let palette = [1u64, 2, 4, 40, 400][round % 5];
        let tail = (next(&mut s) % cols as u64) as usize;
        let mut row: Vec<Cell> = (0..cols).map(|_| random_cell(&mut s, palette)).collect();
        for c in &mut row[tail..] {
            *c = if next(&mut s).is_multiple_of(4) { Cell::from((b'a' + (round % 26) as u8) as char) } else { Cell::default() };
        }
        for (c, cell) in row.iter().enumerate() {
            g.set_cell(c as u16, 0, *cell);
        }
        let packed = g.live_row_at_view(0, 0).expect("a live row");
        if g.styles().is_positional() {
            positional += 1;
        } else {
            shared += 1;
        }
        let plain_from = row.iter().rposition(|c| c.attrs != CellAttrs::default()).map_or(0, |i| i + 1);
        let wide = row.iter().any(|c| c.ch as u32 >= 0x80);
        for hint in [
            RowHint::NONE,
            RowHint { plain_from: plain_from as u16, wide },
            RowHint { plain_from: cols, wide: !wide },
            RowHint { plain_from: plain_from as u16, wide: true },
        ] {
            let (mut want, mut got) = (Vec::new(), Vec::new());
            let a = encode_packed_with(&row, hint, &mut want);
            let b = encode_packed_cells(packed, g.styles(), hint, &mut got);
            assert_eq!(a, b, "round {round}: packed or not");
            assert_eq!(want, got, "round {round}, {hint:?}");
        }
        let mut got = Vec::new();
        append_cells(packed, g.styles(), &mut got);
        assert_eq!(Cell::slice_as_bytes(&row), &got[..], "round {round}: cells as they are");
    }
    assert!(shared > 0 && positional > 0, "{shared} shared rows, {positional} positional");
}
