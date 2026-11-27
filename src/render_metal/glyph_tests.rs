//! Glyph resolution: clusters, neighbours, the wide setting and when
//! the fonts rebuild.

use super::*;
use crate::frame_build::glyph_resolve::{resolve_cell_glyph_routed, resolve_cluster_glyph};

/// The resolver hands back an atlas entry for a cluster, and
/// caches it.
///
/// Between "CoreText draws the pair" and "the render loop asks for
/// it" sits this function, and nothing tested it: the pixel test
/// below calls the rasteriser directly, and the grid tests never
/// reach the atlas.  A cluster that failed here would simply draw
/// nothing, silently, for every cell holding one.
#[test]
fn the_resolver_caches_a_cluster_entry() {
    let Ok(mut font) = crate::font_cache::FontCache::build() else {
        eprintln!("skipping: no font stack");
        return;
    };
    let Ok(r) = MetalRenderer::new_headless() else {
        eprintln!("skipping: no Metal device");
        return;
    };
    let Ok(mut atlas) = new_atlas(&r.device, 512, 512, false, font.font_table()).map(|(a, _)| a) else {
        eprintln!("skipping: atlas would not allocate");
        return;
    };
    let metrics = SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 };

    let first = resolve_cluster_glyph(&mut atlas, &mut font, "e\u{301}", false, false, metrics);
    assert!(first.is_some(), "the resolver returned no entry for a cluster");

    // Asked again, it must be the same slot — a screen full of one
    // cluster has to cost one raster, not one per cell.
    let again = resolve_cluster_glyph(&mut atlas, &mut font, "e\u{301}", false, false, metrics);
    assert_eq!(
        format!("{:?}", first.map(|e| (e.u0, e.v0))),
        format!("{:?}", again.map(|e| (e.u0, e.v0))),
        "the same cluster took a second atlas slot"
    );

    // And a different cluster is a different slot, or the key is
    // not carrying the text.
    let other = resolve_cluster_glyph(
        &mut atlas, &mut font, "\u{1f1ef}\u{1f1f5}", false, false, metrics,
    );
    assert!(other.is_some());
    assert_ne!(
        format!("{:?}", first.map(|e| (e.u0, e.v0))),
        format!("{:?}", other.map(|e| (e.u0, e.v0))),
        "two different clusters share a slot; the key ignores the text"
    );
}

/// A cluster rasterises to something other than its base alone.
///
/// The grid now keeps `e` + U+0301 whole, and the renderer asks
/// CoreText to draw the pair.  If that path were a no-op — wrong
/// font, empty line, baseline off the bitmap — the cell would
/// simply look like `e` and every test above would still pass,
/// because they are all about the grid.  This compares pixels.
#[test]
fn a_cluster_draws_differently_from_its_base() {
    let Ok(mut font) = crate::font_cache::FontCache::build() else {
        eprintln!("skipping: no font stack");
        return;
    };
    let metrics = SlotMetrics { cell_w: 16, cell_h: 32, baseline_from_top: 24 };
    let mut buf_base = vec![0u8; 16 * 32];
    let mut buf_pair = vec![0u8; 16 * 32];

    let (idx, _) = font.resolve_char('e', false, false);
    let ct = font.font(idx).expect("the resolved font is in the table");
    assert!(
        crate::glyph_atlas::rasterise_cluster("e", &ct, metrics, 1, &mut buf_base),
        "the rasteriser refused a one-codepoint cluster"
    );
    assert!(
        crate::glyph_atlas::rasterise_cluster("e\u{301}", &ct, metrics, 1, &mut buf_pair),
        "the rasteriser refused the pair"
    );

    let ink_base: u32 = buf_base.iter().map(|b| *b as u32).sum();
    let ink_pair: u32 = buf_pair.iter().map(|b| *b as u32).sum();
    assert!(ink_base > 0, "even the base drew nothing; the path is dead");
    assert!(
        ink_pair > ink_base,
        "the mark added no ink: base {ink_base}, pair {ink_pair}"
    );

    // And the mark sits above the letter, which is the whole
    // point of letting CoreText place it rather than stacking two
    // rasters.  Row order is taken from the data, not from an
    // argument about Core Graphics' origin: the letter's own ink
    // says which way is up, and the mark has to be on the other
    // side of its first row.
    let row_ink = |b: &[u8]| -> Vec<u32> {
        (0..32)
            .map(|r| b[r * 16..(r + 1) * 16].iter().map(|v| *v as u32).sum())
            .collect()
    };
    let base_rows = row_ink(&buf_base);
    let pair_rows = row_ink(&buf_pair);
    let base_first = base_rows.iter().position(|v| *v > 0).expect("the base drew nothing");
    let added: Vec<usize> = (0..32).filter(|r| pair_rows[*r] > base_rows[*r]).collect();
    assert!(!added.is_empty(), "the pair added ink to no row");
    assert!(
        added.iter().all(|r| *r < base_first),
        "the mark is not clear of the letter: the letter starts at row \
         {base_first} and the added rows are {added:?}"
    );
    assert!(
        added.iter().all(|r| base_rows[*r] == 0),
        "the mark landed on top of the letter rather than beside it"
    );
}

/// 2026-08-08, two reports one after the other.
///
/// The first: `①` renders at a fraction of the CJK beside it.
/// Measured cause — PingFang draws it 11.71 px wide against a
/// 7.20 px cell, so `rasterise_glyph` scale-to-fits it to 61 %, and
/// because circled digits are square the *width* always binds: no
/// font on the machine escapes it.  The fix taken was WezTerm's
/// `allow_square_glyphs_to_overflow_width` = `WhenFollowedBySpace`:
/// spill into the next cell when it is blank.
///
/// The second, on that build: *圈圈文字先小后大…要么全大要么全小.*
/// And that is what the rule guarantees — `① ` is full size, `①消`
/// is 61 %, so the same character changes size with whatever gets
/// written next to it.  Whichever size you prefer, watching one
/// turn into the other reads as a fault.
///
/// So: **a cell's glyph does not depend on its neighbours.**  One
/// cell means scaled to one cell.  Full size costs a second cell
/// and moves the wrap point, which is a real decision with a real
/// downside — so it is a setting, not a guess made per character.
#[test]
fn a_glyph_is_drawn_the_same_whatever_sits_next_to_it() {
    let device = match system_default_device() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let mut font = FontCache::build().expect("font");
    let mut atlas =
        new_atlas(&device, 512, 512, false, font.font_table()).map(|(a, _)| a).expect("atlas");
    let mut color_atlas =
        new_atlas(&device, 64, 64, true, font.font_table()).map(|(a, _)| a).expect("color atlas");
    let metrics = SlotMetrics { cell_w: 7, cell_h: 16, baseline_from_top: 12 };

    // The set the old rule fired on: narrow Ambiguous glyphs and
    // the circled family.  Each is rasterised once, and asking for
    // it again — whatever the line around it looks like — must give
    // the identical slot, because there is nothing left to vary.
    for ch in ['★', '☆', '●', '▲', '▼', '①', '③', 'Ⓐ', '⓪', '❶'] {
        assert_eq!(crate::grid::char_width(ch), 1, "{ch} is drawn narrow");
        let first = resolve_cell_glyph_routed(
            &mut atlas, &mut color_atlas, &mut font, ch, false, false, metrics,
        );
        let again = resolve_cell_glyph_routed(
            &mut atlas, &mut color_atlas, &mut font, ch, false, false, metrics,
        );
        match (first, again) {
            (Some((a, _)), Some((b, _))) => assert_eq!(
                (a.u0, a.v0, a.u1, a.v1),
                (b.u0, b.v0, b.u1, b.v1),
                "{ch} rasterised to two different slots",
            ),
            (None, None) => {}
            _ => panic!("{ch}: resolved once and not the other time"),
        }
    }
}

/// The way to get circled digits at full size is the setting, and
/// it works by giving them a second cell — not by borrowing one.
#[test]
fn the_wide_setting_is_what_makes_circled_digits_full_size() {
    crate::settings::set_for_test(crate::settings::Settings {
        appearance_circled_wide: false,
        ..Default::default()
    });
    for ch in ['①', '⑨', 'Ⓐ', '⓪', '❶'] {
        assert_eq!(crate::grid::char_width(ch), 1, "{ch} off");
    }
    crate::settings::set_for_test(crate::settings::Settings {
        appearance_circled_wide: true,
        ..Default::default()
    });
    for ch in ['①', '⑨', 'Ⓐ', '⓪', '❶'] {
        assert_eq!(crate::grid::char_width(ch), 2, "{ch} on");
    }
    // …and it is a decision about the *grid*, so it is the same
    // decision wherever the character appears — never per-neighbour.
    crate::settings::set_for_test(crate::settings::Settings::default());
}

/// Rebuilding the fonts is what makes a display swap land, and it
/// must be a no-op when nothing moved — it runs on every attach,
/// and throwing both atlases away per resize step would make a
/// window drag re-rasterise the screen continuously.
#[test]
fn fonts_rebuild_only_when_the_scale_actually_moves() {
    let saved = crate::ui::chrome_scale();
    let mut r = match MetalRenderer::new_headless() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("SKIPPED: no Metal device: {e}");
            return;
        }
    };
    let (w0, h0) = r.cell_dims();

    assert!(!r.rebuild_fonts_if_stale(), "nothing moved");
    assert_eq!(r.cell_dims(), (w0, h0));

    crate::ui::set_chrome_scale(2.0);
    assert!(r.rebuild_fonts_if_stale(), "a new density rebuilds");
    let (w2, h2) = r.cell_dims();
    assert!(
        (w2 - w0 * 2.0).abs() < 0.5 && (h2 - h0 * 2.0).abs() < 0.5,
        "the cell has to double with the density: {w0:.2}x{h0:.2} -> {w2:.2}x{h2:.2}",
    );
    assert!(!r.rebuild_fonts_if_stale(), "and settle");

    crate::ui::set_chrome_scale(saved);
    assert!(r.rebuild_fonts_if_stale());
    let (w1, h1) = r.cell_dims();
    assert!(
        (w1 - w0).abs() < 0.01 && (h1 - h0).abs() < 0.01,
        "and come back exactly: {w0:.2}x{h0:.2} -> {w1:.2}x{h1:.2}",
    );
}
