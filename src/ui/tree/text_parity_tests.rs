//! Text through a tree node lands where the painters put it.
//!
//! The same string is drawn twice into empty buffers: by the painter
//! a panel uses today, and as a text node of a tree laid out in the
//! same box.  The glyph instances have to be equal: the same origins,
//! sizes and atlas entries.  That is asked for every panel text role,
//! strings in Latin, kana and Han, boxes on whole and on half pixels,
//! in each interface language and at display scale 1 and 2.

use super::{FontMeasure, Tone, TreeOut, font_of, metrics};
use crate::font_cache::FontCache;
use crate::glyph_atlas::GlyphAtlas;
use crate::ui::components::controls;
use crate::ui::components::density_kit::at_scale;
use crate::ui::core::ViewPainter;
use crate::ui::strings::{Msg, t};
use crate::ui::theme::spec::control as c;
use crate::ui::theme::PanelText;
use golia_ui_core::scene::GlyphInstance;
use golia_ui_core::{Align, Constraints, FontSpec, Interaction, Layout, Length, TextFont, Tree, paint};
use marspot_term::language::LangId;
use marspot_term::layout::{Alignment, Rect};

const ROLES: [PanelText; 6] = [
    PanelText::Title,
    PanelText::Section,
    PanelText::Label,
    PanelText::Item,
    PanelText::Secondary,
    PanelText::Caption,
];

/// The fonts of the labels the controls draw themselves.
fn control_fonts() -> [(f64, u16); 4] {
    [
        (c::BUTTON_TEXT.pt(), c::BUTTON_TEXT.weight()),
        (c::SEGMENT_TEXT.pt(), c::SEGMENT_TEXT.weight()),
        (c::CHIP_TEXT.pt(), c::CHIP_WEIGHT),
        (c::KEYCAP_TEXT.pt(), c::KEYCAP_WEIGHT),
    ]
}

const ALIGNS: [(Alignment, Align); 5] = [
    (Alignment::TopLeft, Align::TopLeading),
    (Alignment::CenterLeft, Align::Leading),
    (Alignment::Center, Align::Center),
    (Alignment::CenterRight, Align::Trailing),
    (Alignment::BottomRight, Align::BottomTrailing),
];

/// Catalogue strings in the language in force, and the three scripts
/// whatever it is.
fn strings() -> Vec<&'static str> {
    let mut all: Vec<&'static str> = [
        Msg::PanelSettings,
        Msg::ReclaimIdlePanes,
        Msg::CostReclaimIdlePanes,
        Msg::WheelSpeed,
        Msg::ScrollFaster,
        Msg::LayoutApply,
        Msg::LangEnglish,
        Msg::LangJapanese,
        Msg::LangChinese,
    ]
    .map(t)
    .to_vec();
    all.extend(["Typography gjpqy", "アイドル状態", "直骨海", "Theme テーマ 主题", "⇧⌘L", "+"]);
    all
}

struct Bench {
    font: FontCache,
    atlas: GlyphAtlas,
}

impl Bench {
    fn new() -> Option<Self> {
        let Ok(device) = crate::render_metal::system_default_device() else {
            eprintln!("SKIPPED: no Metal device");
            return None;
        };
        let font = FontCache::build().expect("font");
        let atlas = crate::render_metal::new_atlas(&device, 2048, 2048, false, font.font_table())
            .map(|(a, _)| a)
            .expect("atlas");
        Some(Self { font, atlas })
    }

    /// The glyphs `draw` pushes into empty buffers.
    fn glyphs(&mut self, draw: impl FnOnce(&mut ViewPainter<'_>)) -> Vec<GlyphInstance> {
        let (atlas_w, atlas_h) = self.atlas.dims();
        let (mut cells, mut glyphs, mut ui_rects) = (Vec::new(), Vec::new(), Vec::new());
        let (cell_w, cell_h, ascent) =
            (self.font.cell_w as f32, self.font.cell_h as f32, self.font.ascent as f32);
        draw(&mut ViewPainter {
            cell_w,
            cell_h,
            ascent,
            atlas_w: atlas_w as f32,
            atlas_h: atlas_h as f32,
            window_w: 4000.0,
            window_h: 4000.0,
            font: &mut self.font,
            atlas: &mut self.atlas,
            cells: &mut cells,
            glyphs: &mut glyphs,
            ui_rects: &mut ui_rects,
        });
        assert!(cells.is_empty() && ui_rects.is_empty(), "text drew a rectangle");
        glyphs
    }
}

/// `s` as the one node of a tree that fills `rect`.
fn through_a_node(p: &mut ViewPainter<'_>, rect: Rect, s: &str, font: TextFont, align: Align, by_ink: bool) {
    let mut tree = Tree::new();
    tree.build(|ui| {
        let node = ui
            .text_copy(font, Tone::Fg.into(), s)
            .width(Length::Px(rect.w))
            .height(Length::Px(rect.h))
            .align(align);
        if by_ink {
            node.by_ink();
        }
    });
    let m = metrics(p.font);
    let mut layout = Layout::new();
    layout.run(
        &tree,
        (rect.x, rect.y_top),
        Constraints::loose(p.window_w, p.window_h),
        &m,
        &FontMeasure::new(&mut *p.font),
    );
    paint(&tree, &layout, &Interaction::default(), &m, &mut TreeOut::new(p));
}

fn boxes() -> [Rect; 4] {
    [
        Rect { x: 40.0, y_top: 60.0, w: 900.0, h: 44.0 },
        Rect { x: 41.0, y_top: 63.0, w: 901.0, h: 45.0 },
        Rect { x: 40.5, y_top: 60.5, w: 900.0, h: 39.0 },
        Rect { x: 17.25, y_top: 9.75, w: 333.5, h: 28.5 },
    ]
}

/// How many comparisons were made, so a run that compared nothing does
/// not pass.
fn roles_match(bench: &mut Bench) -> usize {
    let color = Tone::Fg.rgba();
    let mut compared = 0;
    for role in ROLES {
        for s in strings() {
            for rect in boxes() {
                for (theirs, ours) in ALIGNS {
                    let want = bench.glyphs(|p| p.panel_text_in(role, rect, s, color, theirs));
                    let got = bench.glyphs(|p| through_a_node(p, rect, s, font_of(role), ours, false));
                    assert!(!want.is_empty(), "{role:?} {s:?} drew nothing");
                    assert_eq!(got, want, "{role:?} {s:?} in {rect:?} at {theirs:?}");
                    compared += 1;
                }
            }
        }
    }
    compared
}

fn control_labels_match(bench: &mut Bench) -> usize {
    let color = Tone::Fg.rgba();
    let mut compared = 0;
    for (pt, weight) in control_fonts() {
        let font = TextFont::Ui(FontSpec::new(pt, weight));
        for s in strings() {
            for rect in boxes() {
                // a control is drawn on whole window units
                let rect = crate::ui::theme::spec::snap_rect(rect);
                let want = bench.glyphs(|p| controls::centred_text(p, rect, s, pt, weight, color));
                let got = bench.glyphs(|p| through_a_node(p, rect, s, font, Align::Center, true));
                assert!(!want.is_empty(), "{s:?} at {pt} pt drew nothing");
                assert_eq!(got, want, "{s:?} at {pt} pt, weight {weight}, in {rect:?}");
                compared += 1;
            }
        }
    }
    compared
}

/// The comparison has to be able to fail: a Han label placed by its
/// capitals' height instead of its ink is not where the control draws
/// it, in some box at least.
fn a_label_not_placed_by_its_ink_is_noticed(bench: &mut Bench) {
    let (pt, weight) = (c::SEGMENT_TEXT.pt(), c::SEGMENT_TEXT.weight());
    let font = TextFont::Ui(FontSpec::new(pt, weight));
    let color = Tone::Fg.rgba();
    let moved = (20..60)
        .filter(|h| {
            let rect = Rect { x: 40.0, y_top: 60.0, w: 300.0, h: *h as f64 };
            let want = bench.glyphs(|p| controls::centred_text(p, rect, "直骨海", pt, weight, color));
            let got = bench.glyphs(|p| through_a_node(p, rect, "直骨海", font, Align::Center, false));
            got != want
        })
        .count();
    assert!(moved > 0, "a label placed by its capitals passed for one placed by its ink in every box");
}

fn text_through_a_node_is_the_painters(lang: LangId, scale: f64) {
    at_scale(scale, || {
        crate::settings::set_for_test(crate::settings::Settings {
            appearance_language: Some(lang),
            ..Default::default()
        });
        crate::ui::strings::follow_language();
        if let Some(mut bench) = Bench::new() {
            let roles = roles_match(&mut bench);
            let labels = control_labels_match(&mut bench);
            a_label_not_placed_by_its_ink_is_noticed(&mut bench);
            eprintln!("[tree-text] {lang:?} at scale {scale}: {roles} role runs and {labels} control labels, glyph for glyph");
            assert_eq!(roles, ROLES.len() * strings().len() * boxes().len() * ALIGNS.len());
            assert_eq!(labels, control_fonts().len() * strings().len() * boxes().len());
        }
        crate::settings::set_for_test(crate::settings::Settings::default());
        crate::ui::strings::follow_language();
    });
}

macro_rules! text_through_a_node_is_the_painters {
    ($($test:ident = $lang:expr, $scale:expr;)*) => {$(
        #[test]
        fn $test() {
            text_through_a_node_is_the_painters($lang, $scale);
        }
    )*};
}

text_through_a_node_is_the_painters! {
    tree_text_is_the_painters_en_at_1 = LangId::En, 1.0;
    tree_text_is_the_painters_en_at_2 = LangId::En, 2.0;
    tree_text_is_the_painters_ja_at_1 = LangId::Ja, 1.0;
    tree_text_is_the_painters_ja_at_2 = LangId::Ja, 2.0;
    tree_text_is_the_painters_zh_at_1 = LangId::ZhHans, 1.0;
    tree_text_is_the_painters_zh_at_2 = LangId::ZhHans, 2.0;
}
