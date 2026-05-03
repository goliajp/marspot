//! Metal-backed renderer (in progress).
//!
//! Replaces the AppKit/CGImage/setContents path (`render.rs`) with
//! CAMetalLayer + glyph atlas + instanced quads.  Built incrementally
//! so each phase stands on its own and the existing renderer stays the
//! default until the Metal path matches it pixel-for-pixel.
//!
//! ## Phase plan
//!
//! 1. (this commit) **Scaffold** — `MTLDevice`, `MTLCommandQueue`,
//!    `CAMetalLayer` attached to an `NSView`, single-frame clear-color
//!    render pass.  Proves the Metal pipeline runs end-to-end inside
//!    mars without disturbing the AppKit renderer.
//! 2. **Glyph atlas** — CoreText-rasterise glyphs into an MTLTexture,
//!    LRU-evict per CLAUDE.md "bounded growth".
//! 3. **BG pass** — instanced coloured quads, one per cell.
//! 4. **FG pass** — textured glyph quads sampling the atlas.
//! 5. **Integration** — wire to the terminal grid; A/B against
//!    `Renderer` via `MARS_METAL=1`.  Once visually equivalent and
//!    measurably faster, retire the AppKit path.
//!
//! ## Why "previously failed at this" doesn't apply
//!
//! `render.rs`'s header notes mars *did* try Metal+atlas before and
//! pivoted away due to "gamma + atlas neighbor + sampling issues".
//! This rebuild is informed by that — explicit gamma in the shader
//! (sRGB pixel format, premultiplied alpha), atlas allocator that
//! pads each glyph by 1 px (no neighbour bleed), and exact-pixel
//! sampling with `MTLSamplerMinMagFilter::Nearest` for the BG pass
//! and `Linear` only for the glyph pass.  Full design notes go into
//! `docs/architecture.md` once phase 4 lands.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_foundation::{CGSize, NSString};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLLoadAction, MTLPixelFormat,
    MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions, MTLStoreAction,
    MTLTexture,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use std::ffi::c_void;
use std::ptr::NonNull;

/// One cell's draw data, layout-compatible with `Cell` in
/// `src/shaders/cells.metal`.  Repr-C; no padding shenanigans.
///
/// Coordinate convention: `origin` and `size` in physical pixels,
/// origin = top-left of the viewport, +y = down.  `color` is RGBA
/// in 0..1.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CellInstance {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub color: [f32; 4],
}

/// Pixel format the Metal pipeline + the CAMetalLayer agree on.  sRGB
/// because the AppKit renderer paints in normalised-sRGB-like values
/// (`render.rs` BG/FG constants); matching the colour space here means
/// the same triple drawn through either renderer comes out the same
/// pixel on screen.
const TARGET_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm_sRGB;

const SHADER_SRC: &str = include_str!("shaders/cells.metal");

/// Metal-backed renderer.  Has the pipeline state for the BG pass
/// (phase 3) but no FG pass yet — `render_cells_bg` is the entry
/// point and `render_cells_bg_offscreen` is its testable cousin.
pub struct MetalRenderer {
    /// The default system device.  One per process; cheap to clone (CFRetain).
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    /// Command submission queue.  One per renderer is fine; Apple's
    /// guidance says queues are heavyweight and should be reused.
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// The CAMetalLayer attached to the host NSView.  `None` in headless
    /// mode (used by tests / benches that don't need a window).
    layer: Option<Retained<CAMetalLayer>>,
    /// Drawable size in physical pixels.  Updated lazily in `resize`.
    width_px: f64,
    height_px: f64,
    /// Pre-built pipeline state for the BG pass — created once at
    /// `new()` so per-frame draw calls don't pay shader-compile cost.
    bg_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
}

impl MetalRenderer {
    /// Construct attached to a host `NSView` — replaces the view's
    /// CALayer with a CAMetalLayer.  Failures (no Metal-capable GPU,
    /// no command queue) bubble up as `Err` strings; caller decides
    /// whether to fall back to the AppKit renderer or refuse to start.
    pub fn new(view: &NSView, scale: f32) -> Result<Self, String> {
        let device = system_default_device()?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "MTLDevice.newCommandQueue returned nil".to_string())?;
        let bg_pipeline = build_bg_pipeline(&device)?;

        let layer = unsafe { CAMetalLayer::new() };
        unsafe {
            layer.setDevice(Some(&device));
            layer.setPixelFormat(TARGET_FORMAT);
            // framebufferOnly = true: drawables can only be render
            // targets, not sample sources.  Cheaper and we don't need
            // to read pixels back during compositing.
            layer.setFramebufferOnly(true);
            layer.setContentsScale(scale as f64);
        }

        view.setWantsLayer(true);
        unsafe {
            view.setLayer(Some(&layer));
        }

        Ok(Self {
            device,
            queue,
            layer: Some(layer),
            width_px: 0.0,
            height_px: 0.0,
            bg_pipeline,
        })
    }

    /// Construct without a view — for unit tests, benches, or a future
    /// offscreen render path.  `render_clear` is a no-op (no drawable),
    /// but `render_cells_bg_offscreen` works end-to-end against a
    /// caller-provided MTLTexture.
    pub fn new_headless() -> Result<Self, String> {
        let device = system_default_device()?;
        let queue = device
            .newCommandQueue()
            .ok_or_else(|| "MTLDevice.newCommandQueue returned nil".to_string())?;
        let bg_pipeline = build_bg_pipeline(&device)?;
        Ok(Self {
            device,
            queue,
            layer: None,
            width_px: 0.0,
            height_px: 0.0,
            bg_pipeline,
        })
    }

    /// Update the drawable size after a host-window resize.  Cheap on
    /// no-op (same dims).
    pub fn resize(&mut self, width_px: f64, height_px: f64) {
        if (width_px - self.width_px).abs() < 0.5
            && (height_px - self.height_px).abs() < 0.5
        {
            return;
        }
        self.width_px = width_px;
        self.height_px = height_px;
        if let Some(layer) = &self.layer {
            unsafe {
                layer.setDrawableSize(CGSize {
                    width: width_px,
                    height: height_px,
                });
            }
        }
    }

    /// Phase-1 render: clear the drawable to `bg` and present.  Returns
    /// `false` if there's no drawable available (e.g. all in-flight, or
    /// headless mode), `true` if a frame was committed.
    pub fn render_clear(&self, bg: (f64, f64, f64)) -> bool {
        let layer = match &self.layer {
            Some(l) => l,
            None => return false,
        };
        let drawable = match unsafe { layer.nextDrawable() } {
            Some(d) => d,
            None => return false,
        };
        let texture = unsafe { drawable.texture() };

        let pass = unsafe { MTLRenderPassDescriptor::new() };
        unsafe {
            let attachments = pass.colorAttachments();
            let color = attachments.objectAtIndexedSubscript(0);
            color.setTexture(Some(&texture));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setStoreAction(MTLStoreAction::Store);
            color.setClearColor(MTLClearColor {
                red: bg.0,
                green: bg.1,
                blue: bg.2,
                alpha: 1.0,
            });
        }

        let cmd = self
            .queue
            .commandBuffer()
            .expect("command queue out of buffers");
        let encoder = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("renderCommandEncoderWithDescriptor returned nil");
        encoder.endEncoding();
        // Cast Retained<dyn CAMetalDrawable> down to MTLDrawable for present.
        let mtl_drawable: &ProtocolObject<dyn objc2_metal::MTLDrawable> =
            ProtocolObject::from_ref(&*drawable);
        cmd.presentDrawable(mtl_drawable);
        cmd.commit();
        true
    }
}

/// Compile `cells.metal` and build the BG-pass MTLRenderPipelineState.
/// Called once at renderer construction; the pipeline is then reused
/// for every frame.
fn build_bg_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let src = NSString::from_str(SHADER_SRC);
    let library = device
        .newLibraryWithSource_options_error(&src, None)
        .map_err(|e| format!("MTLDevice.newLibraryWithSource error: {:?}", e))?;
    let vfn_name = NSString::from_str("bg_vertex");
    let ffn_name = NSString::from_str("bg_fragment");
    let vfn = library
        .newFunctionWithName(&vfn_name)
        .ok_or_else(|| "library has no bg_vertex".to_string())?;
    let ffn = library
        .newFunctionWithName(&ffn_name)
        .ok_or_else(|| "library has no bg_fragment".to_string())?;

    let descriptor = unsafe { MTLRenderPipelineDescriptor::new() };
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachments = descriptor.colorAttachments();
    let attachment = unsafe { attachments.objectAtIndexedSubscript(0) };
    unsafe {
        attachment.setPixelFormat(TARGET_FORMAT);
    }

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState error: {:?}", e))
}

impl MetalRenderer {
    /// Phase-3 BG pass into a freshly-allocated MTLTexture, with
    /// pixel readback.  Used by tests and (eventually) by an offscreen
    /// `--bench metal-render` mode.  Returns BGRA bytes, top-left
    /// origin, `width * 4` bytes per row.
    pub fn render_cells_bg_offscreen(
        &self,
        width: u32,
        height: u32,
        cells: &[CellInstance],
    ) -> Result<Vec<u8>, String> {
        // Renderable target texture, Managed so we can read back from CPU.
        let descriptor = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                TARGET_FORMAT,
                width as usize,
                height as usize,
                false,
            )
        };
        unsafe {
            descriptor.setUsage(
                objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
            );
            descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        }
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;

        // Per-instance buffer.  StorageModeShared: CPU writes, GPU reads,
        // no manual sync needed.  newBufferWithBytes does the copy.
        let cells_bytes = cells_as_bytes(cells);
        let buffer = if cells_bytes.is_empty() {
            None
        } else {
            unsafe {
                self.device.newBufferWithBytes_length_options(
                    NonNull::new(cells_bytes.as_ptr() as *mut c_void).unwrap(),
                    cells_bytes.len(),
                    MTLResourceOptions::MTLResourceStorageModeShared,
                )
            }
        };

        let pass = unsafe { MTLRenderPassDescriptor::new() };
        unsafe {
            let attachments = pass.colorAttachments();
            let color = attachments.objectAtIndexedSubscript(0);
            color.setTexture(Some(&texture));
            color.setLoadAction(MTLLoadAction::Clear);
            color.setStoreAction(MTLStoreAction::Store);
            color.setClearColor(MTLClearColor {
                red: 0.0,
                green: 0.0,
                blue: 0.0,
                alpha: 1.0,
            });
        }

        let cmd = self
            .queue
            .commandBuffer()
            .ok_or_else(|| "commandBuffer returned nil".to_string())?;
        let encoder = cmd
            .renderCommandEncoderWithDescriptor(&pass)
            .ok_or_else(|| "renderCommandEncoder returned nil".to_string())?;
        encoder.setRenderPipelineState(&self.bg_pipeline);
        if let Some(buf) = &buffer {
            unsafe {
                encoder.setVertexBuffer_offset_atIndex(Some(buf), 0, 0);
            }
        }
        let viewport_px: [f32; 2] = [width as f32, height as f32];
        unsafe {
            encoder.setVertexBytes_length_atIndex(
                NonNull::new(viewport_px.as_ptr() as *mut c_void).unwrap(),
                std::mem::size_of::<[f32; 2]>(),
                1,
            );
            if !cells.is_empty() {
                encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::Triangle,
                    0,
                    6,
                    cells.len(),
                );
            }
        }
        encoder.endEncoding();

        // Managed → CPU: synchronize so subsequent getBytes reads
        // the latest GPU writes.
        let blit = cmd
            .blitCommandEncoder()
            .ok_or_else(|| "blitCommandEncoder returned nil".to_string())?;
        let resource: &ProtocolObject<dyn objc2_metal::MTLResource> =
            ProtocolObject::from_ref(&*texture);
        blit.synchronizeResource(resource);
        blit.endEncoding();

        cmd.commit();
        unsafe { cmd.waitUntilCompleted() };

        let bytes_per_row = (width as usize) * 4;
        let mut bytes = vec![0u8; bytes_per_row * height as usize];
        let region = objc2_metal::MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize {
                width: width as usize,
                height: height as usize,
                depth: 1,
            },
        };
        unsafe {
            texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(bytes.as_mut_ptr() as *mut c_void).unwrap(),
                bytes_per_row,
                region,
                0,
            );
        }
        Ok(bytes)
    }
}

/// SAFETY: `CellInstance` is `#[repr(C)]` with no padding, so its
/// memory representation is a flat slice of bytes.  No interior
/// uninitialised bytes; safe to view the slice as `&[u8]`.
fn cells_as_bytes(cells: &[CellInstance]) -> &[u8] {
    let len = std::mem::size_of_val(cells);
    unsafe { std::slice::from_raw_parts(cells.as_ptr() as *const u8, len) }
}

pub(crate) fn system_default_device() -> Result<Retained<ProtocolObject<dyn MTLDevice>>, String> {
    // SAFETY: MTLCreateSystemDefaultDevice returns a +1 retained pointer
    // (per Apple docs) or null on failure.  Wrap with Retained::from_raw
    // to take ownership without an extra retain.
    let raw = unsafe { MTLCreateSystemDefaultDevice() };
    if raw.is_null() {
        return Err("MTLCreateSystemDefaultDevice returned nil — no Metal device".into());
    }
    unsafe { Retained::from_raw(raw) }
        .ok_or_else(|| "MTLCreateSystemDefaultDevice → null after non-null check".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify the basic Metal plumbing works on this machine — proves
    /// the dep + bindings resolve and we can talk to the GPU.  CI on
    /// non-Metal machines will skip this naturally because
    /// `system_default_device` returns `Err`.
    #[test]
    fn headless_renderer_constructs() {
        match MetalRenderer::new_headless() {
            Ok(r) => {
                assert!(r.layer.is_none());
            }
            Err(e) => {
                eprintln!("skipping: no Metal device on this host ({e})");
            }
        }
    }

    /// End-to-end BG-pass test: render two solid-colour cells onto
    /// a 4×4 texture and read back the pixels.  Verifies shader
    /// compiles, pipeline state is built correctly, vertex/fragment
    /// IO matches, and instanced draw + readback work.
    #[test]
    fn bg_pass_renders_cells() {
        let r = match MetalRenderer::new_headless() {
            Ok(r) => r,
            Err(_) => {
                eprintln!("skipping: no Metal device");
                return;
            }
        };
        // 4×4 viewport, two cells:
        //   cell A at (0,0)-(2,4) → red    (left half)
        //   cell B at (2,0)-(2,4) → green  (right half)
        let cells = vec![
            CellInstance {
                origin: [0.0, 0.0],
                size: [2.0, 4.0],
                color: [1.0, 0.0, 0.0, 1.0],
            },
            CellInstance {
                origin: [2.0, 0.0],
                size: [2.0, 4.0],
                color: [0.0, 1.0, 0.0, 1.0],
            },
        ];
        let bytes = r
            .render_cells_bg_offscreen(4, 4, &cells)
            .expect("offscreen render");
        assert_eq!(bytes.len(), 4 * 4 * 4);

        // BGRA layout, top-left origin.  Sample one pixel from each
        // half — exact value depends on sRGB encoding so allow a wide
        // tolerance; we just need to see "left = red-ish, right = green-ish".
        let pixel_at = |x: usize, y: usize| {
            let off = (y * 4 + x) * 4;
            (bytes[off + 2], bytes[off + 1], bytes[off]) // R, G, B (skip A)
        };
        let (l_r, l_g, _l_b) = pixel_at(0, 1);
        let (r_r, r_g, _r_b) = pixel_at(3, 1);
        assert!(l_r > 200, "left half should be red, got R={l_r}");
        assert!(l_g < 50, "left half should be red, got G={l_g}");
        assert!(r_r < 50, "right half should be green, got R={r_r}");
        assert!(r_g > 200, "right half should be green, got G={r_g}");
    }
}
