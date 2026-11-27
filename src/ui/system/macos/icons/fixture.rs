//! Masks as AppKit drew them, recorded.
//!
//! Loading a system symbol registers the process with LaunchServices.
//! The test suite runs every test in a process of its own, and the
//! runner starts the app hundreds of times a day: drawn live, each was
//! one more app registration there.  So the lib's unit tests read the
//! masks from a file AppKit wrote, and any other process given that
//! file in [`RECORDED_ENV`] (the test runs, the sandbox) takes the
//! masks it holds from there.  The live check below compares the file
//! with what AppKit draws now; it is ignored in the suite and run by
//! `bin/appkit-fixtures.sh` (with `--write`, it rewrites the file).

#[cfg(test)]
use super::{mask_side, Symbol};

/// Names a file of recorded masks; set only by tests and the sandbox.
pub const RECORDED_ENV: &str = "MARSPOT_TEST_ICON_MASKS";

/// The mask of `symbol` at `point_px` in a recorded file, and its side:
/// `symbol u8, point_px u16 le, side u16 le`, then `side * side` bytes
/// of coverage, repeated.  A torn entry ends the search.
pub fn find(file: &[u8], symbol: u16, point_px: u16) -> Option<(u16, &[u8])> {
    let mut at = 0;
    while at + 5 <= file.len() {
        let key = (file[at] as u16, u16::from_le_bytes([file[at + 1], file[at + 2]]));
        let side = u16::from_le_bytes([file[at + 3], file[at + 4]]);
        let mask = file.get(at + 5..at + 5 + side as usize * side as usize)?;
        if key == (symbol, point_px) {
            return Some((side, mask));
        }
        at += 5 + mask.len();
    }
    None
}

/// Fill `mask` from the file [`RECORDED_ENV`] names; false when there
/// is none, or it has no such mask.
#[cfg(not(test))]
pub(super) fn draw_recorded(symbol: u16, point_px: u32, side: u32, mask: &mut [u8]) -> bool {
    static FILE: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    let file = FILE.get_or_init(|| std::env::var_os(RECORDED_ENV).and_then(|p| std::fs::read(p).ok()).unwrap_or_default());
    match find(file, symbol, point_px as u16) {
        Some((s, found)) if s as u32 == side => {
            mask.copy_from_slice(found);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
static MASKS: &[u8] = include_bytes!("../../../../../tests/fixtures/appkit/icon-masks.bin");

/// The sizes the chrome's tests draw at: every symbol at the two
/// toolbar sizes, the marks at the sizes of every close and add button.
#[cfg(test)]
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
#[cfg(test)]
pub(super) fn draw(symbol: Symbol, point_px: u32, side: u32, mask: &mut [u8]) {
    let Some((s, found)) = find(MASKS, symbol as u16, point_px as u16) else {
        panic!(
            "no recorded mask for {} at {point_px} px: add the size in icons/fixture.rs and run bin/appkit-fixtures.sh --write",
            symbol.name()
        );
    };
    assert_eq!(s as u32, side, "the recorded {} at {point_px} px has another side", symbol.name());
    mask.copy_from_slice(found);
}

/// What AppKit draws now, for every size the tests use, in the file's
/// format.
#[cfg(test)]
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

#[test]
fn a_recorded_mask_is_found_by_symbol_and_size_and_a_torn_file_has_none() {
    let (side, mask) = find(MASKS, Symbol::Plus as u16, 26).expect("plus at 26 px");
    assert_eq!(side as u32, mask_side(26));
    assert!(mask.iter().any(|&a| a > 0));
    assert!(find(MASKS, Symbol::Plus as u16, 99).is_none());
    assert!(find(&MASKS[..10], Symbol::Sidebar as u16, 13).is_none());
}
