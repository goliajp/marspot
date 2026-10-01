//! The text layer's traits must not name a platform.
//!
//! `Rasteriser` used to take `&CTFont` and `CGGlyph`. A DirectWrite or
//! FreeType implementation could not be written against that signature
//! at all -- not "would be awkward": it would have had to produce a
//! CoreText font object to satisfy the type. v1 is three platforms, so
//! that signature was the floor under the whole thing.
//!
//! This reads the source rather than the types, because the thing being
//! asserted is about the *declaration*: a trait can be perfectly
//! portable in its types and still be written in terms of a platform's
//! vocabulary, and the next person adding a method is who this is for.

use std::path::Path;

/// The body of `pub trait <name>`, from the `{` to its matching `}`.
fn trait_body(src: &str, name: &str) -> String {
    let needle = format!("pub trait {name}");
    let start = src
        .find(&needle)
        .unwrap_or_else(|| panic!("no `{needle}` in the file -- did it get renamed?"));
    let open = src[start..]
        .find('{')
        .expect("a trait declaration has a brace")
        + start;
    let mut depth = 0usize;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return src[open..open + i + 1].to_string();
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces in `{name}`");
}

const PLATFORM_WORDS: [&str; 6] = [
    "CTFont",
    "CGGlyph",
    "core_text",
    "core_graphics",
    "IDWrite",
    "FT_Face",
];

fn source() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/font_trait.rs"))
        .expect("src/font_trait.rs")
}

/// The instrument has to be able to fail, and the only honest proof of
/// that is that it still catches the shape the code had yesterday.
#[test]
fn the_check_catches_a_signature_that_names_a_platform() {
    let was = r#"
pub trait Rasteriser: Send + Sync + 'static {
    fn rasterise(&self, font: &CTFont, glyph: CGGlyph, subpx_x: u8) -> Option<RasterOutput>;
}
"#;
    let body = trait_body(was, "Rasteriser");
    let hits: Vec<_> = PLATFORM_WORDS.iter().filter(|w| body.contains(**w)).collect();
    assert_eq!(hits.len(), 2, "the old signature named CTFont and CGGlyph");
}

#[test]
fn the_rasteriser_names_no_platform() {
    let body = trait_body(&source(), "Rasteriser");
    for w in PLATFORM_WORDS {
        assert!(
            !body.contains(w),
            "`Rasteriser` mentions {w}. An implementation on another \
             platform cannot satisfy that. The font is identified by \
             `FontId`; look it up in the implementation's own table."
        );
    }
    // Not a tautology: the body has to actually be the trait.
    assert!(body.contains("fn rasterise"), "found something that is not the trait");
    assert!(body.contains("FontId"), "the font is identified by its id");
}

/// `Shaper` is **not** portable yet, and this pins that rather than
/// leaving it to be discovered.
///
/// It takes `&CTFont` plus an intern callback handing back `CTFont`s
/// that CoreText found mid-shape. Closing it means the font table owns
/// the name dedup, which puts the per-cell `is_color_font` read behind
/// that table's lock -- a cost that wants measuring, so it is a
/// separate change.
///
/// When someone does it, this test fails and tells them to move the
/// name into the list above. That is the point: the gap cannot widen
/// quietly and cannot be forgotten.
#[test]
fn the_shaper_is_still_not_portable_and_this_is_the_record_of_it() {
    let body = trait_body(&source(), "Shaper");
    let hits: Vec<&str> = PLATFORM_WORDS
        .iter()
        .copied()
        .filter(|w| body.contains(w))
        .collect();
    assert_eq!(
        hits,
        ["CTFont"],
        "`Shaper`'s remaining platform coupling changed. If it is now \
         clean, move it to `the_rasteriser_names_no_platform` and delete \
         this test; if something new crept in, that is a step backwards."
    );
}
