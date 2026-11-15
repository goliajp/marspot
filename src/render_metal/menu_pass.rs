//! The context menu's pass: drawn over the finished frame, from the
//! instances its surface keeps.
//!
//! The frame's own overlay layers draw every rounded rectangle before
//! any glyph, so a menu added to them would have the text of whatever
//! it covers showing through it.  It gets a pass of its own instead,
//! after everything else, loading what is there.
//!
//! The two instance buffers live with the surface and are made again
//! only when the surface paints a new picture: a frame of a menu that
//! has not changed uploads nothing.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBuffer, MTLCommandBuffer, MTLCommandEncoder, MTLDevice, MTLLoadAction, MTLPrimitiveType,
    MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLRenderPipelineState, MTLSamplerState, MTLStoreAction,
    MTLTexture,
};

use super::{glyphs_as_bytes, make_instance_buffer, ui_rects_as_bytes};
use crate::ui::components::context_menu_view::MenuSurface;
use golia_ui_core::scene::{GlyphInstance, UiRectInstance};

type Buffer = Retained<ProtocolObject<dyn MTLBuffer>>;

/// The menu's instances on the GPU, and which picture of the surface
/// they are.
#[derive(Default)]
pub struct MenuBuffers {
    /// One more than the surface's `painted_rev` they were made from;
    /// zero before any were.
    made_from: u64,
    ui_rects: Option<Buffer>,
    glyphs: Option<Buffer>,
    counts: (usize, usize),
    slab: Vec<u8>,
}

/// Draw the menu `surface` holds onto `target`, over what is there.
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_menu(
    surface: &mut MenuSurface,
    target: &ProtocolObject<dyn MTLTexture>,
    cmd: &ProtocolObject<dyn MTLCommandBuffer>,
    ui_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    glyph_pipeline: &ProtocolObject<dyn MTLRenderPipelineState>,
    sampler: &ProtocolObject<dyn MTLSamplerState>,
    atlas_texture: &ProtocolObject<dyn MTLTexture>,
    device: &ProtocolObject<dyn MTLDevice>,
    viewport_px: &[f32; 2],
) {
    let rev = surface.painted_rev() + 1;
    if surface.buffers.made_from != rev {
        let mut b = std::mem::take(&mut surface.buffers);
        let (cells, glyphs, ui_rects): (_, &[GlyphInstance], &[UiRectInstance]) = surface.kept.kept();
        debug_assert!(cells.is_empty(), "a menu draws rounded rectangles and glyphs only");
        b.ui_rects = make_instance_buffer(device, ui_rects_as_bytes(ui_rects, &mut b.slab));
        b.glyphs = make_instance_buffer(device, glyphs_as_bytes(glyphs, &mut b.slab));
        b.counts = (ui_rects.len(), glyphs.len());
        b.made_from = rev;
        surface.buffers = b;
    }
    let b = &surface.buffers;
    if b.ui_rects.is_none() && b.glyphs.is_none() {
        return;
    }
    let pass = MTLRenderPassDescriptor::new();
    // SAFETY: attachment 0 always exists on a fresh descriptor
    unsafe {
        let color = pass.colorAttachments().objectAtIndexedSubscript(0);
        color.setTexture(Some(target));
        color.setLoadAction(MTLLoadAction::Load);
        color.setStoreAction(MTLStoreAction::Store);
    }
    let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&pass) else { return };
    let viewport = NonNull::new(viewport_px.as_ptr() as *mut c_void).unwrap();
    // SAFETY: `viewport` points at two f32s that outlive the call, and
    // Metal copies them; the buffers hold `counts` instances of the
    // layout each pipeline reads
    unsafe {
        enc.setVertexBytes_length_atIndex(viewport, std::mem::size_of::<[f32; 2]>(), 1);
        if let Some(buf) = &b.ui_rects {
            enc.setRenderPipelineState(ui_pipeline);
            enc.setVertexBuffer_offset_atIndex(Some(buf), 0, 0);
            enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::Triangle, 0, 6, b.counts.0);
        }
        if let Some(buf) = &b.glyphs {
            enc.setRenderPipelineState(glyph_pipeline);
            enc.setVertexBuffer_offset_atIndex(Some(buf), 0, 0);
            enc.setFragmentTexture_atIndex(Some(atlas_texture), 0);
            enc.setFragmentSamplerState_atIndex(Some(sampler), 0);
            enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::Triangle, 0, 6, b.counts.1);
        }
    }
    enc.endEncoding();
}
