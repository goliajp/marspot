//! What a frame of the toolbar costs drawn from its tree.

use super::toolbar_view::{Toolbar, ToolbarSurface};
use crate::font_cache::FontCache;
use crate::frame_alloc_tests::counted;
use crate::glyph_atlas::GlyphAtlas;
use crate::layout::Layout;
use crate::render_metal::{new_atlas, system_default_device};
use crate::ui::components::density_kit::at_scale;
use crate::ui::core::ViewPainter;
use golia_ui_core::scene::{GlyphInstance, RectInstance, UiRectInstance};

pub(super) type Frame = (Vec<RectInstance>, Vec<GlyphInstance>, Vec<UiRectInstance>);

pub(super) struct Stage {
    pub(super) font: FontCache,
    pub(super) atlas: GlyphAtlas,
}

impl Stage {
    pub(super) fn new() -> Option<Self> {
        let Ok(device) = system_default_device() else {
            eprintln!("SKIPPED: no Metal device");
            return None;
        };
        let font = FontCache::build().expect("font");
        let atlas = new_atlas(&device, 2048, 2048, false, font.font_table()).map(|(a, _)| a).expect("atlas");
        Some(Self { font, atlas })
    }

    pub(super) fn draw(&mut self, window: (f64, f64), into: &mut Frame, paint: &mut dyn FnMut(&mut ViewPainter<'_>)) {
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

pub(super) fn layout(stage: &Stage, (w, h): (f64, f64), scale: f64) -> Layout {
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

/// A frame in which nothing on the toolbar changed paints nothing and
/// takes nothing from the heap; one in which a button lights under the
/// pointer paints again, describes and lays out nothing, and takes
/// nothing from the heap once the buffers have grown.
#[test]
fn toolbar_surface_frame() {
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1800.0, 1100.0);
        let l = layout(&stage, window, 1.0);
        let cell = (stage.font.cell_w as f32, stage.font.cell_h as f32, stage.font.ascent as f32);
        let mut surface = ToolbarSurface::default();
        let mut out: Frame = Default::default();
        let rest = Toolbar::of(&l, [true, false, false, false, false], cell);
        let b = l.toolbar_buttons()[3];
        let (on, off) = ((b.x + b.w * 0.5, b.y_top + b.h * 0.5), (900.0, 600.0));
        for p in [off, on, off, on, off] {
            surface.kept.surface.pointer_move(p);
            stage.draw(window, &mut out, &mut |pa| surface.frame(pa, &rest));
        }
        let warm = surface.kept.surface.counters();
        let (n, _) = counted(|| stage.draw(window, &mut out, &mut |p| surface.frame(p, &rest)));
        assert_eq!(surface.kept.surface.counters(), warm, "a frame with nothing new ran a stage");
        assert_eq!(n, 0, "a frame with nothing new allocated {n} times");
        assert!(out.1.len() >= 5, "the toolbar has to have been painted");
        let rest_picture = out.clone();
        let (n, _) = counted(|| {
            assert!(surface.kept.surface.pointer_move(on), "the button under the pointer is lit");
            stage.draw(window, &mut out, &mut |p| surface.frame(p, &rest))
        });
        assert_eq!(n, 0, "a button lighting allocated {n} times");
        let c = surface.kept.surface.counters();
        assert_eq!((c.build, c.layout, c.paint), (warm.build, warm.layout, warm.paint + 1), "a hover is a paint");
        assert_ne!(out, rest_picture, "and the button is drawn lit");
    });
}

/// A toolbar painted before the shell sent one of its icons left the
/// icon out; the frame after the icon arrives paints the toolbar again
/// though nothing on it changed.
#[test]
fn a_toolbar_kept_without_an_icon_is_painted_again_when_the_icon_arrives() {
    use crate::ui::system::macos::icons::{deliver, mask_side, Symbol};
    at_scale(1.0, || {
        let Some(mut stage) = Stage::new() else { return };
        let window = (1800.0, 1100.0);
        let l = layout(&stage, window, 1.0);
        let cell = (stage.font.cell_w as f32, stage.font.cell_h as f32, stage.font.ascent as f32);
        let mut surface = ToolbarSurface::default();
        let mut out: Frame = Default::default();
        let t = Toolbar::of(&l, [false; 5], cell);
        stage.draw(window, &mut out, &mut |p| surface.frame(p, &t));
        stage.draw(window, &mut out, &mut |p| surface.frame(p, &t));
        let before = surface.kept.surface.counters().paint;
        let side = mask_side(13);
        assert!(deliver(Symbol::Usage as u16, 13, side as u16, vec![0; (side * side) as usize]));
        stage.draw(window, &mut out, &mut |p| surface.frame(p, &t));
        assert_eq!(surface.kept.surface.counters().paint, before + 1, "the arrival did not repaint the toolbar");
    });
}
