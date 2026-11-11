//! The chrome's icons: SF Symbols, the system's own set.
//!
//! One table names every icon the chrome draws.  A symbol is
//! rasterised by AppKit to a coverage mask at the exact pixel size it
//! is drawn at, kept in the glyph atlas beside the font's glyphs and
//! tinted like one, so an icon follows the theme the way text does and
//! the atlas's eviction bounds it.
//!
//! A mask is rasterised on its first use at a size and never on a
//! frame after that.

mod raster;

use marspot_term::layout::Rect;

use crate::frame_build::color::rgba8_of_f32;
use crate::glyph_atlas::{FontId, GlyphKey, BOX_DRAWING_FONT_ID};
use crate::ui::core::ViewPainter;
use golia_ui_core::scene::GlyphInstance;

/// Every icon in the chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Symbol {
    /// Show or hide the sidebar.
    Sidebar,
    /// The pane layout picker.
    Layout,
    /// The process monitor.
    Processes,
    /// The accounts' usage.
    Usage,
    /// Settings.
    Settings,
    /// Add: a pane, a seat.
    Plus,
    /// Close: a pane, a panel, the search bar; end a process.
    Close,
}

impl Symbol {
    pub const ALL: [Symbol; 7] = [
        Symbol::Sidebar,
        Symbol::Layout,
        Symbol::Processes,
        Symbol::Usage,
        Symbol::Settings,
        Symbol::Plus,
        Symbol::Close,
    ];

    /// The symbol's name in SF Symbols.  Every one of these has been
    /// in the set since macOS 11.
    pub const fn name(self) -> &'static str {
        match self {
            Symbol::Sidebar => "sidebar.left",
            Symbol::Layout => "rectangle.split.2x2",
            Symbol::Processes => "list.bullet.indent",
            Symbol::Usage => "chart.bar",
            Symbol::Settings => "slider.horizontal.3",
            Symbol::Plus => "plus",
            Symbol::Close => "xmark",
        }
    }
}

/// The ink of `plus` and `xmark` is this fraction of the point size
/// across: what a caller that wants a mark of a given width divides by.
pub const MARK_INK: f64 = 0.8;

/// The atlas's font field for a symbol's mask, beside the one the
/// box-drawing masks take: no font cache index reaches either.
const SYMBOL_FONT_ID: FontId = BOX_DRAWING_FONT_ID - 1;

/// Side of the square mask a symbol at `point_px` is drawn into.  The
/// widest symbol in the table is 1.3 point sizes across; even, so the
/// mask centres on a whole pixel in a button of an even size.
fn mask_side(point_px: u32) -> u32 {
    2 * (point_px * 7).div_ceil(8)
}

/// Have AppKit load the symbol set, on a thread of its own.
///
/// The first symbol a process asks for costs it the set itself, a
/// tenth of a second; every one after that is drawn in well under a
/// millisecond.  Called when the display's scale is adopted, before
/// there is a renderer, so the first frame does not pay it.
pub fn warm() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            const POINT_PX: u32 = 13;
            let side = mask_side(POINT_PX);
            let mut mask = [0u8; 24 * 24];
            for symbol in Symbol::ALL {
                raster::rasterise(symbol.name(), POINT_PX, side, &mut mask);
            }
        });
    });
}

/// `symbol` at a point size of `point_px` physical pixels, centred on
/// `rect` by its alignment rect, in `color`.
///
/// The mask lands on whole pixels: its strokes were placed on the
/// pixel grid when it was rasterised and stay there.
pub fn paint(p: &mut ViewPainter, symbol: Symbol, rect: Rect, point_px: f64, color: [f32; 4]) {
    let point_px = point_px.round().max(1.0) as u32;
    let side = mask_side(point_px);
    // one weight for the whole set, so the key is the symbol and its size
    let key = GlyphKey::new(SYMBOL_FONT_ID, symbol as u16, point_px as u16, 0, 0);
    let Some(entry) = p.atlas.get_or_insert_custom_raster(key, side, side, 0, 1, |mask| {
        raster::rasterise(symbol.name(), point_px, side, mask);
    }) else {
        return;
    };
    let side = side as f64;
    p.glyphs.push(GlyphInstance {
        origin: [
            (rect.x + (rect.w - side) * 0.5).round() as f32,
            (rect.y_top + (rect.h - side) * 0.5).round() as f32,
        ],
        size: [side as f32, side as f32],
        uv0: [entry.u0 as f32 / p.atlas_w, entry.v0 as f32 / p.atlas_h],
        uv1: [entry.u1 as f32 / p.atlas_w, entry.v1 as f32 / p.atlas_h],
        color: rgba8_of_f32(color),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mask(symbol: Symbol, point_px: u32) -> (Vec<u8>, usize) {
        let side = mask_side(point_px);
        let mut mask = vec![0u8; (side * side) as usize];
        raster::rasterise(symbol.name(), point_px, side, &mut mask);
        (mask, side as usize)
    }

    /// `(left, top, right, bottom)` of the pixels at least half covered,
    /// the last two exclusive.
    fn ink(mask: &[u8], side: usize) -> (usize, usize, usize, usize) {
        let on = |x: usize, y: usize| mask[y * side + x] >= 128;
        let cols: Vec<usize> = (0..side).filter(|&x| (0..side).any(|y| on(x, y))).collect();
        let rows: Vec<usize> = (0..side).filter(|&y| (0..side).any(|x| on(x, y))).collect();
        (cols[0], rows[0], cols[cols.len() - 1] + 1, rows[rows.len() - 1] + 1)
    }

    #[test]
    fn every_symbol_in_the_table_resolves_and_fits_its_mask() {
        let started = std::time::Instant::now();
        let (first, _) = mask(Symbol::Sidebar, 13);
        let first_ms = started.elapsed().as_secs_f64() * 1e3;
        assert!(first.iter().any(|&a| a > 0), "sidebar.left drew nothing");
        for point_px in [13, 26] {
            let started = std::time::Instant::now();
            for symbol in Symbol::ALL {
                let (mask, side) = mask(symbol, point_px);
                assert!(
                    mask.iter().any(|&a| a >= 128),
                    "{} drew nothing at {point_px} px",
                    symbol.name()
                );
                let edge = (0..side).any(|i| {
                    [i, (side - 1) * side + i, i * side, i * side + side - 1]
                        .into_iter()
                        .any(|at| mask[at] > 0)
                });
                assert!(!edge, "{} is cut off by its mask at {point_px} px", symbol.name());
            }
            eprintln!(
                "the table of {} symbols at {point_px} px: {:.2} ms",
                Symbol::ALL.len(),
                started.elapsed().as_secs_f64() * 1e3
            );
        }
        eprintln!("the first symbol this process drew: {first_ms:.2} ms");
    }

    /// A stroke is crisp when a line across it meets a pixel it covers
    /// at least nine parts in ten.  A stroke that straddles two pixels
    /// covers neither that far: it is two rows of grey.  Held for the
    /// upright strokes of the sidebar's frame (a row through the middle
    /// of the ink) and for its level ones (a column through the middle),
    /// at display scale 1 and 2.
    #[test]
    fn the_sidebar_symbol_has_crisp_strokes_at_both_scales() {
        const CRISP: u8 = 230;
        for point_px in [13, 26] {
            let (mask, side) = mask(Symbol::Sidebar, point_px);
            let (left, top, right, bottom) = ink(&mask, side);
            let row = (top + bottom) / 2;
            let upright = (left..right).map(|x| mask[row * side + x]).max().unwrap();
            let col = (left + right) / 2;
            let level = (top..bottom).map(|y| mask[y * side + col]).max().unwrap();
            assert!(
                upright >= CRISP && level >= CRISP,
                "at {point_px} px the frame's strokes peak at {upright} (upright) and {level} (level) of 255",
            );
        }
    }

    #[test]
    fn a_mask_is_centred_on_the_symbols_alignment_rect() {
        // the alignment rect is where the system says the symbol's
        // optical middle is, which is not quite its ink's: the frame of
        // the sidebar symbol sits within an eighth of a point size of
        // the middle of the mask, on both axes
        for point_px in [13usize, 26] {
            let (mask, side) = mask(Symbol::Sidebar, point_px as u32);
            let (left, top, right, bottom) = ink(&mask, side);
            assert!(left.abs_diff(side - right) <= point_px / 8, "{left} | {}", side - right);
            assert!(top.abs_diff(side - bottom) <= point_px / 8, "{top} | {}", side - bottom);
        }
    }

    #[test]
    fn a_mask_holds_the_widest_symbol_at_any_size() {
        for point_px in 5..=64 {
            assert!(mask_side(point_px) as f64 >= 1.3 * point_px as f64 + 2.0);
            assert_eq!(mask_side(point_px) % 2, 0);
        }
    }
}
