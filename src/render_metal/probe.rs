//! The instance lists of the frame last laid out, for tests that
//! compare where things are drawn rather than what the pixels came to.

use super::{MetalRenderer, WindowRender};
use golia_ui_core::scene::{GlyphInstance, RectInstance, UiRectInstance};

/// One frame as the encoder was handed it: the main layers, the
/// overlay layers that go over them, and the context menu's own
/// instances, which are drawn last.
#[derive(Clone, Default)]
pub(crate) struct FrameInstances {
    pub(crate) cells: Vec<RectInstance>,
    pub(crate) glyphs: Vec<GlyphInstance>,
    pub(crate) color_glyphs: Vec<GlyphInstance>,
    pub(crate) ui_rects: Vec<UiRectInstance>,
    pub(crate) overlay_cells: Vec<RectInstance>,
    pub(crate) overlay_glyphs: Vec<GlyphInstance>,
    pub(crate) overlay_ui_rects: Vec<UiRectInstance>,
    pub(crate) menu_ui_rects: Vec<UiRectInstance>,
    pub(crate) menu_glyphs: Vec<GlyphInstance>,
}

impl MetalRenderer {
    /// What `render_layout_to_texture` last laid out into `wr`.  A
    /// pane's own instances are drawn from its slot in `wr`, not from
    /// the frame's lists; they come first, as the panes are drawn
    /// under everything else.
    pub(crate) fn frame_instances(&self, wr: &WindowRender) -> FrameInstances {
        let (menu_ui_rects, menu_glyphs) = self.context_menu_state.as_ref().map_or_else(Default::default, |m| {
            let surface = m.surface.borrow();
            let (_, glyphs, ui_rects) = surface.kept.kept();
            (ui_rects.to_vec(), glyphs.to_vec())
        });
        let shown = || wr.kept.panes.iter().filter(|p| p.shown);
        let with_panes = |pane: fn(&crate::frame_build::pane_cache::PaneInstanceCache) -> &[GlyphInstance], rest: &[GlyphInstance]| {
            shown().flat_map(|p| pane(p).iter().copied()).chain(rest.iter().copied()).collect()
        };
        FrameInstances {
            cells: shown().flat_map(|p| p.cells.iter().copied()).chain(self.cells_scratch.iter().copied()).collect(),
            glyphs: with_panes(|p| &p.glyphs, &self.glyphs_scratch),
            color_glyphs: with_panes(|p| &p.color_glyphs, &self.color_glyphs_scratch),
            ui_rects: self.ui_rects_scratch.clone(),
            overlay_cells: self.overlay_cells_scratch.clone(),
            overlay_glyphs: self.overlay_glyphs_scratch.clone(),
            overlay_ui_rects: self.overlay_ui_rects_scratch.clone(),
            menu_ui_rects,
            menu_glyphs,
        }
    }
}
