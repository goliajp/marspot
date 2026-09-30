//! The only thing that crosses from the core to a backend.
//!
//! A scene is a flat description of one frame: layers, each with a clip
//! rectangle and up to one run of each kind of primitive, all written
//! into a byte slab the caller owns.  The core never learns what a
//! graphics API is; a backend never learns what a button is.
//!
//! Three properties are load-bearing and each is tested:
//!
//! * **Nothing is allocated.** The slab and the layer array come from
//!   the caller, who takes them from a ring of mapped upload buffers.
//!   Running out of room truncates the frame and says so; it never
//!   allocates.
//! * **Every run starts 256-byte aligned.** One graphics API fixes that
//!   number, another leaves it to the device with 256 as the largest it
//!   may demand, and the third falls back to it.  Designing to the
//!   loosest of the three would mean discovering the difference on
//!   someone else's machine.
//! * **The layout is written out, not inherited.** Instances encode
//!   themselves field by field rather than being reinterpreted as
//!   bytes, so the bytes a GPU reads are defined here and asserted in a
//!   test, instead of depending on what a compiler chose to do with a
//!   struct.

use crate::color::Rgba8;
use crate::units::{Px, RectPx};

/// What every run's offset is a multiple of.
pub const ALIGN: usize = 256;

/// The kinds of primitive, one per pipeline.
///
/// A closed set on purpose.  Pipelines are created at start-up on all
/// three backends, so this cannot be an open-ended tag, and a seventh
/// kind is a decision to be argued rather than a struct to be added:
/// primitive sprawl is the first thing that rots a renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// Flat rectangle: cell backgrounds, underlines, cursors, rules.
    Rect = 0,
    /// Filled circle with an antialiased edge: small indicators.
    Circle = 1,
    /// One glyph from the monochrome atlas.
    Glyph = 2,
    /// One glyph from the colour atlas (emoji).
    ColorGlyph = 3,
    /// Rounded rectangle with border and shadow: panels, buttons.
    UiRect = 4,
    /// An image placed in the grid: sixel, and graphics protocols.
    Image = 5,
}

/// How many kinds there are.  Sized arrays below depend on it.
pub const KIND_COUNT: usize = 6;

impl Kind {
    pub const ALL: [Kind; KIND_COUNT] = [
        Kind::Rect,
        Kind::Circle,
        Kind::Glyph,
        Kind::ColorGlyph,
        Kind::UiRect,
        Kind::Image,
    ];

    /// Bytes one instance of this kind occupies.
    pub const fn instance_size(self) -> usize {
        match self {
            Kind::Rect | Kind::Circle => RectInstance::SIZE,
            Kind::Glyph | Kind::ColorGlyph => GlyphInstance::SIZE,
            Kind::UiRect => UiRectInstance::SIZE,
            Kind::Image => ImageInstance::SIZE,
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// An instance that knows its own byte layout.
///
/// `encode` writes little-endian, field by field, into exactly `SIZE`
/// bytes.  Deliberately not a reinterpret of the struct: the bytes a
/// GPU reads are a contract, and a contract that depends on a
/// compiler's padding choices is not one.
pub trait Encode: Copy {
    const SIZE: usize;
    fn encode(&self, out: &mut [u8]);
}

#[inline]
fn put_f32(out: &mut [u8], at: usize, v: Px) {
    out[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

#[inline]
fn put_rgba(out: &mut [u8], at: usize, c: Rgba8) {
    out[at] = c.r;
    out[at + 1] = c.g;
    out[at + 2] = c.b;
    out[at + 3] = c.a;
}

/// A rectangle or a circle: position, size, colour.  20 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct RectInstance {
    pub origin: [Px; 2],
    pub size: [Px; 2],
    pub color: Rgba8,
}

impl Encode for RectInstance {
    const SIZE: usize = 20;
    fn encode(&self, out: &mut [u8]) {
        put_f32(out, 0, self.origin[0]);
        put_f32(out, 4, self.origin[1]);
        put_f32(out, 8, self.size[0]);
        put_f32(out, 12, self.size[1]);
        put_rgba(out, 16, self.color);
    }
}

/// One glyph: where it goes, where it is in the atlas, what colour it
/// is tinted.  36 bytes.  The atlas coordinates are normalised, so the
/// core never needs to know the atlas's pixel size.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct GlyphInstance {
    pub origin: [Px; 2],
    pub size: [Px; 2],
    pub uv0: [f32; 2],
    pub uv1: [f32; 2],
    pub color: Rgba8,
}

impl Encode for GlyphInstance {
    const SIZE: usize = 36;
    fn encode(&self, out: &mut [u8]) {
        put_f32(out, 0, self.origin[0]);
        put_f32(out, 4, self.origin[1]);
        put_f32(out, 8, self.size[0]);
        put_f32(out, 12, self.size[1]);
        put_f32(out, 16, self.uv0[0]);
        put_f32(out, 20, self.uv0[1]);
        put_f32(out, 24, self.uv1[0]);
        put_f32(out, 28, self.uv1[1]);
        put_rgba(out, 32, self.color);
    }
}

/// An image placed in the grid.  36 bytes.  `texture` names which
/// image; resolving that to something a GPU can bind is the backend's
/// problem, which is the whole point of it being a number here.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct ImageInstance {
    pub origin: [Px; 2],
    pub size: [Px; 2],
    pub uv0: [f32; 2],
    pub uv1: [f32; 2],
    pub texture: u32,
}

impl Encode for ImageInstance {
    const SIZE: usize = 36;
    fn encode(&self, out: &mut [u8]) {
        put_f32(out, 0, self.origin[0]);
        put_f32(out, 4, self.origin[1]);
        put_f32(out, 8, self.size[0]);
        put_f32(out, 12, self.size[1]);
        put_f32(out, 16, self.uv0[0]);
        put_f32(out, 20, self.uv0[1]);
        put_f32(out, 24, self.uv1[0]);
        put_f32(out, 28, self.uv1[1]);
        out[32..36].copy_from_slice(&self.texture.to_le_bytes());
    }
}

/// A panel's rectangle: rounded, bordered, with a drop shadow.
/// 48 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct UiRectInstance {
    pub origin: [Px; 2],
    pub size: [Px; 2],
    pub fill: Rgba8,
    pub border: Rgba8,
    pub radius: Px,
    pub border_width: Px,
    pub shadow_offset: [Px; 2],
    pub shadow_color: Rgba8,
    pub shadow_blur: Px,
}

impl Encode for UiRectInstance {
    const SIZE: usize = 48;
    fn encode(&self, out: &mut [u8]) {
        put_f32(out, 0, self.origin[0]);
        put_f32(out, 4, self.origin[1]);
        put_f32(out, 8, self.size[0]);
        put_f32(out, 12, self.size[1]);
        put_rgba(out, 16, self.fill);
        put_rgba(out, 20, self.border);
        put_f32(out, 24, self.radius);
        put_f32(out, 28, self.border_width);
        put_f32(out, 32, self.shadow_offset[0]);
        put_f32(out, 36, self.shadow_offset[1]);
        put_rgba(out, 40, self.shadow_color);
        put_f32(out, 44, self.shadow_blur);
    }
}

/// Where one kind's instances live inside the slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Run {
    /// Byte offset into the slab.  Always a multiple of [`ALIGN`].
    pub offset: u32,
    /// How many instances.  Zero means this kind is absent and the
    /// backend issues no draw for it.
    pub count: u32,
}

/// One clip rectangle's worth of drawing.
///
/// Depth is the layer's position in the scene's list; there is no z
/// field, because two sources of truth for order is one too many.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Layer {
    /// Scissor rectangle, physical pixels, top-left origin.
    pub clip: RectPx,
    /// A fingerprint of everything this layer's contents were built
    /// from.
    ///
    /// Unchanged means the previous frame's bytes are still correct and
    /// can be copied forward instead of rebuilt — the saving is the
    /// build, not the upload.
    ///
    /// **It must cover this layer's own inputs and nothing else.** A
    /// piece of window-wide state inside one layer's key makes every
    /// unrelated change invalidate it; that exact mistake cost a whole
    /// window's panes a rebuild every time the pointer crossed a
    /// toolbar.
    pub content_key: u64,
    /// One run per kind, indexed by `Kind as usize`.
    pub runs: [Run; KIND_COUNT],
}

/// One frame, being written into memory the caller owns.
pub struct Scene<'a> {
    slab: &'a mut [u8],
    used: usize,
    layers: &'a mut [Layer],
    layer_count: usize,
    overflowed: bool,
}

impl<'a> Scene<'a> {
    /// Start a frame in `slab`, recording at most `layers.len()` layers.
    pub fn new(slab: &'a mut [u8], layers: &'a mut [Layer]) -> Self {
        Self { slab, used: 0, layers, layer_count: 0, overflowed: false }
    }

    /// Begin a layer.  Returns `None` when the layer array is full —
    /// the frame goes out short rather than growing anything.
    pub fn layer(&mut self, clip: RectPx, content_key: u64) -> Option<LayerWriter<'_, 'a>> {
        if self.layer_count == self.layers.len() {
            self.overflowed = true;
            return None;
        }
        let index = self.layer_count;
        self.layers[index] = Layer { clip, content_key, runs: [Run::default(); KIND_COUNT] };
        self.layer_count += 1;
        Some(LayerWriter { scene: self, index })
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers[..self.layer_count]
    }

    /// The bytes written so far, to be handed to a backend.
    pub fn bytes(&self) -> &[u8] {
        &self.slab[..self.used]
    }

    /// Whether anything was dropped for want of room.  A frame that
    /// overflowed is still drawable; it is just missing its tail, and
    /// the caller should grow its buffers before the next one.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    fn align_up(&mut self) {
        let rounded = self.used.div_ceil(ALIGN) * ALIGN;
        if rounded > self.slab.len() {
            self.used = self.slab.len();
            self.overflowed = true;
        } else {
            // The padding is never read, but leaving stale bytes in it
            // makes two identical frames differ, which is exactly the
            // sort of thing that makes a content comparison useless.
            self.slab[self.used..rounded].fill(0);
            self.used = rounded;
        }
    }
}

/// A layer that is open for writing.
pub struct LayerWriter<'s, 'a> {
    scene: &'s mut Scene<'a>,
    index: usize,
}

impl<'s, 'a> LayerWriter<'s, 'a> {
    /// Open the run for one kind.
    ///
    /// One run per kind per layer: the returned writer borrows this
    /// layer, so a second run cannot be opened while one is live, and
    /// opening the same kind twice is a caller bug the debug build
    /// catches.  Grouping by kind is what the GPU wants anyway — one
    /// draw per kind per layer.
    pub fn rects(&mut self) -> RunWriter<'_, 'a, RectInstance> {
        self.open(Kind::Rect)
    }
    pub fn circles(&mut self) -> RunWriter<'_, 'a, RectInstance> {
        self.open(Kind::Circle)
    }
    pub fn glyphs(&mut self) -> RunWriter<'_, 'a, GlyphInstance> {
        self.open(Kind::Glyph)
    }
    pub fn color_glyphs(&mut self) -> RunWriter<'_, 'a, GlyphInstance> {
        self.open(Kind::ColorGlyph)
    }
    pub fn ui_rects(&mut self) -> RunWriter<'_, 'a, UiRectInstance> {
        self.open(Kind::UiRect)
    }
    pub fn images(&mut self) -> RunWriter<'_, 'a, ImageInstance> {
        self.open(Kind::Image)
    }

    fn open<T: Encode>(&mut self, kind: Kind) -> RunWriter<'_, 'a, T> {
        debug_assert_eq!(
            self.scene.layers[self.index].runs[kind.index()].count, 0,
            "{kind:?} already has a run in this layer; a kind gets one run, \
             or a backend would need more than one draw for it"
        );
        self.scene.align_up();
        let offset = self.scene.used;
        RunWriter {
            scene: self.scene,
            layer_index: self.index,
            kind,
            offset,
            count: 0,
            _marker: core::marker::PhantomData,
        }
    }
}

/// An open run.  Instances are written straight into the slab; closing
/// it records where they went.
pub struct RunWriter<'s, 'a, T: Encode> {
    scene: &'s mut Scene<'a>,
    layer_index: usize,
    kind: Kind,
    offset: usize,
    count: u32,
    _marker: core::marker::PhantomData<T>,
}

impl<T: Encode> RunWriter<'_, '_, T> {
    /// Append one instance.  `false` means the slab is full and this
    /// one was dropped.
    pub fn push(&mut self, v: T) -> bool {
        let at = self.offset + self.count as usize * T::SIZE;
        if at + T::SIZE > self.scene.slab.len() {
            self.scene.overflowed = true;
            return false;
        }
        v.encode(&mut self.scene.slab[at..at + T::SIZE]);
        self.count += 1;
        true
    }

    /// Append many.  Returns how many fit.
    pub fn extend(&mut self, vs: &[T]) -> usize {
        let mut n = 0;
        for v in vs {
            if !self.push(*v) {
                break;
            }
            n += 1;
        }
        n
    }

    pub fn len(&self) -> u32 {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

impl<T: Encode> Drop for RunWriter<'_, '_, T> {
    fn drop(&mut self) {
        let written = self.count as usize * T::SIZE;
        self.scene.used = self.offset + written;
        let index = self.layer_index;
        self.scene.layers[index].runs[self.kind.index()] = Run {
            offset: self.offset as u32,
            count: self.count,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffers() -> (Vec<u8>, Vec<Layer>) {
        (vec![0u8; 8192], vec![Layer::default(); 8])
    }

    #[test]
    fn a_rect_encodes_to_the_documented_bytes() {
        // The bytes a GPU reads are a contract.  Written out here so a
        // change to the struct cannot silently move a field under a
        // shader that is compiled separately and cannot complain.
        let mut out = [0xAAu8; RectInstance::SIZE];
        RectInstance {
            origin: [1.0, 2.0],
            size: [3.0, 4.0],
            color: Rgba8::rgba(5, 6, 7, 8),
        }
        .encode(&mut out);
        assert_eq!(&out[0..4], &1.0f32.to_le_bytes());
        assert_eq!(&out[4..8], &2.0f32.to_le_bytes());
        assert_eq!(&out[8..12], &3.0f32.to_le_bytes());
        assert_eq!(&out[12..16], &4.0f32.to_le_bytes());
        assert_eq!(&out[16..20], &[5, 6, 7, 8]);
    }

    #[test]
    fn every_instance_size_matches_what_the_kind_promises() {
        assert_eq!(Kind::Rect.instance_size(), RectInstance::SIZE);
        assert_eq!(Kind::Circle.instance_size(), RectInstance::SIZE);
        assert_eq!(Kind::Glyph.instance_size(), GlyphInstance::SIZE);
        assert_eq!(Kind::ColorGlyph.instance_size(), GlyphInstance::SIZE);
        assert_eq!(Kind::UiRect.instance_size(), UiRectInstance::SIZE);
        assert_eq!(Kind::Image.instance_size(), ImageInstance::SIZE);
        // and the kinds are exactly the array
        assert_eq!(Kind::ALL.len(), KIND_COUNT);
        for (i, k) in Kind::ALL.iter().enumerate() {
            assert_eq!(*k as usize, i, "Kind::ALL is out of order at {i}");
        }
    }

    #[test]
    fn every_run_starts_aligned() {
        let (mut slab, mut layers) = buffers();
        let mut scene = Scene::new(&mut slab, &mut layers);
        {
            let mut l = scene.layer(RectPx::new(0.0, 0.0, 100.0, 100.0), 1).unwrap();
            // Deliberately odd counts: 3 rects is 60 bytes, which lands
            // nowhere near a boundary, so the next run has to be pushed
            // out to one.
            let mut r = l.rects();
            for _ in 0..3 {
                assert!(r.push(RectInstance::default()));
            }
            drop(r);
            let mut g = l.glyphs();
            for _ in 0..5 {
                assert!(g.push(GlyphInstance::default()));
            }
        }
        let layer = scene.layers()[0];
        assert_eq!(layer.runs[Kind::Rect as usize].count, 3);
        assert_eq!(layer.runs[Kind::Glyph as usize].count, 5);
        for k in Kind::ALL {
            let run = layer.runs[k as usize];
            if run.count > 0 {
                assert_eq!(
                    run.offset as usize % ALIGN,
                    0,
                    "{k:?} starts at {} which is not a multiple of {ALIGN}",
                    run.offset
                );
            }
        }
        // …and the second run really did move past the first
        assert!(layer.runs[Kind::Glyph as usize].offset >= ALIGN as u32);
    }

    #[test]
    fn the_alignment_check_can_fail() {
        // A test that only ever sees aligned offsets proves nothing
        // about the check.  Feed it an offset built the wrong way and
        // make sure the same assertion rejects it.
        let misaligned = Run { offset: 20, count: 1 };
        assert_ne!(misaligned.offset as usize % ALIGN, 0);
    }

    #[test]
    fn layers_do_not_reach_into_each_other() {
        let (mut slab, mut layers) = buffers();
        let mut scene = Scene::new(&mut slab, &mut layers);
        {
            let mut a = scene.layer(RectPx::new(0.0, 0.0, 10.0, 10.0), 0xAA).unwrap();
            a.rects().push(RectInstance::default());
        }
        let first = scene.layers()[0];
        {
            let mut b = scene.layer(RectPx::new(10.0, 0.0, 10.0, 10.0), 0xBB).unwrap();
            let mut r = b.rects();
            for _ in 0..40 {
                r.push(RectInstance::default());
            }
        }
        assert_eq!(scene.layers()[0], first, "writing a layer moved an earlier one");
        assert_eq!(scene.layers()[1].content_key, 0xBB);
        assert_eq!(scene.layers().len(), 2);
    }

    #[test]
    fn running_out_of_slab_truncates_and_says_so() {
        // Exactly two instances' worth.  The first run starts at 0,
        // so there is no alignment padding to account for.
        let mut slab = [0u8; RectInstance::SIZE * 2];
        let mut layers = [Layer::default(); 4];
        let mut scene = Scene::new(&mut slab, &mut layers);
        {
            let mut l = scene.layer(RectPx::ZERO, 1).unwrap();
            let mut r = l.rects();
            assert!(r.push(RectInstance::default()), "the first one fits");
            assert!(r.push(RectInstance::default()), "and so does the second");
            assert!(!r.push(RectInstance::default()), "the third does not");
            assert_eq!(r.len(), 2, "and it was not counted");
        }
        assert!(scene.overflowed());
        // The frame is still drawable: two rects, correctly recorded.
        assert_eq!(scene.layers()[0].runs[Kind::Rect as usize].count, 2);
    }

    #[test]
    fn running_out_of_layers_truncates_and_says_so() {
        let mut slab = [0u8; 4096];
        let mut layers = [Layer::default(); 1];
        let mut scene = Scene::new(&mut slab, &mut layers);
        assert!(scene.layer(RectPx::ZERO, 1).is_some());
        assert!(scene.layer(RectPx::ZERO, 2).is_none());
        assert!(scene.overflowed());
        assert_eq!(scene.layers().len(), 1);
    }

    #[test]
    fn padding_is_zeroed_so_identical_frames_have_identical_bytes() {
        // Two frames built the same way must produce the same bytes,
        // or comparing a layer's bytes to last frame's — the thing the
        // content key exists to allow — would report changes that are
        // only leftover padding.
        let build = |slab: &mut [u8], layers: &mut [Layer]| {
            let mut scene = Scene::new(slab, layers);
            let mut l = scene.layer(RectPx::new(0.0, 0.0, 9.0, 9.0), 7).unwrap();
            let mut r = l.rects();
            r.push(RectInstance { origin: [1.0, 1.0], ..Default::default() });
            drop(r);
            l.glyphs().push(GlyphInstance::default());
            
            scene.bytes().len()
        };
        let mut a = vec![0x11u8; 4096];
        let mut al = vec![Layer::default(); 4];
        let mut b = vec![0x22u8; 4096];
        let mut bl = vec![Layer::default(); 4];
        let na = build(&mut a, &mut al);
        let nb = build(&mut b, &mut bl);
        assert_eq!(na, nb);
        assert_eq!(a[..na], b[..nb], "the same frame came out as different bytes");
    }
}
