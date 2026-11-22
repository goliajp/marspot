//! The bytes the GPU reads for a run of instances, encoded into a reused slab.

use super::{CellInstance, GlyphInstance, UiRectInstance};

/// SAFETY: `CellInstance` is `#[repr(C)]` with no padding, so its
/// memory representation is a flat slice of bytes.  No interior
/// uninitialised bytes; safe to view the slice as `&[u8]`.
/// The bytes the GPU reads for a run of flat rects, written by the
/// published encoder into a slab the caller reuses.
///
/// Not a reinterpret, for the same reason the rounded rects are not:
/// `RectInstance` is a plain Rust struct with an explicit `encode`, so
/// its field order in memory is the compiler's business and only
/// `encode` says what the layout is.
///
/// This is the per-frame bulk -- thousands of cells -- so the slab is
/// kept between frames and the encode writes in place.
pub(super) fn cells_as_bytes<'a>(cells: &[CellInstance], slab: &'a mut Vec<u8>) -> &'a [u8] {
    use golia_ui_core::scene::Encode;
    let need = cells.len() * CellInstance::SIZE;
    if slab.len() < need {
        slab.resize(need, 0);
    }
    for (i, c) in cells.iter().enumerate() {
        let at = i * CellInstance::SIZE;
        c.encode(&mut slab[at..at + CellInstance::SIZE]);
    }
    &slab[..need]
}

/// SAFETY: same reasoning as `cells_as_bytes` — `GlyphInstance` is
/// `#[repr(C)]` with no padding.
/// The bytes the GPU reads for a run of rounded rects, written by the
/// published encoder into a slab the caller reuses.
///
/// Not a reinterpret of the slice: `UiRectInstance` is a plain Rust
/// struct with an explicit `encode`, not a `repr(C)` mirror of a
/// shader struct, so its field order in memory is the compiler's
/// business and only `encode` says what the layout is. That is the
/// property this move was for -- one declaration of the layout, and
/// the shader reads what it writes.
///
/// The slab grows to fit and is kept between frames, so a steady state
/// allocates nothing.
pub(super) fn ui_rects_as_bytes<'a>(rects: &[UiRectInstance], slab: &'a mut Vec<u8>) -> &'a [u8] {
    use golia_ui_core::scene::Encode;
    let need = rects.len() * UiRectInstance::SIZE;
    if slab.len() < need {
        slab.resize(need, 0);
    }
    for (i, r) in rects.iter().enumerate() {
        let at = i * UiRectInstance::SIZE;
        r.encode(&mut slab[at..at + UiRectInstance::SIZE]);
    }
    &slab[..need]
}

/// The bytes the GPU reads for a run of glyphs, written by the
/// published encoder into a slab the caller reuses.
///
/// Same rule as the flat and rounded rects: `GlyphInstance` is a plain
/// Rust struct with an explicit `encode`, so only `encode` says what
/// the layout is.
pub(super) fn glyphs_as_bytes<'a>(glyphs: &[GlyphInstance], slab: &'a mut Vec<u8>) -> &'a [u8] {
    use golia_ui_core::scene::Encode;
    let need = glyphs.len() * GlyphInstance::SIZE;
    if slab.len() < need {
        slab.resize(need, 0);
    }
    for (i, g) in glyphs.iter().enumerate() {
        let at = i * GlyphInstance::SIZE;
        g.encode(&mut slab[at..at + GlyphInstance::SIZE]);
    }
    &slab[..need]
}
