//! Pieces of the frame's one pass: the scissor a layer clips to, and
//! the web pane pictures drawn over their panes' backgrounds.

use std::ffi::c_void;
use std::ptr::NonNull;

use golia_ui_core::color::Rgba8;
use golia_ui_core::scene::{Encode, GlyphInstance};
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLPrimitiveType, MTLRenderCommandEncoder, MTLScissorRect};

use super::encode::Drawing;
use crate::web_pane::gpu::Texture;

/// One web pane's picture and where it goes, device pixels.
pub struct WebImage {
    pub texture: Texture,
    /// x, y, width, height at whole pixels, the picture's own size or
    /// less, so each texel lands on one pixel
    pub rect: [f32; 4],
    /// The part of the texture drawn, from its top left, 0..1
    pub uv1: [f32; 2],
}

/// A layer's clip as Metal's scissor, held inside the target: Metal
/// rejects a scissor that reaches past the attachment.
pub(super) fn scissor_for(clip: golia_ui_core::units::RectPx, tw: usize, th: usize) -> MTLScissorRect {
    let x0 = (clip.x.max(0.0).floor() as usize).min(tw);
    let y0 = (clip.y.max(0.0).floor() as usize).min(th);
    let x1 = ((clip.x + clip.w).max(0.0).ceil() as usize).min(tw);
    let y1 = ((clip.y + clip.h).max(0.0).ceil() as usize).min(th);
    MTLScissorRect { x: x0, y: y0, width: x1.saturating_sub(x0), height: y1.saturating_sub(y0) }
}

/// Draw each web pane's picture as one quad with the colour-glyph
/// pipeline: it samples a premultiplied BGRA texture and writes the
/// texel as it is, which is what the pages' pixels are.
pub(super) fn draw_images(enc: &ProtocolObject<dyn MTLRenderCommandEncoder>, d: &Drawing) {
    if d.images.is_empty() {
        return;
    }
    enc.setRenderPipelineState(d.fg_color_pipeline);
    for im in d.images {
        let [x, y, w, h] = im.rect;
        let quad = GlyphInstance {
            origin: [x, y],
            size: [w, h],
            uv0: [0.0, 0.0],
            uv1: im.uv1,
            color: Rgba8 { r: 255, g: 255, b: 255, a: 255 },
        };
        let mut bytes = [0u8; GlyphInstance::SIZE];
        quad.encode(&mut bytes);
        let ptr = NonNull::new(bytes.as_mut_ptr() as *mut c_void).expect("a stack array");
        // SAFETY: Metal copies the instance's bytes before the call
        // returns; the texture and sampler outlive the encoder
        unsafe {
            enc.setVertexBytes_length_atIndex(ptr, bytes.len(), 0);
            enc.setFragmentTexture_atIndex(Some(im.texture.texture()), 0);
            enc.setFragmentSamplerState_atIndex(Some(d.fg_sampler), 0);
            enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::Triangle, 0, 6, 1);
        }
    }
}
