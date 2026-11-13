//! A picture for a test: rectangles and runs of text in the order
//! they are to be drawn, each kind through its own pipeline, later
//! over earlier whatever the kind.
//!
//! The frame draws in layers by kind; a font snapshot wants a
//! background, then text, then a rule over the text.  This draws one
//! pass per run of a kind, loading what the last one left.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLLoadAction,
    MTLPrimitiveType, MTLRenderCommandEncoder, MTLRenderPassDescriptor, MTLStoreAction, MTLTexture,
};

use super::{make_instance_buffer, MetalRenderer, OFFSCREEN_CLEAR, TARGET_FORMAT};
use crate::font_shape::ShapeOptions;
use crate::frame_build::color::rgba8_of_f32;
use crate::frame_build::text_run::{push_text_run_kind, push_text_run_ui_shaped, FontKind};
use crate::ui::core::Color;
use golia_ui_core::scene::{Encode, GlyphInstance, UiRectInstance};

struct Text {
    x: f64,
    y: f64,
    content: String,
    color: Color,
    weight: u16,
    opts: ShapeOptions,
    /// `None` takes the face the sheet is rendered in.
    ui: Option<bool>,
    ui_size_q: Option<u16>,
}

enum Item {
    Rect(UiRectInstance),
    Text(Text),
}

/// Lengths are in units of `scale` device pixels.
pub(crate) struct Sheet {
    scale: f64,
    items: Vec<Item>,
}

fn rgba8(c: Color) -> golia_ui_core::Rgba8 {
    rgba8_of_f32(c.to_rgba_f32())
}

impl Sheet {
    pub(crate) fn new(scale: f64) -> Self {
        Self { scale, items: Vec::new() }
    }

    /// A rectangle, of no size and no colour until it is given them.
    pub(crate) fn rect(&mut self) -> RectBuilder<'_> {
        RectBuilder {
            sheet: self,
            at: (0.0, 0.0),
            size: (0.0, 0.0),
            fill: Color::TRANSPARENT,
            radius: 0.0,
            border: None,
            shadow: None,
        }
    }

    /// A run of text with the top of its line at `y`.
    pub(crate) fn text(&mut self, x: f64, y: f64, content: &str) -> TextBuilder<'_> {
        let k = self.scale;
        let text = Text {
            x: x * k,
            y: y * k,
            content: content.to_string(),
            color: Color::TRANSPARENT,
            weight: 400,
            opts: ShapeOptions::full(),
            ui: None,
            ui_size_q: None,
        };
        TextBuilder { sheet: self, text }
    }
}

pub(crate) struct RectBuilder<'s> {
    sheet: &'s mut Sheet,
    at: (f64, f64),
    size: (f64, f64),
    fill: Color,
    radius: f64,
    border: Option<(f64, Color)>,
    shadow: Option<(f64, Color)>,
}

impl RectBuilder<'_> {
    pub(crate) fn at(mut self, x: f64, y: f64) -> Self {
        self.at = (x, y);
        self
    }
    pub(crate) fn size(mut self, w: f64, h: f64) -> Self {
        self.size = (w, h);
        self
    }
    pub(crate) fn fill(mut self, color: Color) -> Self {
        self.fill = color;
        self
    }
    pub(crate) fn radius(mut self, r: f64) -> Self {
        self.radius = r;
        self
    }
    pub(crate) fn border(mut self, width: f64, color: Color) -> Self {
        self.border = Some((width, color));
        self
    }
    pub(crate) fn shadow(mut self, blur: f64, color: Color) -> Self {
        self.shadow = Some((blur, color));
        self
    }
    pub(crate) fn draw(self) {
        let k = self.sheet.scale;
        let none = rgba8_of_f32([0.0; 4]);
        let (border_width, border) = self.border.map_or((0.0, none), |(w, c)| ((w * k) as f32, rgba8(c)));
        let (shadow_blur, shadow_color) = self.shadow.map_or((0.0, none), |(blur, c)| ((blur * k) as f32, rgba8(c)));
        self.sheet.items.push(Item::Rect(UiRectInstance {
            origin: [(self.at.0 * k) as f32, (self.at.1 * k) as f32],
            size: [(self.size.0 * k) as f32, (self.size.1 * k) as f32],
            fill: rgba8(self.fill),
            border,
            radius: (self.radius * k) as f32,
            border_width,
            shadow_offset: [0.0, 0.0],
            shadow_color,
            shadow_blur,
        }));
    }
}

pub(crate) struct TextBuilder<'s> {
    sheet: &'s mut Sheet,
    text: Text,
}

impl TextBuilder<'_> {
    pub(crate) fn color(mut self, c: Color) -> Self {
        self.text.color = c;
        self
    }
    pub(crate) fn weight(mut self, weight: u16) -> Self {
        self.text.weight = weight;
        self
    }
    pub(crate) fn opts(mut self, opts: ShapeOptions) -> Self {
        self.text.opts = opts;
        self
    }
    pub(crate) fn ui(mut self) -> Self {
        self.text.ui = Some(true);
        self
    }
    pub(crate) fn ui_size_q(mut self, size_q: u16) -> Self {
        self.text.ui_size_q = Some(size_q);
        self
    }
    pub(crate) fn draw(self) {
        self.sheet.items.push(Item::Text(self.text));
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RunKind {
    UiRect,
    Glyph,
    ColorGlyph,
}

/// The sheet as instances, and the runs of one kind they fall into.
pub(crate) struct Runs {
    pub(crate) ui: Vec<UiRectInstance>,
    pub(crate) glyphs: Vec<GlyphInstance>,
    pub(crate) color_glyphs: Vec<GlyphInstance>,
    pub(crate) runs: Vec<(RunKind, usize)>,
}

impl MetalRenderer {
    /// Turn `sheet` into instances.  `ui_font` is the face of text
    /// that names none.
    pub(crate) fn sheet_runs(&mut self, sheet: &Sheet, cell_w: f32, cell_h: f32, ascent: f32, ui_font: bool) -> Runs {
        let (aw, ah) = self.atlas.dims();
        let (caw, cah) = self.color_atlas.dims();
        let mut out = Runs { ui: Vec::new(), glyphs: Vec::new(), color_glyphs: Vec::new(), runs: Vec::new() };
        let bump = |runs: &mut Vec<(RunKind, usize)>, k: RunKind, n: usize| match runs.last_mut() {
            Some((kind, count)) if *kind == k => *count += n,
            _ => runs.push((k, n)),
        };
        for item in &sheet.items {
            match item {
                Item::Rect(r) => {
                    out.ui.push(*r);
                    bump(&mut out.runs, RunKind::UiRect, 1);
                }
                Item::Text(t) => {
                    let (mono0, color0) = (out.glyphs.len(), out.color_glyphs.len());
                    if t.ui.unwrap_or(ui_font) {
                        push_text_run_ui_shaped(
                            &t.content, t.x as f32, t.y as f32, t.color.to_rgba_f32(), ascent,
                            aw as f32, ah as f32, caw as f32, cah as f32,
                            t.weight, t.opts, t.ui_size_q,
                            &mut self.font, &mut self.atlas, &mut self.color_atlas,
                            &mut out.glyphs, &mut out.color_glyphs,
                        );
                    } else {
                        push_text_run_kind(
                            &t.content, t.x as f32, t.y as f32 + ascent, t.color.to_rgba_f32(),
                            cell_w, cell_h, ascent, aw as f32, ah as f32,
                            &mut self.font, &mut self.atlas, &mut out.glyphs, FontKind::Terminal,
                        );
                    }
                    // one run of each kind per text: its mono glyphs,
                    // then its colour ones
                    if out.glyphs.len() > mono0 {
                        bump(&mut out.runs, RunKind::Glyph, out.glyphs.len() - mono0);
                    }
                    if out.color_glyphs.len() > color0 {
                        bump(&mut out.runs, RunKind::ColorGlyph, out.color_glyphs.len() - color0);
                    }
                }
            }
        }
        out
    }

    /// Draw `sheet` on black and hand back the BGRA pixels.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_sheet_to_bitmap(
        &mut self,
        width: u32,
        height: u32,
        sheet: &Sheet,
        cell_w: f32,
        cell_h: f32,
        ascent: f32,
        ui_font: bool,
    ) -> Result<Vec<u8>, String> {
        let descriptor = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                TARGET_FORMAT, width as usize, height as usize, false,
            )
        };
        descriptor.setUsage(objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead);
        descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        let texture = self
            .device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;
        let cmd = self.queue.commandBuffer().ok_or_else(|| "commandBuffer returned nil".to_string())?;
        let viewport_px: [f32; 2] = [width as f32, height as f32];
        let viewport = NonNull::new(viewport_px.as_ptr() as *mut c_void).unwrap();

        let r = self.sheet_runs(sheet, cell_w, cell_h, ascent, ui_font);
        let bytes_of = |ui: &[UiRectInstance], glyphs: &[GlyphInstance]| {
            let mut slab = vec![0u8; ui.len() * UiRectInstance::SIZE + glyphs.len() * GlyphInstance::SIZE];
            for (v, slot) in ui.iter().zip(slab.chunks_exact_mut(UiRectInstance::SIZE)) {
                v.encode(slot);
            }
            for (v, slot) in glyphs.iter().zip(slab.chunks_exact_mut(GlyphInstance::SIZE)) {
                v.encode(slot);
            }
            slab
        };
        let (mut ui_at, mut glyph_at, mut color_at) = (0usize, 0usize, 0usize);
        // a sheet with nothing on it is still cleared
        let passes = if r.runs.is_empty() { vec![None] } else { r.runs.iter().copied().map(Some).collect() };
        for (n, run) in passes.into_iter().enumerate() {
            let pass = MTLRenderPassDescriptor::new();
            unsafe {
                let color = pass.colorAttachments().objectAtIndexedSubscript(0);
                color.setTexture(Some(&*texture));
                if n == 0 {
                    color.setLoadAction(MTLLoadAction::Clear);
                    color.setClearColor(OFFSCREEN_CLEAR);
                } else {
                    color.setLoadAction(MTLLoadAction::Load);
                }
                color.setStoreAction(MTLStoreAction::Store);
            }
            let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&pass) else { continue };
            let Some((kind, count)) = run else {
                enc.endEncoding();
                continue;
            };
            let (pipeline, bytes, atlas) = match kind {
                RunKind::UiRect => {
                    let b = bytes_of(&r.ui[ui_at..ui_at + count], &[]);
                    ui_at += count;
                    (&self.scene_ui_pipeline, b, None)
                }
                RunKind::Glyph => {
                    let b = bytes_of(&[], &r.glyphs[glyph_at..glyph_at + count]);
                    glyph_at += count;
                    (&self.fg_pipeline, b, Some(self.atlas_texture.texture()))
                }
                RunKind::ColorGlyph => {
                    let b = bytes_of(&[], &r.color_glyphs[color_at..color_at + count]);
                    color_at += count;
                    (&self.fg_color_pipeline, b, Some(self.color_atlas_texture.texture()))
                }
            };
            let buf = make_instance_buffer(&self.device, &bytes);
            unsafe {
                enc.setVertexBytes_length_atIndex(viewport, std::mem::size_of::<[f32; 2]>(), 1);
                enc.setRenderPipelineState(pipeline);
                if let Some(b) = &buf {
                    enc.setVertexBuffer_offset_atIndex(Some(b), 0, 0);
                }
                if let Some(tex) = atlas {
                    enc.setFragmentTexture_atIndex(Some(tex), 0);
                    enc.setFragmentSamplerState_atIndex(Some(&*self.fg_sampler), 0);
                }
                enc.drawPrimitives_vertexStart_vertexCount_instanceCount(MTLPrimitiveType::Triangle, 0, 6, count);
            }
            enc.endEncoding();
        }

        let blit = cmd.blitCommandEncoder().ok_or_else(|| "blitCommandEncoder returned nil".to_string())?;
        let resource: &ProtocolObject<dyn objc2_metal::MTLResource> = ProtocolObject::from_ref(&*texture);
        blit.synchronizeResource(resource);
        blit.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();

        let bytes_per_row = (width as usize) * 4;
        let mut bytes = vec![0u8; bytes_per_row * height as usize];
        let region = objc2_metal::MTLRegion {
            origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
            size: objc2_metal::MTLSize { width: width as usize, height: height as usize, depth: 1 },
        };
        unsafe {
            texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                NonNull::new(bytes.as_mut_ptr() as *mut c_void).unwrap(),
                bytes_per_row, region, 0,
            );
        }
        Ok(bytes)
    }
}
