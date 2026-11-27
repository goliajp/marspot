//! The masks the unit tests draw icons with, as AppKit drew them.
//!
//! Loading a system symbol registers the process with LaunchServices,
//! and the suite runs every test in a process of its own: drawn live,
//! each test that paints the chrome was one more app registration on
//! the runner.  So the tests read the masks from a file AppKit wrote,
//! and the live checks below compare that file with what AppKit draws
//! now.  They are ignored in the suite; `bin/appkit-fixtures.sh` runs
//! them (with `--write`, rewrites the file).

use super::{mask_side, Symbol};

/// `symbol u8, point_px u16 le, side u16 le`, then `side * side` bytes
/// of coverage, repeated.
static MASKS: &[u8] = include_bytes!("../../../../../tests/fixtures/appkit/icon-masks.bin");

/// The sizes the chrome's tests draw at: every symbol at the two
/// toolbar sizes, the marks at the sizes of every close and add button.
fn sizes() -> Vec<(Symbol, u32)> {
    let mut out = Vec::new();
    for symbol in Symbol::ALL {
        let marks: &[u32] = if matches!(symbol, Symbol::Close | Symbol::Plus) { &[3, 8, 16, 20, 23, 40, 45] } else { &[] };
        for &px in [13, 26].iter().chain(marks) {
            out.push((symbol, px));
        }
    }
    out
}

/// Fill `mask` with the recorded mask of `symbol` at `point_px`.
pub(super) fn draw(symbol: Symbol, point_px: u32, side: u32, mask: &mut [u8]) {
    let mut at = 0;
    while at + 5 <= MASKS.len() {
        let sym = MASKS[at];
        let px = u16::from_le_bytes([MASKS[at + 1], MASKS[at + 2]]) as u32;
        let s = u16::from_le_bytes([MASKS[at + 3], MASKS[at + 4]]) as usize;
        let body = &MASKS[at + 5..at + 5 + s * s];
        if sym == symbol as u8 && px == point_px {
            assert_eq!(s, side as usize, "the recorded {} at {point_px} px has another side", symbol.name());
            mask.copy_from_slice(body);
            return;
        }
        at += 5 + s * s;
    }
    panic!(
        "no recorded mask for {} at {point_px} px: add the size in icons/fixture.rs and run bin/appkit-fixtures.sh --write",
        symbol.name()
    );
}

/// What AppKit draws now, for every size the tests use, in the file's
/// format.
fn live() -> Vec<u8> {
    let mut out = Vec::new();
    for (symbol, px) in sizes() {
        let side = mask_side(px);
        let mut mask = vec![0u8; (side * side) as usize];
        super::raster::rasterise(symbol.name(), px, side, &mut mask);
        out.push(symbol as u8);
        out.extend_from_slice(&(px as u16).to_le_bytes());
        out.extend_from_slice(&(side as u16).to_le_bytes());
        out.extend_from_slice(&mask);
    }
    out
}

#[test]
#[ignore = "registers the test process with LaunchServices: run by bin/appkit-fixtures.sh"]
fn live_appkit_icon_masks_are_the_recorded_ones() {
    let now = live();
    if std::env::var_os("MARSPOT_TEST_WRITE_APPKIT_FIXTURES").is_some() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/appkit/icon-masks.bin");
        std::fs::write(path, &now).expect("write the masks");
        return;
    }
    assert!(now == MASKS, "AppKit draws the icons differently now: run bin/appkit-fixtures.sh --write and review");
}
