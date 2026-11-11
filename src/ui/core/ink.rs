//! How many rows of pixels a run of interface text has ink on, above
//! and below its baseline.
//!
//! A label in Latin letters is centred by its capitals: their height
//! is one number for the whole face.  A label in Han or kana has no
//! such number — the characters fill a square, and how much of it
//! differs from one face to the next and from one character to the
//! next, by more than a pixel at interface sizes — so it is centred on
//! the ink it actually has.
//!
//! The rows are read off the masks the glyphs are drawn from, not off
//! their outlines: an outline's edge that covers a third of a row is
//! not seen as a row of ink, and a label placed by it sits a pixel off.

use crate::font_cache::FontCache;
use marspot_term::fast_hash::{FxHashMap, FxHasher};
use std::cell::RefCell;
use std::hash::{Hash, Hasher};

/// Runs remembered before the table is emptied: a panel's labels many
/// times over.
const REMEMBERED: usize = 512;
/// A pixel at least this covered is ink.
const INK: u8 = 128;

thread_local! {
    static ROWS: RefCell<FxHashMap<u64, (f32, f32)>> = RefCell::new(FxHashMap::default());
}

/// `(above, below)` the baseline for `s` at `size_pt` and `weight`, in
/// whole rows, or `None` for a run that draws nothing.
pub(crate) fn rows(font: &mut FontCache, s: &str, size_pt: f64, weight: u16) -> Option<(f32, f32)> {
    let mut hasher = FxHasher::default();
    (s, size_pt.to_bits(), weight, crate::ui::px_per_pt().to_bits()).hash(&mut hasher);
    let key = hasher.finish();
    if let Some(known) = ROWS.with_borrow(|m| m.get(&key).copied()) {
        return Some(known);
    }
    let shaped = font.shape_ui_weighted_opts_at_size(s, weight, crate::font_shape::ShapeOptions::default(), size_pt);
    let fonts = font.font_table();
    let (mut above, mut below) = (i32::MIN, i32::MIN);
    for glyph in &shaped {
        let Some(face) = fonts.get(glyph.font_id as usize) else { continue };
        let Some(mask) = crate::glyph_atlas::rasterise_glyph_natural(&face, glyph.glyph_id, 0) else { continue };
        let mut inked = mask
            .bytes
            .chunks_exact(mask.px_w as usize)
            .enumerate()
            .filter(|(_, row)| row.iter().any(|a| *a >= INK))
            .map(|(i, _)| i as i32);
        let Some(first) = inked.next() else { continue };
        let last = inked.next_back().unwrap_or(first);
        // the mask's top row is `bearing_y` rows above the baseline
        above = above.max(mask.bearing_y as i32 - first);
        below = below.max(last + 1 - mask.bearing_y as i32);
    }
    if above == i32::MIN {
        return None;
    }
    let rows = (above as f32, below as f32);
    ROWS.with_borrow_mut(|m| {
        if m.len() >= REMEMBERED {
            m.clear();
        }
        m.insert(key, rows);
    });
    Some(rows)
}
