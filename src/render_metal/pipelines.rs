//! The shader library, the five pipelines made from it and the glyph
//! sampler: what the renderer builds once, at launch.
//!
//! Every pipeline blends alpha as source over destination, so a target
//! cleared opaque stays opaque whatever is drawn: the window server
//! composites the surface as it is, and an alpha under one there would
//! let the layer below show through.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::NSString;
use objc2_metal::{
    MTLBlendFactor, MTLBlendOperation, MTLDevice, MTLLibrary, MTLRenderPipelineDescriptor,
    MTLRenderPipelineState, MTLSamplerAddressMode, MTLSamplerDescriptor, MTLSamplerMinMagFilter,
    MTLSamplerState,
};

use super::TARGET_FORMAT;

const SHADER_SRC: &str = include_str!("../shaders/cells.metal");

type Pipeline = Retained<ProtocolObject<dyn MTLRenderPipelineState>>;

type Pipelines = (Pipeline, Pipeline, Pipeline, Pipeline, Pipeline);

/// The shader library and the five pipelines made from it, and the
/// microseconds each half took.
fn build_timed(device: &ProtocolObject<dyn MTLDevice>) -> Result<(Pipelines, u64, u64), String> {
    let t0 = std::time::Instant::now();
    let library = build_shader_library(device)?;
    let t1 = std::time::Instant::now();
    let all = (
        build_bg_pipeline(device, &library)?,
        build_fg_pipeline(device, &library)?,
        build_fg_color_pipeline(device, &library)?,
        build_dot_pipeline(device, &library)?,
        build_scene_ui_pipeline(device, &library)?,
    );
    Ok((all, (t1 - t0).as_micros() as u64, t1.elapsed().as_micros() as u64))
}

/// [`build_timed`], with what each half cost logged once: this runs at
/// every launch.
pub(super) fn build_pipelines(device: &ProtocolObject<dyn MTLDevice>) -> Result<Pipelines, String> {
    let (all, library_us, pipelines_us) = build_timed(device)?;
    crate::lx_event!(
        "RENDERER_SHADERS",
        "shader library and pipelines built",
        library_us = library_us,
        pipelines_us = pipelines_us
    );
    Ok(all)
}

/// Build the library and the pipelines exactly as a start does, and
/// drop them: the system keeps what it compiled, per binary, so the
/// next process of this binary finds it.  Returns the microseconds the
/// library and the pipelines took.  Touches nothing else: no log, no
/// state directory.
pub fn warm_shaders() -> Result<(u64, u64), String> {
    let device = super::system_default_device()?;
    let (_, library_us, pipelines_us) = build_timed(&device)?;
    Ok((library_us, pipelines_us))
}

/// Compile `cells.metal` once.  Both the BG and FG pipelines pull
/// their entry-point functions out of the same library — the .metal
/// file declares them side-by-side.
fn build_shader_library(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLLibrary>>, String> {
    let src = NSString::from_str(SHADER_SRC);
    device
        .newLibraryWithSource_options_error(&src, None)
        .map_err(|e| format!("MTLDevice.newLibraryWithSource error: {:?}", e))
}

fn pipeline_function(
    library: &ProtocolObject<dyn MTLLibrary>,
    name: &str,
) -> Result<Retained<ProtocolObject<dyn objc2_metal::MTLFunction>>, String> {
    let n = NSString::from_str(name);
    library
        .newFunctionWithName(&n)
        .ok_or_else(|| format!("library has no function {name:?}"))
}

/// BG-pass pipeline.  No blending — opaque cells fill the whole quad.
fn build_bg_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "bg_vertex")?;
    let ffn = pipeline_function(library, "bg_fragment")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    // Alpha blending so translucent BG cells (e.g. the
    // inactive-pane dim overlay pushed at the end of
    // push_session) composite over the colour BG fills below
    // them.  Opaque cells (alpha = 1.0) render identically
    // either way — `src.a = 1` makes destination contribution
    // zero, same as no blending.
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (BG) error: {:?}", e))
}

/// FG-pass pipeline.  Alpha blending on so the glyph's coverage
/// composites onto whatever the BG pass painted underneath:
///
/// ```text
/// final.rgb = src.rgb * src.a + dst.rgb * (1 - src.a)
/// final.a   = src.a   * src.a + dst.a   * (1 - src.a)
/// ```
fn build_fg_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "fg_vertex")?;
    let ffn = pipeline_function(library, "fg_fragment")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (FG) error: {:?}", e))
}

/// Colour-glyph FG pipeline.  Same `fg_vertex` as the mono path, but the
/// `fg_fragment_color` fragment samples the BGRA colour atlas and outputs
/// the texel directly (the emoji's own colours).  The texel is
/// **premultiplied** (the rasteriser drew into a premultiplied-alpha
/// context), so the blend uses source factor `One` rather than
/// `SourceAlpha`:
///
/// ```text
/// final.rgb = src.rgb * 1 + dst.rgb * (1 - src.a)
/// ```
fn build_fg_color_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "fg_vertex")?;
    let ffn = pipeline_function(library, "fg_fragment_color")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (FG colour) error: {:?}", e))
}

/// Dot-pass pipeline.  Same `CellInstance` input as BG, but the
/// fragment shader smoothsteps to a circle inscribed in the quad.
/// Alpha-blended (same equation as FG) so the AA edge composites
/// cleanly over the underlying sidebar BG.
fn build_dot_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, "dot_vertex")?;
    let ffn = pipeline_function(library, "dot_fragment")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::SourceAlpha);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (dot) error: {:?}", e))
}


fn build_scene_ui_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    build_ui_pipeline_with(device, library, "scene_ui_rect_vertex")
}


fn build_ui_pipeline_with(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    vertex_fn: &str,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let vfn = pipeline_function(library, vertex_fn)?;
    let ffn = pipeline_function(library, "ui_rect_fragment")?;

    let descriptor = MTLRenderPipelineDescriptor::new();
    descriptor.setVertexFunction(Some(&vfn));
    descriptor.setFragmentFunction(Some(&ffn));
    let attachment = unsafe { descriptor.colorAttachments().objectAtIndexedSubscript(0) };
    attachment.setPixelFormat(TARGET_FORMAT);
    attachment.setBlendingEnabled(true);
    attachment.setRgbBlendOperation(MTLBlendOperation::Add);
    attachment.setAlphaBlendOperation(MTLBlendOperation::Add);
    // `One`, not `SourceAlpha`: `ui_rect_fragment` composites its
    // shadow/fill/border layers into PREMULTIPLIED form (it returns
    // `rgb * a`), so the source contribution is already scaled and the
    // blend must not scale it again.
    //
    // It used to be `SourceAlpha`, which applied alpha twice.  At
    // a = 1.0 the two formulas agree, so every opaque surface looked
    // correct and the bug hid; a 0.2-alpha fill rendered at an
    // effective 0.04 and simply disappeared.  Measured before the fix:
    // white at a = 0.5 over black read 64 instead of 128.  This is
    // where "semi-transparent panels don't work, make them opaque"
    // came from — it was never a taste constraint, it was this.
    attachment.setSourceRGBBlendFactor(MTLBlendFactor::One);
    attachment.setSourceAlphaBlendFactor(MTLBlendFactor::One);
    attachment.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    attachment.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);

    device
        .newRenderPipelineStateWithDescriptor_error(&descriptor)
        .map_err(|e| format!("newRenderPipelineState (ui_rect) error: {:?}", e))
}

pub(super) fn build_fg_sampler(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLSamplerState>>, String> {
    let descriptor = MTLSamplerDescriptor::new();
    descriptor.setMinFilter(MTLSamplerMinMagFilter::Linear);
    descriptor.setMagFilter(MTLSamplerMinMagFilter::Linear);
    descriptor.setSAddressMode(MTLSamplerAddressMode::ClampToEdge);
    descriptor.setTAddressMode(MTLSamplerAddressMode::ClampToEdge);
    device
        .newSamplerStateWithDescriptor(&descriptor)
        .ok_or_else(|| "newSamplerStateWithDescriptor returned nil".to_string())
}
