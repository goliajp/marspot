//! The toolbar drawn from its tree and drawn by hand put down the same
//! instances, in the same order, at both display scales.

use super::toolbar_view::{Toolbar, ToolbarSurface};
use crate::font_cache::FontCache;
use crate::frame_alloc_tests::counted;
use crate::frame_build::chrome::{paint_toolbar, paint_version_label, ChromeState};
use crate::glyph_atlas::GlyphAtlas;
use crate::layout::Layout;
use crate::render_metal::{new_atlas, system_default_device};
use crate::ui::components::chrome_hover::ChromeHover;
use crate::ui::components::density_kit::at_scale;
use crate::ui::core::ViewPainter;
use golia_ui_core::scene::{GlyphInstance, RectInstance, UiRectInstance};

type Frame = (Vec<RectInstance>, Vec<GlyphInstance>, Vec<UiRectInstance>);

struct Stage {
    font: FontCache,
    atlas: GlyphAtlas,
}

impl Stage {
    fn new() -> Option<Self> {
        let Ok(device) = system_default_device() else {
            eprintln!("SKIPPED: no Metal device");
            return None;
        };
        let font = FontCache::build().expect("font");
        let atlas = new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        Some(Self { font, atlas })
    }

    fn draw(&mut self, window: (f64, f64), into: &mut Frame, paint: &mut dyn FnMut(&mut ViewPainter<'_>)) {
        into.0.clear();
        into.1.clear();
        into.2.clear();
        let (atlas_w, atlas_h) = self.atlas.dims();
        let mut p = ViewPainter {
            cell_w: self.font.cell_w as f32,
            cell_h: self.font.cell_h as f32,
            ascent: self.font.ascent as f32,
            atlas_w: atlas_w as f32,
            atlas_h: atlas_h as f32,
            window_w: window.0,
            window_h: window.1,
            font: &mut self.font,
            atlas: &mut self.atlas,
            cells: &mut into.0,
            glyphs: &mut into.1,
            ui_rects: &mut into.2,
        };
        paint(&mut p);
    }
}

fn layout(stage: &Stage, (w, h): (f64, f64), scale: f64) -> Layout {
    Layout::build(
        w,
        h,
        crate::ui::SIDEBAR_W_LOGICAL * scale,
        crate::HEADER_PT * scale,
        crate::ui::CELL_TITLE_PT * scale,
        2,
        2,
        stage.font.cell_w,
        stage.font.cell_h,
    )
    .with_chrome(scale, 4, None)
}

/// Every combination of buttons on, with the pointer nowhere, on each
/// button and on something else.
fn states() -> Vec<ChromeState> {
    let hovers = [
        None,
        Some(ChromeHover::Toolbar(0)),
        Some(ChromeHover::Toolbar(1)),
        Some(ChromeHover::Toolbar(2)),
        Some(ChromeHover::Toolbar(3)),
        Some(ChromeHover::Toolbar(4)),
        Some(ChromeHover::Add),
    ];
    let mut out = Vec::new();
    for bits in 0u8..32 {
        let on = std::array::from_fn(|i| bits & (1 << i) != 0);
        for hover in hovers {
            out.push(ChromeState { hover, on });
        }
    }
    out
}

#[test]
fn the_toolbar_from_its_tree_is_the_toolbar_painted_by_hand() {
    for scale in [1.0, 2.0] {
        at_scale(scale, || {
            let Some(mut stage) = Stage::new() else { return };
            let mut compared = 0;
            // roomy, narrow, so narrow the label is held at one cell, wide
            for (w, h) in [(1800.0, 1100.0), (700.0, 500.0), (180.0, 400.0), (3200.0, 1800.0)] {
                let window = (w * scale, h * scale);
                let l = layout(&stage, window, scale);
                let cell = (stage.font.cell_w as f32, stage.font.cell_h as f32, stage.font.ascent as f32);
                let mut surface = ToolbarSurface::default();
                let (mut by_hand, mut by_tree): (Frame, Frame) = Default::default();
                for state in states() {
                    stage.draw(window, &mut by_hand, &mut |p| {
                        paint_toolbar(p, &l, state);
                        paint_version_label(p, &l);
                    });
                    let t = Toolbar::of(&l, state.on, state.hover, cell);
                    stage.draw(window, &mut by_tree, &mut |p| surface.frame(p, &t));
                    assert!(!by_hand.1.is_empty(), "scale {scale}, {window:?}: nothing was painted");
                    assert_eq!(by_tree, by_hand, "scale {scale}, {window:?}, {state:?}");
                    compared += 1;
                }
            }
            eprintln!("scale {scale}: {compared} toolbars, every instance the same");
        });
    }
}

/// The comparison can fail: the pointer on another button, or a button
/// left off, is a different list of instances.
#[test]
fn a_toolbar_lit_or_on_elsewhere_is_noticed() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1800.0, 1100.0);
        let l = layout(&stage, window, 1.0);
        let cell = (stage.font.cell_w as f32, stage.font.cell_h as f32, stage.font.ascent as f32);
        let state = ChromeState { hover: Some(ChromeHover::Toolbar(1)), on: [true, false, false, false, false] };
        let (mut by_hand, mut by_tree): (Frame, Frame) = Default::default();
        stage.draw(window, &mut by_hand, &mut |p| {
            paint_toolbar(p, &l, state);
            paint_version_label(p, &l);
        });
        for (hover, on) in [
            (Some(ChromeHover::Toolbar(2)), state.on),
            (state.hover, [true, true, false, false, false]),
        ] {
            let t = Toolbar::of(&l, on, hover, cell);
            stage.draw(window, &mut by_tree, &mut |p| ToolbarSurface::default().frame(p, &t));
            assert_ne!(by_tree, by_hand, "{hover:?} {on:?} drew what the hand did for {state:?}");
        }
    });
}

/// A frame in which nothing on the toolbar changed paints nothing and
/// takes nothing from the heap; one in which a button lights repaints
/// without an allocation once the buffers have grown.
#[test]
fn toolbar_surface_frame() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1800.0, 1100.0);
        let l = layout(&stage, window, 1.0);
        let cell = (stage.font.cell_w as f32, stage.font.cell_h as f32, stage.font.ascent as f32);
        let mut surface = ToolbarSurface::default();
        let mut out: Frame = Default::default();
        let rest = Toolbar::of(&l, [true, false, false, false, false], None, cell);
        let lit = Toolbar::of(&l, rest.on, Some(ChromeHover::Toolbar(3)), cell);
        for t in [rest, lit, rest, lit, rest] {
            stage.draw(window, &mut out, &mut |p| surface.frame(p, &t));
        }
        let warm = surface.kept.surface.counters();
        let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, &rest)));
        assert_eq!(surface.kept.surface.counters(), warm, "a frame with nothing new ran a stage");
        assert_eq!(n, 0, "a frame with nothing new allocated {n} times");
        assert!(out.1.len() >= 5, "the toolbar has to have been painted");
        let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, &lit)));
        assert_eq!(n, 0, "a button lighting allocated {n} times");
        assert_eq!(surface.kept.surface.counters().paint, warm.paint + 1);
    });
}
