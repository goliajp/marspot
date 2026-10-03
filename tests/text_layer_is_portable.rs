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

/// `Shaper` used to take `&CTFont` and a callback handing back the
/// `CTFont`s CoreText found mid-shape. It takes a `FontId` now, and the
/// implementation registers fallbacks in its own table; the caller's
/// per-font snapshots catch up after the call (see `FontCache`'s test
/// that a font found while shaping is known before anyone asks).
#[test]
fn the_shaper_names_no_platform() {
    let body = trait_body(&source(), "Shaper");
    for w in PLATFORM_WORDS {
        assert!(
            !body.contains(w),
            "`Shaper` mentions {w}. An implementation on another platform \
             cannot satisfy that. Take a `FontId` and register fallbacks in \
             the implementation's own table."
        );
    }
    assert!(body.contains("fn shape"), "found something that is not the trait");
    assert!(body.contains("FontId"), "the font is identified by its id");
}

/// The glyph atlas packs and looks up on the CPU; the texture belongs to
/// whichever backend draws, behind `AtlasSink`.  A graphics API's name
/// in the atlas's source is that line being crossed again.
#[test]
fn the_glyph_atlas_names_no_graphics_api() {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/glyph_atlas.rs"),
    )
    .expect("src/glyph_atlas.rs");
    for w in GRAPHICS_WORDS {
        assert!(
            !src.contains(w),
            "src/glyph_atlas.rs mentions {w}. Pixels leave the atlas through \
             `AtlasSink::upload`; the texture lives in the backend."
        );
    }
}

/// And the check still catches what the file looked like before.
#[test]
fn the_graphics_check_catches_a_texture_field() {
    let was = "texture: Retained<ProtocolObject<dyn MTLTexture>>,";
    assert!(GRAPHICS_WORDS.iter().any(|w| was.contains(w)));
}

const GRAPHICS_WORDS: [&str; 5] = ["MTL", "objc2_metal", "ProtocolObject", "ID3D12", "VkImage"];

/// Laying out a frame turns panes, chrome and text into instances; any
/// backend draws those.  Reaching into the Metal renderer, or naming a
/// graphics API, from that code ties the layout to one backend again.
#[test]
fn the_frame_layout_names_no_backend() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/frame_build");
    let mut seen = 0;
    for entry in std::fs::read_dir(&dir).expect("src/frame_build") {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        seen += 1;
        let src = std::fs::read_to_string(&path).unwrap();
        for w in GRAPHICS_WORDS.iter().chain(&["render_metal"]) {
            assert!(!src.contains(w), "{} mentions {w}", path.display());
        }
    }
    assert!(seen >= 10, "read only {seen} files from src/frame_build");
}

/// And it still catches the import every layout file used to have.
#[test]
fn the_backend_check_catches_an_import_from_the_renderer() {
    let was = "use crate::render_metal::{rgba8_of_f32, CellInstance, GlyphInstance};";
    assert!(GRAPHICS_WORDS.iter().chain(&["render_metal"]).any(|w| was.contains(w)));
}
