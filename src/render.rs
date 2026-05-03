//! Metal rendering pipeline.
//!
//! Phase 1.2.3: per-cell instanced quad rendering.  The atlas is no
//! longer drawn as a single test pattern — instead we walk a `Terminal`
//! grid each frame, build a list of "draw this glyph at that cell" tuples,
//! and issue a single `drawPrimitives:instanceCount:` call.  Each instance
//! emits 6 base vertices generated procedurally from `[[vertex_id]]`,
//! stretched to the destination rectangle in pixels and UV-mapped to the
//! glyph's atlas region.
//!
//! Coordinates flow:
//!   grid (col, row) → pixel rect (cell_w, cell_h) → clip space (NDC).
//! UV flow:
//!   atlas pixel rect (info.atlas_x..) → atlas UV in [0,1].
//!
//! The Terminal currently holds hardcoded seed content so this phase has
//! something visible.  Phase 1.2.4 extends instance data with foreground
//! colors; 1.2.5 wires resize → grid resize → re-render.

use crate::atlas::GlyphAtlas;
use crate::terminal::Terminal;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_foundation::{CGSize, NSString};
use objc2_metal::{
    MTLBlendFactor, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder,
    MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLLoadAction, MTLOrigin,
    MTLPixelFormat, MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions, MTLSize,
    MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use std::ffi::c_void;

const SHADER_SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Uniforms {
    float2 viewport_size;   // pixels
    float2 atlas_size;      // texels
};

struct Instance {
    float2 dest_pos;        // pixels (top-left)
    float2 dest_size;       // pixels
    float2 src_pos;         // atlas texels (top-left)
    float2 src_size;        // atlas texels
};

struct VertexOut {
    float4 clip_position [[position]];
    float2 uv;
};

constant float2 BASE_POS[6] = {
    float2(0.0, 1.0), float2(1.0, 1.0), float2(0.0, 0.0),
    float2(1.0, 1.0), float2(1.0, 0.0), float2(0.0, 0.0),
};

vertex VertexOut cell_vert(uint vid [[vertex_id]],
                           uint iid [[instance_id]],
                           constant Uniforms& uni [[buffer(0)]],
                           constant Instance* instances [[buffer(1)]]) {
    Instance inst = instances[iid];
    float2 base = BASE_POS[vid];

    // Pixel position of this vertex within the dest rect.
    float2 pixel = inst.dest_pos + base * inst.dest_size;
    // Normalize to NDC, flipping y (pixel y down → clip y up).
    float2 ndc = pixel / uni.viewport_size * 2.0 - 1.0;
    ndc.y = -ndc.y;

    // Atlas UV: same parametric base mapped through src rect, then
    // normalized by atlas size.
    float2 atlas_px = inst.src_pos + base * inst.src_size;
    float2 atlas_uv = atlas_px / uni.atlas_size;

    VertexOut out;
    out.clip_position = float4(ndc, 0.0, 1.0);
    out.uv = atlas_uv;
    return out;
}

fragment float4 cell_frag(VertexOut in            [[stage_in]],
                          texture2d<float> atlas  [[texture(0)]]) {
    // Linear sampling: at HiDPI the atlas is already at physical pixel
    // resolution; linear gives clean alpha edges where atlas texels and
    // drawable pixels nearly align.  Nearest produces hard staircase
    // edges (good for pixel art, bad for glyph antialiasing).
    constexpr sampler s(coord::normalized,
                        filter::linear,
                        address::clamp_to_edge);
    float intensity = atlas.sample(s, in.uv).r;
    return float4(intensity, intensity, intensity, intensity);
}
"#;

const ATLAS_SIZE_LOGICAL: u32 = 512;
const FONT_NAME: &str = "Menlo";
const FONT_POINT_LOGICAL: f32 = 13.0;
const GRID_COLS: u16 = 80;
const GRID_ROWS: u16 = 24;

#[repr(C)]
#[derive(Clone, Copy)]
struct Uniforms {
    viewport_w: f32,
    viewport_h: f32,
    atlas_w: f32,
    atlas_h: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Instance {
    dest_x: f32,
    dest_y: f32,
    dest_w: f32,
    dest_h: f32,
    src_x: f32,
    src_y: f32,
    src_w: f32,
    src_h: f32,
}

pub struct Renderer {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    /// Present path: when bound to an `NSView`, render() pulls drawables
    /// from this layer and presents.  In headless / snapshot mode it's
    /// `None` and rendering goes to caller-supplied textures.
    layer: Option<Retained<CAMetalLayer>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    atlas_texture: Retained<ProtocolObject<dyn MTLTexture>>,
    atlas: GlyphAtlas,
    /// Pre-allocated; reused every frame (no allocation churn on the hot
    /// path — see "no allocation on hot paths" engineering principle).
    instance_buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
    uniform_buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    /// Holds the demo content for 1.2.3 — hardcoded seed text to prove the
    /// engine→renderer wire.  In 1.2.5+ this gets driven by the PTY.
    terminal: Terminal,
    viewport_w: f32,
    viewport_h: f32,
}

impl Renderer {
    /// Build a renderer bound to an `NSView` for live display.  `scale`
    /// is the display's backing scale factor (1.0 standard, 2.0 Retina).
    pub fn new(view: &NSView, scale: f32) -> Result<Self, String> {
        Self::build(Some(view), scale)
    }

    /// Build an offscreen renderer for snapshots / tests.  No NSView, no
    /// CAMetalLayer — just GPU resources and the seeded terminal content.
    pub fn new_offscreen(scale: f32) -> Result<Self, String> {
        Self::build(None, scale)
    }

    fn build(view: Option<&NSView>, scale: f32) -> Result<Self, String> {
        let device = unsafe { Retained::from_raw(MTLCreateSystemDefaultDevice()) }
            .ok_or("no Metal device available")?;
        let queue = device
            .newCommandQueue()
            .ok_or("could not create Metal command queue")?;

        let layer = if let Some(view) = view {
            let layer = unsafe { CAMetalLayer::new() };
            unsafe {
                layer.setDevice(Some(&device));
                layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
                // contentsScale tells CA the layer's intrinsic resolution so
                // the compositor doesn't try to magnify it again on top of
                // our already-physical-pixel drawable.
                layer.setContentsScale(scale as f64);
            }
            view.setWantsLayer(true);
            unsafe { view.setLayer(Some(&**layer)) };
            Some(layer)
        } else {
            None
        };

        let raster_pt = FONT_POINT_LOGICAL * scale;
        let atlas_size = ((ATLAS_SIZE_LOGICAL as f32) * scale) as u32;
        let mut atlas = GlyphAtlas::new(FONT_NAME, raster_pt, atlas_size);
        for code in 0x20u32..0x7Fu32 {
            if let Some(ch) = char::from_u32(code) {
                atlas.ensure(ch);
            }
        }
        let cell_w = atlas.cell_width();
        let cell_h = atlas.cell_height();
        let ascent = atlas.ascent();

        // Diagnostic: print actual font metrics + a few glyph dims so we
        // can verify the rendering layout against expectations.
        if let Some(info_a) = atlas.get('A') {
            eprintln!(
                "RENDER DIAG: scale={} font={} pt={} cell_w={:.2} cell_h={:.2} ascent={:.2}",
                scale, FONT_NAME, FONT_POINT_LOGICAL * scale, cell_w, cell_h, ascent
            );
            eprintln!(
                "RENDER DIAG: 'A' atlas=({},{}) wxh={}x{} bearing=({:.2},{:.2})",
                info_a.atlas_x,
                info_a.atlas_y,
                info_a.width,
                info_a.height,
                info_a.bearing_x,
                info_a.bearing_y
            );
        }
        if let Some(info_m) = atlas.get('M') {
            eprintln!(
                "RENDER DIAG: 'M' atlas=({},{}) wxh={}x{} bearing=({:.2},{:.2})",
                info_m.atlas_x,
                info_m.atlas_y,
                info_m.width,
                info_m.height,
                info_m.bearing_x,
                info_m.bearing_y
            );
        }

        let atlas_texture = upload_atlas_texture(&device, &atlas)?;
        let pipeline = build_pipeline(&device, MTLPixelFormat::BGRA8Unorm)?;

        // Pre-allocate instance buffer for the worst case (every cell occupied).
        let max_instances = (GRID_COLS as usize) * (GRID_ROWS as usize);
        let instance_bytes = max_instances * std::mem::size_of::<Instance>();
        let instance_buffer = unsafe {
            device.newBufferWithLength_options(
                instance_bytes,
                MTLResourceOptions::MTLResourceStorageModeShared,
            )
        }
        .ok_or("could not allocate instance buffer")?;

        let uniform_buffer = unsafe {
            device.newBufferWithLength_options(
                std::mem::size_of::<Uniforms>(),
                MTLResourceOptions::MTLResourceStorageModeShared,
            )
        }
        .ok_or("could not allocate uniform buffer")?;

        // Seed the terminal with hardcoded content so the user sees real
        // engine output without yet wiring a PTY.
        let mut terminal = Terminal::new(GRID_COLS, GRID_ROWS);
        let banner = format!(
            "mars v{} ({})\r\n",
            env!("CARGO_PKG_VERSION"),
            env!("MARS_GIT_SHA")
        );
        terminal.feed(banner.as_bytes());
        terminal.feed(b"\r\n");
        terminal.feed(b"hello mars\r\n");
        terminal.feed(b"the engine is alive\r\n");
        terminal.feed(b"\r\n");
        terminal.feed(b"  pty + parser + grid + atlas + metal\r\n");
        terminal.feed(b"  88 unit tests + 4 soak tests, all green\r\n");
        terminal.feed(b"\r\n");
        terminal.feed(b"  ascii printable: !\"#$%&'()*+,-./0123456789:;<=>?@\r\n");
        terminal.feed(b"                   ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_\r\n");
        terminal.feed(b"                   `abcdefghijklmnopqrstuvwxyz{|}~\r\n");

        Ok(Self {
            device,
            queue,
            layer,
            pipeline,
            atlas_texture,
            atlas,
            instance_buffer,
            uniform_buffer,
            cell_w,
            cell_h,
            ascent,
            terminal,
            viewport_w: 0.0,
            viewport_h: 0.0,
        })
    }

    pub fn resize(&mut self, width_px: f64, height_px: f64) {
        self.viewport_w = width_px as f32;
        self.viewport_h = height_px as f32;
        if let Some(layer) = &self.layer {
            unsafe {
                layer.setDrawableSize(CGSize::new(width_px.max(1.0), height_px.max(1.0)));
            }
        }
    }

    pub fn render(&mut self) {
        let Some(layer) = &self.layer else { return };
        let Some(drawable) = (unsafe { layer.nextDrawable() }) else {
            return;
        };
        let texture = unsafe { drawable.texture() };

        // Encode the render pass and commit a SEPARATE presentation
        // buffer.  We commit twice in order: first the render commits,
        // second a tiny present-only buffer (Metal sequences them on the
        // queue automatically, so the present sees the rendered texture).
        self.encode_render_pass(&texture);
        let present_buffer = self
            .queue
            .commandBuffer()
            .expect("present buffer allocation failed");
        present_buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
        present_buffer.commit();
    }

    /// Render a single frame into an offscreen MTLTexture and return the
    /// raw BGRA8 pixel bytes.  `width` and `height` are in physical pixels.
    /// Used by `--snapshot` mode and tests; never touches the layer.
    pub fn snapshot(&mut self, width: u32, height: u32) -> Result<Vec<u8>, String> {
        let descriptor = unsafe { MTLTextureDescriptor::new() };
        unsafe {
            descriptor.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
            descriptor.setWidth(width as usize);
            descriptor.setHeight(height as usize);
            descriptor.setUsage(
                MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead,
            );
            descriptor.setStorageMode(MTLStorageMode::Shared);
        }
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or("could not create snapshot texture")?;

        self.viewport_w = width as f32;
        self.viewport_h = height as f32;

        let buffer = self.encode_render_pass(&texture);
        unsafe { buffer.waitUntilCompleted() };

        let row_bytes = width as usize * 4;
        let mut pixels = vec![0u8; row_bytes * height as usize];
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: width as usize,
                height: height as usize,
                depth: 1,
            },
        };
        unsafe {
            texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                std::ptr::NonNull::new(pixels.as_mut_ptr() as *mut c_void).unwrap(),
                row_bytes,
                region,
                0,
            );
        }
        Ok(pixels)
    }

    /// Build instance buffer + encode the render pass into `target`.
    /// Returns the (committed) command buffer so callers can wait or
    /// chain a presentDrawable on the same buffer.
    fn encode_render_pass(
        &mut self,
        target: &ProtocolObject<dyn MTLTexture>,
    ) -> Retained<ProtocolObject<dyn MTLCommandBuffer>> {
        // Refresh uniforms.
        let uniforms = Uniforms {
            viewport_w: self.viewport_w.max(1.0),
            viewport_h: self.viewport_h.max(1.0),
            atlas_w: self.atlas.width() as f32,
            atlas_h: self.atlas.height() as f32,
        };
        unsafe {
            let dst = self.uniform_buffer.contents().as_ptr() as *mut Uniforms;
            *dst = uniforms;
        }

        let instance_count = self.build_instances();

        let pass = unsafe { MTLRenderPassDescriptor::new() };
        let attachments = unsafe { pass.colorAttachments() };
        let attachment = unsafe { attachments.objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(target));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(MTLClearColor {
            red: 0.05,
            green: 0.07,
            blue: 0.12,
            alpha: 1.0,
        });

        let buffer = self
            .queue
            .commandBuffer()
            .expect("command buffer allocation failed");
        let encoder = buffer
            .renderCommandEncoderWithDescriptor(&pass)
            .expect("render command encoder failed");

        encoder.setRenderPipelineState(&self.pipeline);
        unsafe {
            encoder.setVertexBuffer_offset_atIndex(Some(&self.uniform_buffer), 0, 0);
            encoder.setVertexBuffer_offset_atIndex(Some(&self.instance_buffer), 0, 1);
            encoder.setFragmentTexture_atIndex(Some(&self.atlas_texture), 0);
            if instance_count > 0 {
                encoder.drawPrimitives_vertexStart_vertexCount_instanceCount(
                    MTLPrimitiveType::Triangle,
                    0,
                    6,
                    instance_count,
                );
            }
            encoder.endEncoding();
        }
        buffer.commit();
        buffer
    }

    /// Walk every cell in the grid; for each non-blank cell whose glyph is
    /// in the atlas, write an `Instance` into the pre-allocated buffer.
    /// Returns how many instances were written.  No allocation here — the
    /// buffer is reused frame to frame.
    fn build_instances(&mut self) -> usize {
        let grid = self.terminal.grid();
        let cols = grid.cols();
        let rows = grid.rows();

        // SAFETY: instance_buffer is host-shared and large enough for every
        // cell.  We write exactly `count` consecutive Instance structs.
        let buf_ptr = self.instance_buffer.contents().as_ptr() as *mut Instance;
        let mut count: usize = 0;

        for r in 0..rows {
            for c in 0..cols {
                let cell = grid.cell(c, r);
                if cell.ch == ' ' {
                    continue;
                }
                let Some(info) = self.atlas.get(cell.ch) else {
                    continue;
                };
                if info.width == 0 || info.height == 0 {
                    continue;
                }

                let cell_origin_x = c as f32 * self.cell_w;
                let cell_origin_y = r as f32 * self.cell_h;
                let baseline_y = cell_origin_y + self.ascent;
                let dest_x = cell_origin_x + info.bearing_x;
                let dest_y = baseline_y - info.bearing_y - info.height as f32;

                let instance = Instance {
                    dest_x,
                    dest_y,
                    dest_w: info.width as f32,
                    dest_h: info.height as f32,
                    src_x: info.atlas_x as f32,
                    src_y: info.atlas_y as f32,
                    src_w: info.width as f32,
                    src_h: info.height as f32,
                };

                unsafe {
                    *buf_ptr.add(count) = instance;
                }
                count += 1;
            }
        }
        count
    }
}

fn upload_atlas_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    atlas: &GlyphAtlas,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    let descriptor = unsafe { MTLTextureDescriptor::new() };
    unsafe {
        descriptor.setPixelFormat(MTLPixelFormat::R8Unorm);
        descriptor.setWidth(atlas.width() as usize);
        descriptor.setHeight(atlas.height() as usize);
        descriptor.setUsage(MTLTextureUsage::ShaderRead);
        descriptor.setStorageMode(MTLStorageMode::Shared);
    }

    let texture = device
        .newTextureWithDescriptor(&descriptor)
        .ok_or("could not create atlas texture")?;

    let region = MTLRegion {
        origin: MTLOrigin { x: 0, y: 0, z: 0 },
        size: MTLSize {
            width: atlas.width() as usize,
            height: atlas.height() as usize,
            depth: 1,
        },
    };
    unsafe {
        texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            region,
            0,
            std::ptr::NonNull::new(atlas.pixels().as_ptr() as *mut c_void).unwrap(),
            atlas.width() as usize,
        );
    }
    Ok(texture)
}

fn build_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    color_format: MTLPixelFormat,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let source = NSString::from_str(SHADER_SOURCE);
    let library = unsafe { device.newLibraryWithSource_options_error(&source, None) }
        .map_err(|e| format!("shader compile failed: {:?}", e))?;

    let vert_name = NSString::from_str("cell_vert");
    let frag_name = NSString::from_str("cell_frag");
    let vertex_func = library
        .newFunctionWithName(&vert_name)
        .ok_or("vertex function not found")?;
    let fragment_func = library
        .newFunctionWithName(&frag_name)
        .ok_or("fragment function not found")?;

    let descriptor = unsafe { MTLRenderPipelineDescriptor::new() };
    descriptor.setVertexFunction(Some(&vertex_func));
    descriptor.setFragmentFunction(Some(&fragment_func));

    // Color attachment: BGRA8 with standard premultiplied "src over" blend
    // so glyph alpha composites correctly against the dark clear color.
    let color_attachments = unsafe { descriptor.colorAttachments() };
    let color = unsafe { color_attachments.objectAtIndexedSubscript(0) };
    color.setPixelFormat(color_format);
    unsafe {
        color.setBlendingEnabled(true);
        color.setRgbBlendOperation(objc2_metal::MTLBlendOperation::Add);
        color.setAlphaBlendOperation(objc2_metal::MTLBlendOperation::Add);
        color.setSourceRGBBlendFactor(MTLBlendFactor::One);
        color.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        color.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        color.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    }

    // We don't need a vertex descriptor — base quad positions are
    // generated procedurally in the shader from [[vertex_id]].

    let pipeline = device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("pipeline state creation failed: {:?}", e))?;
    Ok(pipeline)
}
