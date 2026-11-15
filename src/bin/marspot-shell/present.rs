//! The presenter that copies: samples an IOSurface and draws it into a
//! `CAMetalLayer`'s drawable.
//!
//! Only for a core that cannot be told when its surface has left the
//! screen (see `frame_layer`), and made only when such a core is met:
//! it has its own `MTLDevice`, command queue and one small shader, and
//! the driver's pool that comes with executing anything on the GPU.

use core_foundation::base::TCFType;
use core_graphics::color::CGColor;
use objc2::encode::{Encoding, RefEncode};
use objc2::msg_send;

/// objc2 needs a `RefEncode` impl to dispatch a pointer through
/// `msg_send!`; CGColorRef's runtime encoding is `^{CGColor=}`.  Wrap
/// the pointer locally so the macro accepts it.
#[repr(C)]
pub(crate) struct OpaqueCGColor {
    _p: [u8; 0],
}
unsafe impl RefEncode for OpaqueCGColor {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
}
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_foundation::NSString;
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLDrawable, MTLLibrary, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLSamplerDescriptor, MTLSamplerMinMagFilter, MTLSamplerState,
    MTLStoreAction, MTLTexture,
};
use objc2_quartz_core::{kCAGravityTopLeft, CAMetalDrawable, CAMetalLayer};

use marspot::iosurface::IOSurface;
use marspot::latency_trace::{self as trace, Stage};

const TARGET_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

use crate::present_shaders::SHADER_SRC;

/// The terminal's background under the theme in force.  The same value
/// drives `layer.backgroundColor`, `window.backgroundColor`, and the
/// render-pass clear colour so the resize fill seam is invisible.
fn terminal_bg() -> (f64, f64, f64) {
    marspot_term::palette::current().bg
}

/// `layer.backgroundColor`, set when the layer is made and again when
/// the theme changes.
pub(crate) fn set_layer_background(layer: &Retained<CAMetalLayer>) {
    let (r, g, b) = terminal_bg();
    let cgcolor = CGColor::rgb(r, g, b, 1.0);
    let cgcolor_ref = cgcolor.as_concrete_TypeRef() as *const OpaqueCGColor;
    let layer_ptr: *mut AnyObject = Retained::as_ptr(layer) as *mut AnyObject;
    let _: () = unsafe { msg_send![layer_ptr, setBackgroundColor: cgcolor_ref] };
}

pub struct ShellPresenter {
    /// Retained so `set_pair` can rebuild the IOSurface-backed
    /// textures (on resize / pending-update swap) without
    /// re-discovering the device.
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pub(crate) layer: Retained<CAMetalLayer>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    /// PROTO_VERSION=2 double-buffer: two MTLTextures wrap a pair of
    /// shared IOSurfaces (front + back).  Core writes the back surface;
    /// when it acks `SurfaceReady(id)`, `swap_to_id` flips
    /// `current_idx` so the next `present()` samples the just-completed
    /// surface.  Eliminates the cross-process mid-render race that was
    /// the dominant flash source.
    iosurface_tex: [Retained<ProtocolObject<dyn MTLTexture>>; 2],
    /// Surface IDs in the same slot order as `iosurface_tex`.  Used by
    /// `swap_to_id` to resolve which slot just became renderable.
    surface_ids: [u32; 2],
    /// Slot currently being sampled by `present()`.  Flipped by
    /// `swap_to_id` when the core acks a SurfaceReady whose ID matches
    /// the OTHER slot — i.e. the back has become the new front.
    current_idx: u8,
}

impl ShellPresenter {
    /// The layer is the caller's to place over the window.
    pub fn new(scale: f32, front: &IOSurface, back: &IOSurface) -> Result<Self, String> {
        let device = system_default_device()?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "newCommandQueue returned nil".to_string())?;

        let t0 = std::time::Instant::now();
        let library = build_library(&device)?;
        let t1 = std::time::Instant::now();
        let pipeline = build_pipeline(&device, &library)?;
        marspot::lx_event!(
            "PRESENTER_SHADERS",
            "shader libraries and pipelines built",
            library_us = (t1 - t0).as_micros() as u64,
            pipelines_us = t1.elapsed().as_micros() as u64
        );
        let sampler = build_sampler(&device)?;

        let layer = { CAMetalLayer::new() };
        unsafe {
            layer.setDevice(Some(&device));
            layer.setPixelFormat(TARGET_FORMAT);
            // Tag the layer's colour space via the SAME shared helper the
            // standalone renderer uses (Display P3 — matches iTerm2's deeper
            // reds on wide-gamut panels; sRGB-tagging read washed/pink).
            marspot::render_metal::pin_layer_colorspace(&layer);
            layer.setFramebufferOnly(true);
            layer.setContentsScale(scale as f64);
            // Glitchless live-resize recipe (matches the metal-live-resize
            // crate the user wrote for this exact problem):
            //   1. contentsGravity = topLeft — pin stale frames to the
            //      top-left of the layer instead of letting the
            //      compositor stretch them into the new bounds.
            //   2. contentsScale = backingScaleFactor — already set above
            //      from the caller-supplied scale.
            //   3. Per-frame, read `drawable.texture.width/height` —
            //      done in `present()`.
            //
            // Intentionally NOT setting `setOpaque(true)` — the demo
            // example in metal-live-resize doesn't either.  With
            // opaque=true the compositor treats uncovered layer area
            // (the band between the stale top-left-pinned drawable
            // and the new layer bounds) as if it must be opaque
            // without telling it what colour, which shows up as a
            // "ghost outline" of the previous frame.  Leaving the
            // layer non-opaque lets the NSWindow's `backgroundColor`
            // (set below) bleed through that band cleanly.
            layer.setContentsGravity(kCAGravityTopLeft);
            // Host view (`MarspotView` in app.rs) is `isFlipped=true`
            // — origin top-left, Y down.  CAMetalLayer's default
            // geometry is bottom-left, Y up.  Without
            // `setGeometryFlipped(true)`, `contentsGravity = topLeft`
            // is interpreted in the layer's native (un-flipped) coord
            // space — i.e. "top" actually means the *bottom* of the
            // view, and old drawables get pinned there instead of the
            // visual top.  During a live resize this reads as content
            // jumping up and down as the layer bounds change in one
            // coord system while the gravity-pin tries to honour them
            // in another.
            layer.setGeometryFlipped(true);
            // Sync `presentDrawable` with AppKit's CATransaction.  In
            // default async mode the drawable is presented whenever
            // the compositor next runs, which during a live resize
            // can lag a frame or two behind the window-bounds change
            // and shows up as visible content-vs-bounds drift.  With
            // `presentsWithTransaction = true` the drawable is held
            // until the next CATransaction commits — same transaction
            // AppKit uses for resize — so the drawable and the new
            // bounds appear in the same frame, the way Sublime Text /
            // Xcode / iTerm2 do it.  Caller pays for this by issuing
            // `commandBuffer.waitUntilScheduled()` + `drawable.present()`
            // by hand instead of letting `presentDrawable` schedule.
            layer.setPresentsWithTransaction(true);
            // Layer BG = the terminal's background so the fill band between
            // the IOSurface (its native pixel size) and the drawable
            // edge is *colour-matched* with the terminal cell BG.
            // Visually the IOSurface looks like it extends beyond its
            // real pixels — no seam, no ghost outline.
            set_layer_background(&layer);
        }
        let tex0 = front.make_metal_texture(&device)?;
        let tex1 = back.make_metal_texture(&device)?;
        let iosurface_tex = [tex0, tex1];
        let surface_ids = [front.id(), back.id()];

        Ok(Self {
            device,
            queue,
            layer,
            pipeline,
            sampler,
            iosurface_tex,
            surface_ids,
            current_idx: 0,
        })
    }

    /// Rebuild both MTLTexture slots from a fresh IOSurface pair.
    /// Used on resize (new pair at new dims) and on silent-update
    /// promote (pending core's pair becomes live).  `current_idx`
    /// resets to 0 — the next `SurfaceReady` from the core picks the
    /// real front; until then `present()` samples slot 0 (which the
    /// caller is expected to have rendered into at least once before
    /// calling, or else it shows the IOSurface's default-init state).
    pub fn set_pair(
        &mut self,
        front: &IOSurface,
        back: &IOSurface,
    ) -> Result<(), String> {
        let tex0 = front.make_metal_texture(&self.device)?;
        let tex1 = back.make_metal_texture(&self.device)?;
        self.iosurface_tex = [tex0, tex1];
        self.surface_ids = [front.id(), back.id()];
        self.current_idx = 0;
        Ok(())
    }

    /// Honour a SurfaceReady(id) ack: if the id matches one of the
    /// pair's slot ids, flip `current_idx` so the next present samples
    /// it.  Returns `true` on a recognised id (caller can mark
    /// `frame_pending` and request a redraw); `false` if the id
    /// doesn't belong to the current pair (e.g. a late ack from a
    /// retired pair after resize — the caller should ignore it).
    pub fn swap_to_id(&mut self, id: u32) -> bool {
        if self.surface_ids[0] == id {
            self.current_idx = 0;
            true
        } else if self.surface_ids[1] == id {
            self.current_idx = 1;
            true
        } else {
            false
        }
    }

    /// The surface the next copy samples.
    #[cfg(feature = "latency-trace")]
    pub(crate) fn current_id(&self) -> u32 {
        self.surface_ids[self.current_idx as usize]
    }

    /// Update the CAMetalLayer drawable size after the window resizes,
    /// and its scale: a window moved to a display of another density
    /// has the same size in points and a different one in pixels.
    pub fn set_drawable_size(&self, w_phys: f64, h_phys: f64, scale: f64) {
        let w = w_phys.max(1.0);
        let h = h_phys.max(1.0);
        self.layer.setContentsScale(scale);
        self.layer.setDrawableSize(objc2_foundation::NSSize::new(w, h));
    }

    /// Render the IOSurface to the next CAMetalLayer drawable and
    /// present.  No-op if no drawable is available (all in-flight).
    /// False when nothing was drawn (no drawable, no command buffer),
    /// so the caller keeps the frame pending.
    pub fn present(&mut self) -> bool {
        let t_begin = trace::now_ns();
        trace::mark_at(Stage::PresentBegin, 0, t_begin);
        let drawable = match self.layer.nextDrawable() {
            Some(d) => d,
            None => {
                // Dev-only — flash investigation 2026-06-15.  If
                // this fires bursty + frequently it may correlate
                // with the visible flash (presenter drops a frame
                // → prior drawable's content stays for one extra
                // refresh interval, no flash on its own, but a
                // cluster could indicate scheduling pressure that
                // also produces the cross-process IOSurface race
                // on neighboring frames).
                marspot::lx_debug!(
                    "shell.present.no_drawable",
                    "nextDrawable returned None — frame skipped"
                );
                return false;
            }
        };
        let texture = { drawable.texture() };
        let t_drawable = trace::now_ns();

        let pass = { MTLRenderPassDescriptor::new() };
        let color0 = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        color0.setTexture(Some(&texture));
        color0.setLoadAction(MTLLoadAction::Clear);
        color0.setStoreAction(MTLStoreAction::Store);
        // Clear = the terminal's background.  The IOSurface quad covers
        // (0,0)-(tex_w,tex_h); everything else in the drawable is this
        // colour, which matches what the terminal grid would have
        // painted there if it were still part of the rendered view.
        let (red, green, blue) = terminal_bg();
        color0.setClearColor(MTLClearColor { red, green, blue, alpha: 1.0 });

        // Top-left anchored ratio so the IOSurface is shown at its
        // own pixel size, not stretched.  Combined with the BG-matched
        // clearColor this reads as "the terminal grew" rather than
        // "the content stretched" or "there's a fill outline".
        let draw_w = texture.width() as f32;
        let draw_h = texture.height() as f32;
        let front_tex = &self.iosurface_tex[self.current_idx as usize];
        let tex_w = front_tex.width() as f32;
        let tex_h = front_tex.height() as f32;
        let ratio_x = if draw_w > 0.0 { tex_w / draw_w } else { 1.0 };
        let ratio_y = if draw_h > 0.0 { tex_h / draw_h } else { 1.0 };
        let view_params: [f32; 2] = [ratio_x, ratio_y];

        let cmd = match self.queue.commandBuffer() {
            Some(c) => c,
            None => return false,
        };
        let encoder = match cmd.renderCommandEncoderWithDescriptor(&pass) {
            Some(e) => e,
            None => return false,
        };

        encoder.setRenderPipelineState(&self.pipeline);
        unsafe {
            let vp_nn = std::ptr::NonNull::new(
                view_params.as_ptr() as *mut std::ffi::c_void,
            )
            .expect("view_params stack ptr non-null");
            encoder.setVertexBytes_length_atIndex(
                vp_nn,
                std::mem::size_of_val(&view_params),
                0,
            );
            encoder.setFragmentTexture_atIndex(Some(front_tex), 0);
            encoder.setFragmentSamplerState_atIndex(Some(&self.sampler), 0);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 6);
        }

        encoder.endEncoding();

        // `presentsWithTransaction = true` path:
        //   1. commit the command buffer
        //   2. waitUntilScheduled — block until the GPU has picked up
        //      our work; after this the drawable's pixels are
        //      ready-to-display
        //   3. drawable.present() — hand the drawable to the next
        //      CATransaction.  AppKit's resize batches into the same
        //      transaction, so window bounds and our pixels land
        //      together
        cmd.commit();
        let t_commit = trace::now_ns();
        cmd.waitUntilScheduled();
        let t_scheduled = trace::now_ns();
        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        mtl_drawable.present();
        if trace::enabled() {
            trace::mark(Stage::PresentEnd, 0);
            trace::mark_at(Stage::DrawableUs, ((t_drawable - t_begin) / 1000) as u32, 0);
            trace::mark_at(Stage::ScheduledUs, ((t_scheduled - t_commit) / 1000) as u32, 0);
            #[cfg(feature = "latency-trace")]
            if let Some(s) = IOSurface::lookup(self.surface_ids[self.current_idx as usize]) {
                trace::mark_at(Stage::SurfaceHash, s.pixel_hash(), 0);
            }
            trace::flush();
        }
        true
    }
}

fn system_default_device() -> Result<Retained<ProtocolObject<dyn MTLDevice>>, String> {
    // Mirror render_metal::system_default_device — kept private to the
    // bin so the shell doesn't pull lib internals across the boundary.
    // objc2-metal 0.3 wraps the +1 retained pointer (or null) into
    // `Option<Retained<..>>` for us.
    MTLCreateSystemDefaultDevice()
        .ok_or_else(|| "MTLCreateSystemDefaultDevice returned nil — no Metal device".into())
}

fn build_library(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLLibrary>>, String> {
    let src = NSString::from_str(SHADER_SRC);
    device
        .newLibraryWithSource_options_error(&src, None)
        .map_err(|e| format!("newLibraryWithSource error: {e:?}"))
}

fn build_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = library
        .newFunctionWithName(&NSString::from_str("shell_quad_vs"))
        .ok_or_else(|| "shell_quad_vs not found".to_string())?;
    let ffn = library
        .newFunctionWithName(&NSString::from_str("shell_quad_fs"))
        .ok_or_else(|| "shell_quad_fs not found".to_string())?;

    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(Some(&vfn));
    desc.setFragmentFunction(Some(&ffn));
    let color0 = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
    color0.setPixelFormat(TARGET_FORMAT);

    device
        .newRenderPipelineStateWithDescriptor_error(&desc)
        .map_err(|e| format!("newRenderPipelineState error: {e:?}"))
}

fn build_sampler(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>, String> {
    let desc = MTLSamplerDescriptor::new();
    // Linear sampling: at steady state, source and dest are 1:1, so
    // Linear and Nearest produce identical output.  In the brief
    // resize window before the core has rebuilt the IOSurface at the
    // new dimensions (Step 4's ack-and-swap), Linear softens the
    // stretched intermediate frame so the snap-to-crisp transition
    // reads as a gradual sharpening rather than a "flash".
    desc.setMinFilter(MTLSamplerMinMagFilter::Linear);
    desc.setMagFilter(MTLSamplerMinMagFilter::Linear);
    device
        .newSamplerStateWithDescriptor(&desc)
        .ok_or_else(|| "newSamplerStateWithDescriptor returned nil".to_string())
}
