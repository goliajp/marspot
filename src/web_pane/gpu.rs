//! A web pane's shared-memory buffers and the textures over them.
//!
//! The buffer's memory is the texture's: the content process writes
//! pixels into the mapping and the GPU samples them where they are, no
//! copy in between.  The mapping goes to Metal with the buffer and is
//! unmapped only when Metal lets go of it, so a frame still in flight
//! when the pane closes reads memory that is still there.

use std::ffi::c_void;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use madcawl_wire::display::Buffer;
use madcawl_wire::os::{self, Mapping};
use madcawl_wire::shm::{Header, GENERATION_AT, HEADER_LEN};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{
    MTLBuffer, MTLDevice, MTLPixelFormat, MTLResourceOptions, MTLStorageMode, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage,
};

pub type Device = Retained<ProtocolObject<dyn MTLDevice>>;

/// A texture over one buffer, and the buffer it shares memory with.
#[derive(Clone)]
pub struct Texture {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    buffer: Retained<ProtocolObject<dyn MTLBuffer>>,
}

// SAFETY: Metal resources may be used from any thread; who writes the
// memory when is the display channel's protocol
unsafe impl Send for Texture {}
// SAFETY: as above; only immutable Metal calls are made through it
unsafe impl Sync for Texture {}

impl Texture {
    pub fn texture(&self) -> &ProtocolObject<dyn MTLTexture> {
        &self.texture
    }

    /// The commit sequence the content process last marked written.
    pub fn generation(&self) -> u64 {
        let base = self.buffer.contents().as_ptr() as *const u8;
        // SAFETY: the buffer is the whole mapping, page-aligned and at
        // least a header long, so the generation is an aligned u64 in it
        let cell = unsafe { &*base.add(GENERATION_AT).cast::<AtomicU64>() };
        u64::from_le(cell.load(Ordering::Acquire))
    }
}

/// The OS page size, which a no-copy GPU buffer's length is a multiple of.
pub fn page_size() -> u64 {
    // SAFETY: no arguments
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as u64 }
}

/// A new buffer of `w` by `h` pixels: its descriptor to send, its
/// mapping with the header written, and how it is described.
pub fn make_buffer(w: u32, h: u32) -> io::Result<(OwnedFd, Mapping, Buffer)> {
    let bytes = super::frames::buffer_bytes(w, h, page_size());
    let fd = os::shm_create(bytes)?;
    let mut m = Mapping::map(fd.as_fd(), bytes)?;
    let head = Header::new(w, h, bytes);
    head.write(m.bytes_mut());
    Ok((fd, m, Buffer { bytes, width: w, height: h, stride: head.stride }))
}

/// The texture over `m`, which `b` describes; the mapping is Metal's
/// from here on.
pub fn wrap(device: &ProtocolObject<dyn MTLDevice>, m: Mapping, b: &Buffer) -> Option<Texture> {
    let ptr = NonNull::new(m.as_ptr() as *mut c_void)?;
    let len = m.len();
    let hold = std::cell::Cell::new(Some(m));
    // unmapped when Metal releases the block, after the buffer is gone
    let free = block2::RcBlock::new(move |_: NonNull<c_void>, _: usize| drop(hold.take()));
    // SAFETY: `ptr` is `len` bytes of page-aligned mapped memory, a
    // whole number of pages, kept mapped until the block runs
    let buffer = unsafe {
        device.newBufferWithBytesNoCopy_length_options_deallocator(
            ptr,
            len,
            MTLResourceOptions::StorageModeShared,
            Some(&free),
        )
    }?;
    // SAFETY: plain descriptor constructor
    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            b.width as usize,
            b.height as usize,
            false,
        )
    };
    desc.setUsage(MTLTextureUsage::ShaderRead);
    desc.setStorageMode(MTLStorageMode::Shared);
    // the pixels start past the 64-byte header and rows are 16-byte
    // aligned, which a texture over a buffer requires
    let texture = buffer.newTextureWithDescriptor_offset_bytesPerRow(&desc, HEADER_LEN, b.stride as usize)?;
    Some(Texture { texture, buffer })
}
