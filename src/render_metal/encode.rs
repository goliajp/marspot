//! One frame's draw calls, and the buffers its instances are read from.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLDevice, MTLLoadAction, MTLPrimitiveType,
    MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineState, MTLResourceOptions,
    MTLSamplerState, MTLScissorRect, MTLStoreAction, MTLTexture,
};

use super::{clear_color, CellInstance, GlyphInstance, UiRectInstance};
use crate::frame_build::pane_cache::PaneInstanceCache;
use golia_ui_core::scene::Encode;

/// Wall time spent creating instance buffers, and how many bytes,
/// since the last `take_instance_buffer_cost`.
///
/// A path that allocates a fresh `MTLBuffer` for its instances on
/// every frame is exactly what a per-frame hot path must not do.
/// Whether that is what a slow `encode` is made of is a question for a
/// number, not for a reading of the code: these two counters are that
/// number.
static INSTANCE_BUF_US: AtomicU64 = AtomicU64::new(0);
static INSTANCE_BUF_BYTES: AtomicU64 = AtomicU64::new(0);
/// Bytes copied into instance buffers since it was last read.
static UPLOAD_BYTES: AtomicU64 = AtomicU64::new(0);

/// Read and reset the allocation cost accumulated since the last call.
pub fn take_instance_buffer_cost() -> (u64, u64) {
    (INSTANCE_BUF_US.swap(0, Ordering::Relaxed), INSTANCE_BUF_BYTES.swap(0, Ordering::Relaxed))
}

/// Read and reset the bytes copied into instance buffers.
pub(super) fn take_upload_bytes() -> u64 {
    UPLOAD_BYTES.swap(0, Ordering::Relaxed)
}

type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;

fn new_buffer(device: &ProtocolObject<dyn MTLDevice>, cap: usize) -> Option<Buffer> {
    let t0 = std::time::Instant::now();
    let buf = device.newBufferWithLength_options(cap, MTLResourceOptions::StorageModeShared);
    INSTANCE_BUF_US.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    INSTANCE_BUF_BYTES.fetch_add(cap as u64, Ordering::Relaxed);
    buf
}

/// One pane's instances on the GPU: a buffer per list, kept from frame
/// to frame and written only where the pane's lists changed.
#[derive(Default)]
struct PaneBuffers {
    bufs: [Option<Buffer>; 3],
    caps: [usize; 3],
}

/// Persistent per-pass instance buffers for the IOSurface path.
///
/// Allocating a buffer per pass per frame was measured at 126–143 ms of
/// a ~200 ms `encode` in sampled stalls (2026-08-12, 13 panes, load
/// ~7–10).  While it blocks, every pane is frozen and the supervisor's
/// PONG deadline is running.
///
/// So the buffers are allocated once and refilled in place.
/// `StorageModeShared` means `contents()` is CPU-writable, and the
/// capacity only ever grows — rounded up to a power of two so growth
/// stops happening after the first few frames.  Steady state performs
/// no allocation at all, which is what is asked of a per-frame path in
/// the first place.
///
/// **Safety contract**: refilling in place is only sound if the GPU is
/// done with the previous frame's contents.  On the IOSurface path the
/// pool belongs to one window (`WindowRender::instance_pool`) and a
/// window never starts a second frame while its first is in flight, so
/// it is.  It was one pool for every window back when each frame ended
/// in `waitUntilCompleted`; once frames were left running, two windows
/// painting at once overwrote each other's instances.  The live
/// `CAMetalLayer` path only waits until *scheduled*, so it keeps
/// allocating per frame and passes `None`.
#[derive(Default)]
pub struct InstanceBufferPool {
    /// Everything that is not a pane's own: one scene slab a frame.
    slab: Option<Buffer>,
    slab_cap: usize,
    panes: Vec<PaneBuffers>,
    /// The buffers of the panes in the frame being encoded, and how
    /// many instances each holds; refilled every frame.
    drawn: Vec<[(Option<Buffer>, usize); 3]>,
}

impl InstanceBufferPool {
    /// Copy the frame's slab in, growing the buffer if need be.
    fn upload_slab(&mut self, device: &ProtocolObject<dyn MTLDevice>, bytes: &[u8]) -> Option<Buffer> {
        if bytes.is_empty() {
            return None;
        }
        if self.slab_cap < bytes.len() || self.slab.is_none() {
            // Round up so a frame that grows a little does not
            // reallocate — the allocation is the thing being avoided.
            let cap = bytes.len().next_power_of_two().max(64 * 1024);
            self.slab = new_buffer(device, cap);
            self.slab_cap = if self.slab.is_some() { cap } else { 0 };
        }
        let buf = self.slab.as_ref()?;
        UPLOAD_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        // SAFETY: the buffer holds at least `bytes.len()` bytes, and
        // the frame that last read it has completed (see the contract)
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf.contents().as_ptr() as *mut u8, bytes.len());
        }
        Some(buf.clone())
    }

    /// Where every buffer this pool holds keeps its bytes.
    #[cfg(test)]
    pub(super) fn memory(&self) -> Vec<*mut c_void> {
        let panes = self.panes.iter().flat_map(|p| p.bufs.iter());
        std::iter::once(&self.slab).chain(panes).flatten().map(|b| b.contents().as_ptr()).collect()
    }

    /// Bring pane `i`'s three buffers up to date with its lists,
    /// writing only what changed since they were last brought up to
    /// date, and add them to `drawn`.
    fn sync_pane(&mut self, device: &ProtocolObject<dyn MTLDevice>, i: usize, cache: &mut PaneInstanceCache) {
        while self.panes.len() <= i {
            self.panes.push(PaneBuffers::default());
        }
        let pane = &mut self.panes[i];
        let mut out: [(Option<Buffer>, usize); 3] = Default::default();
        for kind in 0..3 {
            let (len, size) = match kind {
                0 => (cache.cells.len(), CellInstance::SIZE),
                1 => (cache.glyphs.len(), GlyphInstance::SIZE),
                _ => (cache.color_glyphs.len(), GlyphInstance::SIZE),
            };
            let mut spans = std::mem::take(&mut cache.dirty[kind]);
            if len > 0 {
                if pane.caps[kind] < len * size || pane.bufs[kind].is_none() {
                    let cap = (len * size).next_power_of_two().max(16 * 1024);
                    pane.bufs[kind] = new_buffer(device, cap);
                    pane.caps[kind] = if pane.bufs[kind].is_some() { cap } else { 0 };
                    // a new buffer holds nothing yet
                    spans.clear();
                    spans.push((0, len as u32));
                }
                if let Some(buf) = pane.bufs[kind].as_ref() {
                    // SAFETY: the buffer is `caps[kind]` bytes of
                    // CPU-writable memory nothing else is reading: the
                    // frame that last drew from it has completed
                    let dst = unsafe {
                        std::slice::from_raw_parts_mut(buf.contents().as_ptr() as *mut u8, pane.caps[kind])
                    };
                    for &(lo, hi) in &spans {
                        let (lo, hi) = (lo as usize, (hi as usize).min(len));
                        if lo >= hi {
                            continue;
                        }
                        match kind {
                            0 => encode_span(&cache.cells[lo..hi], &mut dst[lo * size..hi * size]),
                            1 => encode_span(&cache.glyphs[lo..hi], &mut dst[lo * size..hi * size]),
                            _ => encode_span(&cache.color_glyphs[lo..hi], &mut dst[lo * size..hi * size]),
                        }
                        UPLOAD_BYTES.fetch_add(((hi - lo) * size) as u64, Ordering::Relaxed);
                    }
                    out[kind] = (Some(buf.clone()), len);
                }
            }
            spans.clear();
            cache.dirty[kind] = spans;
        }
        self.drawn.push(out);
    }
}

fn encode_span<T: Encode>(instances: &[T], out: &mut [u8]) {
    for (v, slot) in instances.iter().zip(out.chunks_exact_mut(T::SIZE)) {
        v.encode(slot);
    }
}

pub(super) fn make_instance_buffer(device: &ProtocolObject<dyn MTLDevice>, bytes: &[u8]) -> Option<Buffer> {
    if bytes.is_empty() {
        return None;
    }
    let t0 = std::time::Instant::now();
    // SAFETY: `bytes` is non-empty, so its pointer is non-null, and
    // Metal copies `bytes.len()` bytes out of it before returning
    let out = unsafe {
        device.newBufferWithBytes_length_options(
            NonNull::new(bytes.as_ptr() as *mut c_void).unwrap(),
            bytes.len(),
            MTLResourceOptions::StorageModeShared,
        )
    };
    INSTANCE_BUF_US.fetch_add(t0.elapsed().as_micros() as u64, Ordering::Relaxed);
    INSTANCE_BUF_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    out
}

/// The pipelines and atlases every frame is drawn with.
pub(super) struct Drawing<'a> {
    pub(super) bg_pipeline: &'a ProtocolObject<dyn MTLRenderPipelineState>,
    pub(super) dot_pipeline: &'a ProtocolObject<dyn MTLRenderPipelineState>,
    pub(super) fg_pipeline: &'a ProtocolObject<dyn MTLRenderPipelineState>,
    pub(super) fg_color_pipeline: &'a ProtocolObject<dyn MTLRenderPipelineState>,
    pub(super) scene_ui_pipeline: &'a ProtocolObject<dyn MTLRenderPipelineState>,
    pub(super) fg_sampler: &'a ProtocolObject<dyn MTLSamplerState>,
    pub(super) atlas_texture: &'a ProtocolObject<dyn MTLTexture>,
    pub(super) color_atlas_texture: &'a ProtocolObject<dyn MTLTexture>,
    pub(super) device: &'a ProtocolObject<dyn MTLDevice>,
}

/// Encode the whole frame against `target` in one pass, for the
/// offscreen `render_layout_to_texture` paths (caller-provided target).
///
/// With `panes`, each pane's instances are drawn from its own buffers
/// rather than from the frame's lists, which then hold everything
/// else.  The order is the one a frame holding them all draws in: a
/// pane's rectangles before the rest of the rectangles, its glyphs
/// before the rest of the glyphs, its colour glyphs before the rest of
/// those.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_passes(
    cmd: &ProtocolObject<dyn MTLCommandBuffer>,
    target: &ProtocolObject<dyn MTLTexture>,
    d: &Drawing,
    cells: &[CellInstance],
    dots: &[CellInstance],
    glyphs: &[GlyphInstance],
    color_glyphs: &[GlyphInstance],
    ui_rects: &[UiRectInstance],
    overlay_cells: &[CellInstance],
    overlay_glyphs: &[GlyphInstance],
    overlay_ui_rects: &[UiRectInstance],
    viewport_w: f32,
    viewport_h: f32,
    // `Some` = refill persistent buffers (only sound on a path that
    // does not start a frame while the last is in flight); `None` =
    // allocate per frame.
    pool: Option<&mut InstanceBufferPool>,
    panes: Option<&mut [PaneInstanceCache]>,
    // Reused across frames so encoding the rects allocates nothing in
    // a steady state.
    ui_slab: &mut Vec<u8>,
) {
    let mut pool = pool;
    let viewport: [f32; 2] = [viewport_w, viewport_h];
    let viewport_ptr = NonNull::new(viewport.as_ptr() as *mut c_void).unwrap();
    let viewport_len = std::mem::size_of::<[f32; 2]>();

    // One pass for the whole frame. Clear to the window's surface
    // first: the chrome strip above the grid is not covered by any
    // opaque cell, so it relies on the clear to reset every frame.
    // (Load-by-default was tried once, 2126cda, and let the strip's
    // antialiased glyphs accumulate over their own edges.)
    //
    // The frame used to be eight passes, one per kind of instance, each
    // loading the whole target and storing it again. It is now one
    // scene: the same instances in one slab, in layers that keep the
    // order the passes drew in -- see `frame_scene`.
    let mut layers = [golia_ui_core::scene::Layer::default(); crate::frame_scene::FRAME_LAYERS];
    let frame = crate::frame_scene::FrameInstances {
        cells,
        dots,
        ui_rects,
        glyphs,
        color_glyphs,
        overlay_cells,
        overlay_ui_rects,
        overlay_glyphs,
    };
    let clip = golia_ui_core::units::RectPx::new(0.0, 0.0, viewport_w, viewport_h);
    let built = crate::frame_scene::build(&frame, clip, ui_slab, &mut layers, panes.is_some());
    let slab_buffer = match &mut pool {
        Some(p) => p.upload_slab(d.device, &ui_slab[..built.bytes]),
        None => make_instance_buffer(d.device, &ui_slab[..built.bytes]),
    };
    // Each shown pane's buffers, brought up to date before the pass.
    if let Some(p) = pool.as_mut() {
        p.drawn.clear();
        for (i, cache) in panes.into_iter().flatten().enumerate().filter(|(_, c)| c.shown) {
            p.sync_pane(d.device, i, cache);
        }
    }
    let pane_buffers: &[[(Option<Buffer>, usize); 3]] = pool.as_ref().map_or(&[], |p| &p.drawn[..]);

    let pass = { MTLRenderPassDescriptor::new() };
    // SAFETY: attachment 0 always exists on a fresh descriptor
    unsafe {
        let color = pass.colorAttachments().objectAtIndexedSubscript(0);
        color.setTexture(Some(target));
        color.setStoreAction(MTLStoreAction::Store);
        color.setLoadAction(MTLLoadAction::Clear);
        // The window's own surface, which the sidebar and the header
        // never paint over.
        color.setClearColor(clear_color(crate::ui::components::panel_palette::pane_bg(false)));
    }
    let enc = cmd.renderCommandEncoderWithDescriptor(&pass).expect("frame encoder");
    // SAFETY: the viewport lives on this stack frame until the call returns
    unsafe { enc.setVertexBytes_length_atIndex(viewport_ptr, viewport_len, 1) };
    use golia_ui_core::scene::Kind;
    let (tw, th) = (target.width(), target.height());
    let draw = |pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
                texture: Option<&ProtocolObject<dyn MTLTexture>>,
                buf: &ProtocolObject<dyn MTLBuffer>,
                offset: usize,
                count: usize| {
        enc.setRenderPipelineState(pipeline);
        // SAFETY: `buf` holds `count` instances from `offset`, and the
        // texture and sampler outlive the encoder
        unsafe {
            enc.setVertexBuffer_offset_atIndex(Some(buf), offset, 0);
            if let Some(texture) = texture {
                enc.setFragmentTexture_atIndex(Some(texture), 0);
                enc.setFragmentSamplerState_atIndex(Some(d.fg_sampler), 0);
            }
            enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::Triangle, 0, 6, count);
        }
    };
    let draw_panes = |list: usize, pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
                      texture: Option<&ProtocolObject<dyn MTLTexture>>| {
        for bufs in pane_buffers {
            if let (Some(buf), count) = &bufs[list]
                && *count > 0
            {
                draw(pipeline, texture, buf, 0, *count);
            }
        }
    };
    for (li, layer) in layers[..built.layers].iter().enumerate() {
        enc.setScissorRect(scissor_for(layer.clip, tw, th));
        for kind in Kind::ALL {
            let (pipeline, texture) = match kind {
                Kind::Rect => (d.bg_pipeline, None),
                Kind::Circle => (d.dot_pipeline, None),
                Kind::Glyph => (d.fg_pipeline, Some(d.atlas_texture)),
                Kind::ColorGlyph => (d.fg_color_pipeline, Some(d.color_atlas_texture)),
                Kind::UiRect => (d.scene_ui_pipeline, None),
                // nothing produces images yet, and there is no pipeline
                Kind::Image => continue,
            };
            match kind {
                Kind::Rect if built.pane_rects == Some(li) => draw_panes(0, pipeline, texture),
                Kind::Glyph if built.pane_glyphs == Some(li) => draw_panes(1, pipeline, texture),
                Kind::ColorGlyph if built.pane_glyphs == Some(li) => draw_panes(2, pipeline, texture),
                _ => {}
            }
            let run = layer.runs[kind as usize];
            if run.count == 0 {
                continue;
            }
            if let Some(buf) = &slab_buffer {
                draw(pipeline, texture, buf, run.offset as usize, run.count as usize);
            }
        }
    }
    enc.endEncoding();
}

/// A layer's clip as Metal's scissor, held inside the target: Metal
/// rejects a scissor that reaches past the attachment.
fn scissor_for(clip: golia_ui_core::units::RectPx, tw: usize, th: usize) -> MTLScissorRect {
    let x0 = (clip.x.max(0.0).floor() as usize).min(tw);
    let y0 = (clip.y.max(0.0).floor() as usize).min(th);
    let x1 = ((clip.x + clip.w).max(0.0).ceil() as usize).min(tw);
    let y1 = ((clip.y + clip.h).max(0.0).ceil() as usize).min(th);
    MTLScissorRect { x: x0, y: y0, width: x1.saturating_sub(x0), height: y1.saturating_sub(y0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_metal::system_default_device;

    /// The pool has one job — never allocate twice for the same
    /// shape — and one hazard: refilling a buffer the GPU might still
    /// be reading.  The hazard is handled by only using it on the
    /// path that waits for completion (see `InstanceBufferPool`);
    /// this covers the job.
    #[test]
    fn the_instance_pool_grows_once_and_then_stops_allocating() {
        let device = match system_default_device() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("SKIPPED: no Metal device: {e}");
                return;
            }
        };
        let mut pool = InstanceBufferPool::default();
        let _ = take_instance_buffer_cost();

        // First upload allocates; capacity is rounded up, so a
        // slightly larger second frame must NOT allocate again — that
        // is the whole point, and a naive `cap < len` on an exact-fit
        // buffer would reallocate on every frame that grew by a byte.
        let small = vec![7u8; 1000];
        let buf = pool.upload_slab(&device, &small).expect("first upload");
        let (_, bytes_after_first) = take_instance_buffer_cost();
        assert!(bytes_after_first > 0, "the first upload has to allocate");
        // The bytes actually landed.
        let got = unsafe {
            std::slice::from_raw_parts(buf.contents().as_ptr() as *const u8, small.len())
        };
        assert_eq!(got, &small[..], "contents must be what we uploaded");

        let bigger = vec![9u8; 60_000];
        pool.upload_slab(&device, &bigger).expect("second upload");
        let (_, bytes_after_second) = take_instance_buffer_cost();
        assert_eq!(
            bytes_after_second, 0,
            "growth inside the rounded-up capacity must not reallocate",
        );

        // Past the capacity it must grow — silently truncating would
        // corrupt the frame instead of costing an allocation.
        let huge = vec![1u8; 300_000];
        let buf = pool.upload_slab(&device, &huge).expect("third upload");
        let (_, bytes_after_third) = take_instance_buffer_cost();
        assert!(bytes_after_third >= huge.len() as u64, "must reallocate to fit");
        let got = unsafe {
            std::slice::from_raw_parts(buf.contents().as_ptr() as *const u8, huge.len())
        };
        assert_eq!(got.len(), huge.len());
        assert!(got.iter().all(|&b| b == 1), "the whole payload must land");

        // Empty input is not a buffer.
        assert!(pool.upload_slab(&device, &[]).is_none());
    }
}
