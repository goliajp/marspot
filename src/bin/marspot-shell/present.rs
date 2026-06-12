//! Shell-side Metal presenter — samples an IOSurface and draws it
//! full-window to a CAMetalLayer.
//!
//! This is the *only* GPU work the shell does.  It owns its own
//! `MTLDevice` and command queue (separate from the core's), a tiny
//! shader library (fullscreen quad + texture sampler), and a single
//! sampler.  No glyph atlas, no instance buffers — none of the
//! per-frame allocation budget the renderer thinks about.  Idle cost
//! is bounded by how often the shell calls `present()`; the redraw
//! pump in the bin runs at ~60 fps.

use core_foundation::base::TCFType;
use core_graphics::color::CGColor;
use objc2::encode::{Encoding, RefEncode};
use objc2::msg_send;

/// objc2 needs a `RefEncode` impl to dispatch a pointer through
/// `msg_send!`; CGColorRef's runtime encoding is `^{CGColor=}`.  Wrap
/// the pointer locally so the macro accepts it.
#[repr(C)]
struct OpaqueCGColor {
    _p: [u8; 0],
}
unsafe impl RefEncode for OpaqueCGColor {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGColor", &[]));
}
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_app_kit::{NSColor, NSView, NSViewLayerContentsPlacement};
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

const TARGET_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;

const SHADER_SRC: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexOut {
    float4 position [[position]];
    float2 uv;
};

// Texture-to-drawable size ratio: the quad's bottom-right corner
// lands at the texture's natural pixel size inside the drawable,
// anchored top-left.  Stale IOSurface during a window resize stays at
// its native pixel size; the drawable's exposed band is filled with
// `font_cache::BG` (set as `clearColor`), which is the *exact* same
// colour as the terminal cell BG — so the seam between the
// IOSurface region and the fill is invisible to the eye.
struct ViewParams {
    float ratio_x; // texture_w / drawable_w
    float ratio_y; // texture_h / drawable_h
};

vertex VertexOut shell_quad_vs(uint vid [[vertex_id]],
                               constant ViewParams& vp [[buffer(0)]]) {
    const float2 uvs[6] = {
        float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
        float2(0.0, 1.0), float2(1.0, 0.0), float2(1.0, 1.0),
    };
    float2 uv = uvs[vid];
    float ndc_x = uv.x == 0.0 ? -1.0 : (-1.0 + 2.0 * vp.ratio_x);
    float ndc_y = uv.y == 0.0 ?  1.0 : ( 1.0 - 2.0 * vp.ratio_y);
    VertexOut out;
    out.position = float4(ndc_x, ndc_y, 0.0, 1.0);
    out.uv = uv;
    return out;
}

fragment float4 shell_quad_fs(VertexOut in [[stage_in]],
                              texture2d<float> tex [[texture(0)]],
                              sampler smp [[sampler(0)]]) {
    return tex.sample(smp, in.uv);
}
"#;

/// Exact terminal-cell BG colour (`font_cache::BG`).  Hardcoded here
/// — duplicated rather than imported so the shell never accidentally
/// pulls in the rest of the lib's font/render surface area, but
/// any change to `font_cache::BG` must mirror here.  Same constants
/// drive `layer.backgroundColor`, `window.backgroundColor`, and the
/// render-pass clear colour so the resize fill seam is invisible.
const BG_R: f64 = 0.006;
const BG_G: f64 = 0.008;
const BG_B: f64 = 0.014;

pub struct ShellPresenter {
    /// Retained so `swap_surface` (Step 4 + 5) can rebuild the
    /// IOSurface-backed texture without re-discovering the device.
    #[allow(dead_code)]
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    layer: Retained<CAMetalLayer>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    sampler: Retained<ProtocolObject<dyn MTLSamplerState>>,
    /// MTLTexture wrapping the current shared IOSurface.  Stays valid
    /// across many `present()` calls until the shell rebuilds the
    /// surface (Step 4 resize, Step 5 supervisor swap).
    iosurface_tex: Retained<ProtocolObject<dyn MTLTexture>>,
}

impl ShellPresenter {
    pub fn new(view: &NSView, scale: f32, surface: &IOSurface) -> Result<Self, String> {
        let device = system_default_device()?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "newCommandQueue returned nil".to_string())?;

        let library = build_library(&device)?;
        let pipeline = build_pipeline(&device, &library)?;
        let sampler = build_sampler(&device)?;

        let layer = unsafe { CAMetalLayer::new() };
        unsafe {
            layer.setDevice(Some(&device));
            layer.setPixelFormat(TARGET_FORMAT);
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
            // Layer BG = exact font_cache::BG so the fill band between
            // the IOSurface (its native pixel size) and the drawable
            // edge is *colour-matched* with the terminal cell BG.
            // Visually the IOSurface looks like it extends beyond its
            // real pixels — no seam, no ghost outline.
            let cgcolor = CGColor::rgb(BG_R, BG_G, BG_B, 1.0);
            let cgcolor_ref = cgcolor.as_concrete_TypeRef() as *const OpaqueCGColor;
            let layer_ptr: *mut AnyObject = Retained::as_ptr(&layer) as *mut AnyObject;
            let _: () = msg_send![layer_ptr, setBackgroundColor: cgcolor_ref];
        }
        view.setWantsLayer(true);
        unsafe {
            // Tell NSView how to *itself* place the layer's contents
            // during a live resize.  AppKit drives this — distinct
            // from CAMetalLayer's `contentsGravity`, which only kicks
            // in once the layer compositor is involved.  When dragging
            // the bottom edge, AppKit's path takes over and we need
            // it to anchor top-left (matching the layer-side gravity)
            // instead of the default `ScaleAxesIndependently`, which
            // stretches the old layer contents into the new view
            // bounds and reads as content shifting up/down during the
            // drag.  We do NOT set
            // `setLayerContentsRedrawPolicy(.DuringViewResize)` — that
            // policy tells AppKit to invoke `displayLayer:`/`drawRect:`
            // each resize step, which fights with our own
            // `nextDrawable` + `commit` path and blanks the layer.
            view.setLayerContentsPlacement(NSViewLayerContentsPlacement::TopLeft);
            view.setLayer(Some(&layer));
            // NSWindow BG mirrors layer BG for the same reason.
            if let Some(window) = view.window() {
                let bg =
                    NSColor::colorWithSRGBRed_green_blue_alpha(BG_R, BG_G, BG_B, 1.0);
                window.setBackgroundColor(Some(&bg));
            }
        }

        let iosurface_tex = surface.make_metal_texture(&device)?;

        Ok(Self {
            device,
            queue,
            layer,
            pipeline,
            sampler,
            iosurface_tex,
        })
    }

    /// Swap the IOSurface backing the presenter.  Used during resize
    /// (new surface for new dimensions) and during supervisor swap
    /// (new surface ID after core restart, if we ever do that).
    #[allow(dead_code)]
    pub fn swap_surface(&mut self, surface: &IOSurface) -> Result<(), String> {
        self.iosurface_tex = surface.make_metal_texture(&self.device)?;
        Ok(())
    }

    /// Update the CAMetalLayer drawable size after the window resizes.
    /// Cheap on Apple Silicon — the layer pools drawables.
    pub fn set_drawable_size(&self, w_phys: f64, h_phys: f64) {
        let w = w_phys.max(1.0);
        let h = h_phys.max(1.0);
        unsafe {
            self.layer
                .setDrawableSize(objc2_foundation::NSSize::new(w, h));
        }
    }

    /// Render the IOSurface to the next CAMetalLayer drawable and
    /// present.  No-op if no drawable is available (all in-flight).
    pub fn present(&mut self) {
        let drawable = match unsafe { self.layer.nextDrawable() } {
            Some(d) => d,
            None => return,
        };
        let texture = unsafe { drawable.texture() };

        let pass = unsafe { MTLRenderPassDescriptor::new() };
        let color0 = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        color0.setTexture(Some(&texture));
        color0.setLoadAction(MTLLoadAction::Clear);
        color0.setStoreAction(MTLStoreAction::Store);
        // Clear = exact font_cache::BG.  The IOSurface quad covers
        // (0,0)-(tex_w,tex_h); everything else in the drawable is this
        // colour, which matches what the terminal grid would have
        // painted there if it were still part of the rendered view.
        color0.setClearColor(MTLClearColor {
            red: BG_R,
            green: BG_G,
            blue: BG_B,
            alpha: 1.0,
        });

        // Top-left anchored ratio so the IOSurface is shown at its
        // own pixel size, not stretched.  Combined with the BG-matched
        // clearColor this reads as "the terminal grew" rather than
        // "the content stretched" or "there's a fill outline".
        let draw_w = texture.width() as f32;
        let draw_h = texture.height() as f32;
        let tex_w = self.iosurface_tex.width() as f32;
        let tex_h = self.iosurface_tex.height() as f32;
        let ratio_x = if draw_w > 0.0 { tex_w / draw_w } else { 1.0 };
        let ratio_y = if draw_h > 0.0 { tex_h / draw_h } else { 1.0 };
        let view_params: [f32; 2] = [ratio_x, ratio_y];

        let cmd = match self.queue.commandBuffer() {
            Some(c) => c,
            None => return,
        };
        let encoder = match cmd.renderCommandEncoderWithDescriptor(&pass) {
            Some(e) => e,
            None => return,
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
            encoder.setFragmentTexture_atIndex(Some(&self.iosurface_tex), 0);
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
        cmd.waitUntilScheduled();
        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        mtl_drawable.present();
    }
}

fn system_default_device() -> Result<Retained<ProtocolObject<dyn MTLDevice>>, String> {
    // Mirror render_metal::system_default_device — kept private to the
    // bin so the shell doesn't pull lib internals across the boundary.
    let raw = unsafe { MTLCreateSystemDefaultDevice() };
    if raw.is_null() {
        return Err("MTLCreateSystemDefaultDevice returned nil — no Metal device".into());
    }
    unsafe { Retained::from_raw(raw) }
        .ok_or_else(|| "MTLCreateSystemDefaultDevice → null after non-null check".into())
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
