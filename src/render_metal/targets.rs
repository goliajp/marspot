//! Render targets: allocating them, and reading their pixels back.

use std::ffi::c_void;
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLTexture};

use super::{MetalRenderer, TARGET_FORMAT};
use crate::frame_build::glyph_resolve::resolve_cell_glyph;
use crate::glyph_atlas::SlotMetrics;

/// Allocate a render-target MTLTexture.  Helper for tests + the
/// `--bench metal-render` harness.  StorageModePrivate (GPU-only)
/// because we never read the bytes back in the bench path; for
/// readback (existing offscreen render_cells_bg_offscreen / fg)
/// the caller still allocates Managed + blit-synchronizes.
/// A render target whose bytes can be read back on the CPU.
///
/// [`make_target_texture`] asks for `Private` storage — right for a
/// texture only the GPU ever looks at, and unreadable by `getBytes`.
/// Anything that wants the pixels afterwards (the offscreen `--shot`
/// path, the snapshot tests) needs `Managed`, plus a blit
/// `synchronizeResource` before the read.
pub fn make_readback_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    width: u32,
    height: u32,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    let descriptor = unsafe {
        objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            TARGET_FORMAT,
            width as usize,
            height as usize,
            false,
        )
    };
    descriptor.setUsage(
        objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
    );
    descriptor.setStorageMode(objc2_metal::MTLStorageMode::Managed);
    device
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())
}

/// Copy a Managed texture's pixels out, BGRA, top-left origin.
///
/// The caller must have synchronised the resource already — every
/// call site here does it inside the command buffer that drew, which
/// is the only place it can be done without a second submit.
pub fn texture_bytes_bgra(texture: &ProtocolObject<dyn MTLTexture>) -> Vec<u8> {
    let width = texture.width();
    let height = texture.height();
    let bytes_per_row = width * 4;
    let mut bytes = vec![0u8; bytes_per_row * height];
    let region = objc2_metal::MTLRegion {
        origin: objc2_metal::MTLOrigin { x: 0, y: 0, z: 0 },
        size: objc2_metal::MTLSize { width, height, depth: 1 },
    };
    unsafe {
        texture.getBytes_bytesPerRow_fromRegion_mipmapLevel(
            NonNull::new(bytes.as_mut_ptr() as *mut c_void).unwrap(),
            bytes_per_row,
            region,
            0,
        );
    }
    bytes
}

pub fn make_target_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    width: u32,
    height: u32,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    let descriptor = unsafe {
        objc2_metal::MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            TARGET_FORMAT,
            width as usize,
            height as usize,
            false,
        )
    };
    descriptor.setUsage(
        objc2_metal::MTLTextureUsage::RenderTarget | objc2_metal::MTLTextureUsage::ShaderRead,
    );
    descriptor.setStorageMode(objc2_metal::MTLStorageMode::Private);
    device
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| "newTextureWithDescriptor returned nil".to_string())
}

impl MetalRenderer {
    /// Expose the device so the bench harness can allocate a
    /// render-target texture without a public `device` field.
    pub fn device(&self) -> &ProtocolObject<dyn MTLDevice> {
        &self.device
    }

    /// Flush a Managed render target's GPU writes to the CPU side and
    /// hand back its pixels, BGRA.
    ///
    /// A second command buffer, because `render_layout_to_texture` has
    /// already committed its own — the cost of one extra submit buys
    /// callers that do not have to thread a blit through the render
    /// path they are borrowing.
    pub fn read_target(
        &self,
        texture: &ProtocolObject<dyn MTLTexture>,
    ) -> Result<Vec<u8>, String> {
        let cmd = self
            .queue
            .commandBuffer()
            .ok_or_else(|| "commandBuffer returned nil".to_string())?;
        let blit = cmd
            .blitCommandEncoder()
            .ok_or_else(|| "blitCommandEncoder returned nil".to_string())?;
        let resource: &ProtocolObject<dyn objc2_metal::MTLResource> =
            ProtocolObject::from_ref(texture);
        blit.synchronizeResource(resource);
        blit.endEncoding();
        cmd.commit();
        cmd.waitUntilCompleted();
        Ok(texture_bytes_bgra(texture))
    }

    /// Bench helper (perf-attack B3/B4): rasterise each char into the
    /// atlas via the real cache-miss path (`resolve_char` +
    /// `get_or_rasterize`), returning total nanoseconds.  Distinct chars
    /// force misses, so this isolates exactly the per-glyph cost a
    /// CJK/emoji firehose pays on first sight of each glyph — the
    /// `get_bounding_rects` CT call, the per-glyph `Vec` alloc +
    /// `CGBitmapContextCreate` + ~8 `set_*` context-property calls, and
    /// `draw_glyphs` — with no GPU draw and no parse in the number.  Call
    /// on a fresh renderer for a cold atlas.  Returns 0 if `chars` is
    /// empty.  Keep the distinct-char count under the atlas capacity
    /// (~20k cell-sized slots) to avoid a rebuild skewing the average.
    pub fn bench_rasterize(&mut self, chars: &[char]) -> u64 {
        let metrics = SlotMetrics {
            cell_w: self.font.cell_w.round() as u32,
            cell_h: self.font.cell_h.round() as u32,
            baseline_from_top: self.font.ascent.round() as u32,
        };
        let t0 = std::time::Instant::now();
        for &ch in chars {
            let _ = resolve_cell_glyph(&mut self.atlas, &mut self.font, ch, false, false, metrics);
        }
        t0.elapsed().as_nanos() as u64
    }
}
