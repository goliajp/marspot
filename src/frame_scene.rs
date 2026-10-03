//! One frame's instances, written out as a `Scene`.
//!
//! The renderer used to hand the GPU eight separate buffers and open a
//! render pass for each, every pass loading the whole target and
//! storing it again. This lays the same instances out as one scene --
//! one slab, a few layers, at most one run per kind per layer -- so a
//! backend draws it in a single pass.
//!
//! The order things are drawn in does not change. Within a layer a
//! scene draws kinds in a fixed order (rects, circles, glyphs, colour
//! glyphs, rounded rects, images), and the frame draws rounded rects
//! *under* its glyphs, so they go in a layer of their own. Six layers
//! reproduce the eight passes exactly:
//!
//! | layer | runs |
//! |---|---|
//! | 0 | cell rects, circles |
//! | 1 | rounded rects |
//! | 2 | glyphs, colour glyphs |
//! | 3 | overlay rects |
//! | 4 | overlay rounded rects |
//! | 5 | overlay glyphs |
//!
//! Nothing here knows what a graphics API is.

use golia_ui_core::scene::{
    ALIGN, Encode, GlyphInstance, Layer, RectInstance, Scene, UiRectInstance,
};
use golia_ui_core::units::RectPx;

/// The most layers a frame uses; see the table above.
pub const FRAME_LAYERS: usize = 6;

/// Everything one frame draws, grouped the way the passes were.
#[derive(Clone, Copy)]
pub struct FrameInstances<'a> {
    pub cells: &'a [RectInstance],
    pub dots: &'a [RectInstance],
    pub ui_rects: &'a [UiRectInstance],
    pub glyphs: &'a [GlyphInstance],
    pub color_glyphs: &'a [GlyphInstance],
    pub overlay_cells: &'a [RectInstance],
    pub overlay_ui_rects: &'a [UiRectInstance],
    pub overlay_glyphs: &'a [GlyphInstance],
}

/// What a scene built from it occupies: how many bytes of the slab and
/// how many of the layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Built {
    pub bytes: usize,
    pub layers: usize,
}

impl FrameInstances<'_> {
    /// An upper bound on the slab this frame needs: every run's bytes,
    /// plus the most padding aligning its start can add.
    fn slab_bound(&self) -> usize {
        fn run<T: Encode>(xs: &[T]) -> usize {
            if xs.is_empty() { 0 } else { xs.len() * T::SIZE + ALIGN }
        }
        run(self.cells)
            + run(self.dots)
            + run(self.ui_rects)
            + run(self.glyphs)
            + run(self.color_glyphs)
            + run(self.overlay_cells)
            + run(self.overlay_ui_rects)
            + run(self.overlay_glyphs)
    }
}

/// Write `frame` into `slab` as a scene clipped to `clip`, recording
/// its layers in `layers`.
///
/// The slab grows to fit and is kept by the caller, so a steady state
/// allocates nothing. A layer with nothing in it is left out, and an
/// empty run is never opened: a backend skips both anyway, and leaving
/// them out keeps the padding they would have cost out of the slab.
pub fn build(
    frame: &FrameInstances,
    clip: RectPx,
    slab: &mut Vec<u8>,
    layers: &mut [Layer; FRAME_LAYERS],
) -> Built {
    let need = frame.slab_bound();
    if slab.len() < need {
        slab.resize(need, 0);
    }
    let mut scene = Scene::new(&mut slab[..], &mut layers[..]);
    {
        let f = frame;
        if !f.cells.is_empty() || !f.dots.is_empty() {
            if let Some(mut l) = scene.layer(clip, 0) {
                if !f.cells.is_empty() {
                    l.rects().extend(f.cells);
                }
                if !f.dots.is_empty() {
                    l.circles().extend(f.dots);
                }
            }
        }
        if !f.ui_rects.is_empty() {
            if let Some(mut l) = scene.layer(clip, 0) {
                l.ui_rects().extend(f.ui_rects);
            }
        }
        if !f.glyphs.is_empty() || !f.color_glyphs.is_empty() {
            if let Some(mut l) = scene.layer(clip, 0) {
                if !f.glyphs.is_empty() {
                    l.glyphs().extend(f.glyphs);
                }
                if !f.color_glyphs.is_empty() {
                    l.color_glyphs().extend(f.color_glyphs);
                }
            }
        }
        if !f.overlay_cells.is_empty() {
            if let Some(mut l) = scene.layer(clip, 0) {
                l.rects().extend(f.overlay_cells);
            }
        }
        if !f.overlay_ui_rects.is_empty() {
            if let Some(mut l) = scene.layer(clip, 0) {
                l.ui_rects().extend(f.overlay_ui_rects);
            }
        }
        if !f.overlay_glyphs.is_empty() {
            if let Some(mut l) = scene.layer(clip, 0) {
                l.glyphs().extend(f.overlay_glyphs);
            }
        }
    }
    // The bound above is exact enough that this cannot happen; if it
    // ever does, the frame would go out missing its tail.
    debug_assert!(!scene.overflowed(), "the slab bound under-counted");
    Built { bytes: scene.bytes().len(), layers: scene.layers().len() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use golia_ui_core::color::Rgba8;
    use golia_ui_core::scene::Kind;

    fn rect(x: f32) -> RectInstance {
        RectInstance { origin: [x, 0.0], size: [1.0, 1.0], color: Rgba8::rgba(1, 2, 3, 4) }
    }
    fn glyph(x: f32) -> GlyphInstance {
        GlyphInstance {
            origin: [x, 0.0],
            size: [1.0, 1.0],
            uv0: [0.0, 0.0],
            uv1: [1.0, 1.0],
            color: Rgba8::rgba(9, 9, 9, 9),
        }
    }
    fn ui(x: f32) -> UiRectInstance {
        UiRectInstance { origin: [x, 0.0], size: [4.0, 4.0], ..Default::default() }
    }

    const CLIP: RectPx = RectPx::new(0.0, 0.0, 800.0, 600.0);

    fn empty() -> FrameInstances<'static> {
        FrameInstances {
            cells: &[],
            dots: &[],
            ui_rects: &[],
            glyphs: &[],
            color_glyphs: &[],
            overlay_cells: &[],
            overlay_ui_rects: &[],
            overlay_glyphs: &[],
        }
    }

    /// The order the passes drew in is the order the layers draw in:
    /// rounded rects under glyphs, overlays over everything.
    #[test]
    fn the_layers_reproduce_the_order_of_the_passes() {
        let cells = [rect(1.0), rect(2.0)];
        let dots = [rect(3.0)];
        let uis = [ui(4.0)];
        let glyphs = [glyph(5.0), glyph(6.0), glyph(7.0)];
        let colour = [glyph(8.0)];
        let ocells = [rect(9.0)];
        let ouis = [ui(10.0)];
        let oglyphs = [glyph(11.0)];
        let frame = FrameInstances {
            cells: &cells,
            dots: &dots,
            ui_rects: &uis,
            glyphs: &glyphs,
            color_glyphs: &colour,
            overlay_cells: &ocells,
            overlay_ui_rects: &ouis,
            overlay_glyphs: &oglyphs,
        };
        let mut slab = Vec::new();
        let mut layers = [Layer::default(); FRAME_LAYERS];
        let built = build(&frame, CLIP, &mut slab, &mut layers);
        assert_eq!(built.layers, 6);

        let counts: Vec<Vec<(Kind, u32)>> = layers[..built.layers]
            .iter()
            .map(|l| {
                Kind::ALL
                    .iter()
                    .filter(|k| l.runs[**k as usize].count > 0)
                    .map(|k| (*k, l.runs[*k as usize].count))
                    .collect()
            })
            .collect();
        assert_eq!(
            counts,
            vec![
                vec![(Kind::Rect, 2), (Kind::Circle, 1)],
                vec![(Kind::UiRect, 1)],
                vec![(Kind::Glyph, 3), (Kind::ColorGlyph, 1)],
                vec![(Kind::Rect, 1)],
                vec![(Kind::UiRect, 1)],
                vec![(Kind::Glyph, 1)],
            ]
        );
        assert!(layers[..built.layers].iter().all(|l| l.clip == CLIP));
    }

    /// The bytes in each run are the published encoding of the
    /// instances that went in, at an aligned offset.
    #[test]
    fn each_run_holds_its_instances_encoded_at_an_aligned_offset() {
        let glyphs = [glyph(5.0), glyph(6.0)];
        let cells = [rect(1.0)];
        let frame = FrameInstances { cells: &cells, glyphs: &glyphs, ..empty() };
        let mut slab = Vec::new();
        let mut layers = [Layer::default(); FRAME_LAYERS];
        let built = build(&frame, CLIP, &mut slab, &mut layers);
        assert_eq!(built.layers, 2);

        let run = layers[1].runs[Kind::Glyph as usize];
        assert_eq!(run.offset as usize % ALIGN, 0);
        let mut want = vec![0u8; GlyphInstance::SIZE];
        for (i, g) in glyphs.iter().enumerate() {
            g.encode(&mut want);
            let at = run.offset as usize + i * GlyphInstance::SIZE;
            assert_eq!(&slab[at..at + GlyphInstance::SIZE], &want[..], "glyph {i}");
        }
        assert!(run.offset as usize + 2 * GlyphInstance::SIZE <= built.bytes);
    }

    /// An empty frame is no layers and no bytes; a layer with nothing
    /// in it is left out rather than drawn as nothing.
    #[test]
    fn nothing_to_draw_is_nothing_in_the_scene() {
        let mut slab = Vec::new();
        let mut layers = [Layer::default(); FRAME_LAYERS];
        assert_eq!(build(&empty(), CLIP, &mut slab, &mut layers), Built { bytes: 0, layers: 0 });

        let oglyphs = [glyph(1.0)];
        let only_overlay = FrameInstances { overlay_glyphs: &oglyphs, ..empty() };
        let built = build(&only_overlay, CLIP, &mut slab, &mut layers);
        assert_eq!(built.layers, 1);
        assert_eq!(layers[0].runs[Kind::Glyph as usize].count, 1);
    }

    /// A steady state reuses the slab: a frame no bigger than the last
    /// does not grow it.
    #[test]
    fn a_frame_no_bigger_than_the_last_does_not_grow_the_slab() {
        let cells: Vec<RectInstance> = (0..2000).map(|i| rect(i as f32)).collect();
        let glyphs: Vec<GlyphInstance> = (0..1500).map(|i| glyph(i as f32)).collect();
        let frame = FrameInstances { cells: &cells, glyphs: &glyphs, ..empty() };
        let mut slab = Vec::new();
        let mut layers = [Layer::default(); FRAME_LAYERS];
        build(&frame, CLIP, &mut slab, &mut layers);
        let (ptr, cap) = (slab.as_ptr(), slab.capacity());
        let smaller = FrameInstances { cells: &cells[..1000], glyphs: &glyphs, ..empty() };
        build(&smaller, CLIP, &mut slab, &mut layers);
        build(&frame, CLIP, &mut slab, &mut layers);
        assert_eq!((slab.as_ptr(), slab.capacity()), (ptr, cap));
    }
}
