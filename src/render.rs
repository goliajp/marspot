//! Metal rendering pipeline.
//!
//! Phase 1.2.2 brings the first real GPU draw call: an atlas-mapped
//! fullscreen quad showing the rasterized glyph atlas as a test pattern.
//! Per-cell instanced rendering of grid contents lands in 1.2.3.

use crate::atlas::GlyphAtlas;
use objc2::ffi::NSInteger;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_foundation::{CGSize, NSString};
use objc2_metal::{
    MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLLibrary, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLPrimitiveType, MTLRegion, MTLRenderCommandEncoder, MTLRenderPassDescriptor,
    MTLRenderPipelineDescriptor, MTLRenderPipelineState, MTLResourceOptions, MTLSize,
    MTLStorageMode, MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
    MTLVertexAttributeDescriptor, MTLVertexBufferLayoutDescriptor, MTLVertexDescriptor,
    MTLVertexFormat, MTLVertexStepFunction,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use std::ffi::c_void;

/// Metal Shading Language source for the atlas test-pattern pipeline.
/// Vertex passes a full-screen quad through clip space; fragment samples
/// the single-channel atlas and outputs a grayscale color.
const SHADER_SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexIn {
    float2 position [[attribute(0)]];
    float2 uv       [[attribute(1)]];
};

struct VertexOut {
    float4 clip_position [[position]];
    float2 uv;
};

vertex VertexOut atlas_vert(VertexIn in [[stage_in]]) {
    VertexOut out;
    out.clip_position = float4(in.position, 0.0, 1.0);
    out.uv = in.uv;
    return out;
}

fragment float4 atlas_frag(VertexOut in            [[stage_in]],
                           texture2d<float> atlas  [[texture(0)]]) {
    constexpr sampler s(coord::normalized,
                        filter::linear,
                        address::clamp_to_edge);
    float intensity = atlas.sample(s, in.uv).r;
    // Tint slightly so the pattern reads as 'rendered text' against the
    // dark background.  Pure white on dark for clarity.
    return float4(intensity, intensity, intensity, 1.0);
}
"#;

/// Two triangles forming a full-screen quad.  Each vertex is
/// `(pos.x, pos.y, uv.x, uv.y)` packed contiguously.  UVs are top-down
/// (V=0 at top), matching how the atlas stores its pixels.
#[rustfmt::skip]
const FULLSCREEN_QUAD: [f32; 24] = [
    // tri 1: bottom-left, bottom-right, top-left
    -1.0, -1.0, 0.0, 1.0,
     1.0, -1.0, 1.0, 1.0,
    -1.0,  1.0, 0.0, 0.0,
    // tri 2: bottom-right, top-right, top-left
     1.0, -1.0, 1.0, 1.0,
     1.0,  1.0, 1.0, 0.0,
    -1.0,  1.0, 0.0, 0.0,
];

const ATLAS_SIZE: u32 = 512;
const FONT_NAME: &str = "Menlo";
const FONT_POINT: f32 = 13.0;

pub struct Renderer {
    _device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    layer: Retained<CAMetalLayer>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    vertex_buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
    atlas_texture: Retained<ProtocolObject<dyn MTLTexture>>,
    /// Kept alive so subsequent phases can mutate (add glyphs) and re-upload.
    _atlas: GlyphAtlas,
}

impl Renderer {
    /// Build a renderer bound to the given `NSView`.  Caller must invoke
    /// from the main thread (AppKit requirement).
    pub fn new(view: &NSView) -> Result<Self, String> {
        let device = unsafe { Retained::from_raw(MTLCreateSystemDefaultDevice()) }
            .ok_or("no Metal device available")?;
        let queue = device
            .newCommandQueue()
            .ok_or("could not create Metal command queue")?;

        let layer = unsafe { CAMetalLayer::new() };
        unsafe {
            layer.setDevice(Some(&device));
            layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        }
        view.setWantsLayer(true);
        unsafe { view.setLayer(Some(&**layer)) };

        // Build the atlas (CPU side) and pre-rasterize printable ASCII so
        // 1.2.2's test pattern has something visible to display.
        let mut atlas = GlyphAtlas::new(FONT_NAME, FONT_POINT, ATLAS_SIZE);
        for code in 0x20u32..0x7Fu32 {
            if let Some(ch) = char::from_u32(code) {
                atlas.ensure(ch);
            }
        }

        let atlas_texture = upload_atlas_texture(&device, &atlas)?;
        let pipeline = build_pipeline(&device, MTLPixelFormat::BGRA8Unorm)?;
        let vertex_buffer = upload_vertex_buffer(&device, &FULLSCREEN_QUAD)?;

        Ok(Self {
            _device: device,
            queue,
            layer,
            pipeline,
            vertex_buffer,
            atlas_texture,
            _atlas: atlas,
        })
    }

    pub fn resize(&self, width_px: f64, height_px: f64) {
        unsafe {
            self.layer
                .setDrawableSize(CGSize::new(width_px.max(1.0), height_px.max(1.0)));
        }
    }

    pub fn render(&self) {
        let Some(drawable) = (unsafe { self.layer.nextDrawable() }) else {
            return;
        };
        let texture = unsafe { drawable.texture() };

        let pass = unsafe { MTLRenderPassDescriptor::new() };
        let attachments = unsafe { pass.colorAttachments() };
        let attachment = unsafe { attachments.objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&texture));
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
            encoder.setVertexBuffer_offset_atIndex(Some(&self.vertex_buffer), 0, 0);
            encoder.setFragmentTexture_atIndex(Some(&self.atlas_texture), 0);
            encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 6);
            encoder.endEncoding();
        }

        buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
        buffer.commit();
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
            atlas.width() as usize, // bytesPerRow for R8 = width
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

    let vert_name = NSString::from_str("atlas_vert");
    let frag_name = NSString::from_str("atlas_frag");
    let vertex_func = library
        .newFunctionWithName(&vert_name)
        .ok_or("vertex function not found")?;
    let fragment_func = library
        .newFunctionWithName(&frag_name)
        .ok_or("fragment function not found")?;

    let descriptor = unsafe { MTLRenderPipelineDescriptor::new() };
    descriptor.setVertexFunction(Some(&vertex_func));
    descriptor.setFragmentFunction(Some(&fragment_func));

    let color_attachments = unsafe { descriptor.colorAttachments() };
    let color = unsafe { color_attachments.objectAtIndexedSubscript(0) };
    color.setPixelFormat(color_format);

    // Vertex layout: each vertex = (vec2 pos, vec2 uv) tightly packed.
    let vertex_descriptor = unsafe { MTLVertexDescriptor::new() };

    unsafe {
        let attrs = vertex_descriptor.attributes();
        let attr_pos = attrs.objectAtIndexedSubscript(0);
        attr_pos.setFormat(MTLVertexFormat::Float2);
        attr_pos.setOffset(0);
        attr_pos.setBufferIndex(0);

        let attr_uv = attrs.objectAtIndexedSubscript(1);
        attr_uv.setFormat(MTLVertexFormat::Float2);
        attr_uv.setOffset(8);
        attr_uv.setBufferIndex(0);

        let layouts = vertex_descriptor.layouts();
        let layout0 = layouts.objectAtIndexedSubscript(0);
        layout0.setStride(16); // 4 floats per vertex
        layout0.setStepFunction(MTLVertexStepFunction::PerVertex);
    }

    descriptor.setVertexDescriptor(Some(&vertex_descriptor));

    let pipeline = device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("pipeline state creation failed: {:?}", e))?;
    Ok(pipeline)
}

fn upload_vertex_buffer(
    device: &ProtocolObject<dyn MTLDevice>,
    data: &[f32],
) -> Result<Retained<ProtocolObject<dyn MTLBuffer>>, String> {
    let len_bytes = std::mem::size_of_val(data);
    let buffer = unsafe {
        device.newBufferWithBytes_length_options(
            std::ptr::NonNull::new(data.as_ptr() as *mut c_void).unwrap(),
            len_bytes,
            MTLResourceOptions::MTLResourceStorageModeShared,
        )
    }
    .ok_or("could not allocate vertex buffer")?;
    Ok(buffer)
}

// Suppress an unused-import warning in case the OS doesn't expose NSInteger
// from objc2_metal in some config — keeping the import as documentation of
// where MTLOrigin/MTLSize fields ultimately come from.
#[allow(dead_code)]
fn _force_use_nsinteger(_: NSInteger) {}
