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

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLLoadAction, MTLPixelFormat,
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

vertex VertexOut shell_quad_vs(uint vid [[vertex_id]]) {
    // Two-triangle quad covering NDC.  UV origin top-left maps to
    // texture origin top-left (IOSurface BGRA stored top-down).
    const float2 positions[6] = {
        float2(-1.0,  1.0), float2( 1.0,  1.0), float2(-1.0, -1.0),
        float2(-1.0, -1.0), float2( 1.0,  1.0), float2( 1.0, -1.0),
    };
    const float2 uvs[6] = {
        float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0),
        float2(0.0, 1.0), float2(1.0, 0.0), float2(1.0, 1.0),
    };
    VertexOut out;
    out.position = float4(positions[vid], 0.0, 1.0);
    out.uv = uvs[vid];
    return out;
}

fragment float4 shell_quad_fs(VertexOut in [[stage_in]],
                              texture2d<float> tex [[texture(0)]],
                              sampler smp [[sampler(0)]]) {
    return tex.sample(smp, in.uv);
}
"#;

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
            // Mirror MetalRenderer's resize-flicker fix.
            layer.setContentsGravity(kCAGravityTopLeft);
            layer.setOpaque(true);
        }
        view.setWantsLayer(true);
        unsafe {
            view.setLayer(Some(&layer));
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
        // Window BG = black until the IOSurface lands; the core's first
        // frame replaces it.  Picked dark to match the terminal BG we
        // use elsewhere (font_cache::BG ≈ #1a1d23).
        color0.setClearColor(MTLClearColor {
            red: 0.10,
            green: 0.11,
            blue: 0.14,
            alpha: 1.0,
        });

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
            encoder.setFragmentTexture_atIndex(Some(&self.iosurface_tex), 0);
            encoder.setFragmentSamplerState_atIndex(Some(&self.sampler), 0);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 6);
        }
        encoder.endEncoding();

        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        cmd.presentDrawable(mtl_drawable);
        cmd.commit();
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
    // Nearest sampling: the IOSurface dims match the drawable when the
    // core renders at the same physical-pixel resolution as the shell
    // window.  Linear would smear during the resize gap before Step 4.
    desc.setMinFilter(MTLSamplerMinMagFilter::Nearest);
    desc.setMagFilter(MTLSamplerMinMagFilter::Nearest);
    device
        .newSamplerStateWithDescriptor(&desc)
        .ok_or_else(|| "newSamplerStateWithDescriptor returned nil".to_string())
}
