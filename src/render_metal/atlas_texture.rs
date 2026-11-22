//! The Metal texture behind a glyph atlas.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTexture};

use crate::glyph_atlas::GlyphAtlas;

/// The glyph atlas's texture: the half of the atlas that is Metal's.
///
/// The atlas packs and evicts on the CPU and writes each new glyph
/// through `AtlasSink` the moment it is rasterised; this is what it
/// writes into, and what the glyph passes bind.  The renderer keeps one
/// handle and the atlas the other.
pub struct MetalAtlasTexture {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    width: u32,
    height: u32,
    bpp: u32,
}

impl MetalAtlasTexture {
    /// An R8 texture for the mono atlas, or a BGRA8 one for colour.
    pub fn new(
        device: &ProtocolObject<dyn MTLDevice>,
        width: u32,
        height: u32,
        color: bool,
    ) -> Result<std::rc::Rc<Self>, String> {
        let (format, bpp) = if color {
            (MTLPixelFormat::BGRA8Unorm, 4u32)
        } else {
            (MTLPixelFormat::R8Unorm, 1u32)
        };
        let descriptor = unsafe {
            objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                format,
                width as usize,
                height as usize,
                false,
            )
        };
        // Managed: CPU writes via replaceRegion, GPU reads.  On Apple
        // Silicon Shared would also work and skip the synchronize step,
        // but Managed is portable across Intel and Apple Silicon and the
        // difference is irrelevant for an atlas written on a miss only.
        descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
        descriptor.setUsage(objc2_metal::MTLTextureUsage::ShaderRead);
        let texture = device
            .newTextureWithDescriptor(&descriptor)
            .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())?;
        Ok(std::rc::Rc::new(Self { texture, width, height, bpp }))
    }

    pub fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }
}

impl crate::glyph_atlas::AtlasSink for MetalAtlasTexture {
    fn dims(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn bytes_per_pixel(&self) -> u32 {
        self.bpp
    }

    fn upload(&self, bytes: &[u8], w: u32, h: u32, x: u32, y: u32) {
        let region = objc2_metal::MTLRegion {
            origin: objc2_metal::MTLOrigin { x: x as usize, y: y as usize, z: 0 },
            size: objc2_metal::MTLSize { width: w as usize, height: h as usize, depth: 1 },
        };
        unsafe {
            let ptr = NonNull::new(bytes.as_ptr() as *mut c_void).unwrap();
            self.texture.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                ptr,
                (w * self.bpp) as usize,
            );
        }
    }
}

/// A glyph atlas over a new Metal texture, and the texture to bind it by.
pub fn new_atlas(
    device: &ProtocolObject<dyn MTLDevice>,
    width: u32,
    height: u32,
    color: bool,
    fonts: std::sync::Arc<crate::font_cache::CoreTextFontTable>,
) -> Result<(GlyphAtlas, std::rc::Rc<MetalAtlasTexture>), String> {
    let texture = MetalAtlasTexture::new(device, width, height, color)?;
    let atlas = GlyphAtlas::new(texture.clone(), fonts)?;
    Ok((atlas, texture))
}
