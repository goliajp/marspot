//! Minimal IOSurface FFI + Metal texture bridge.
//!
//! IOSurface is the macOS primitive that lets two processes share a
//! single GPU buffer with zero copies.  We use it as the bridge between
//! the marspot **shell** process (window owner, supervisor) and the
//! marspot **core** process (renderer): the shell creates an IOSurface,
//! publishes its global ID to the core, and the core wraps it in an
//! MTLTexture and draws into it.  The shell, in turn, samples the same
//! IOSurface as an MTLTexture and presents it to the CAMetalLayer.
//!
//! When the core is killed and re-exec'd during a silent update, the
//! IOSurface stays alive in the shell's address space — the last frame
//! the old core wrote is what the user keeps seeing until the new core
//! attaches and begins writing.  This is what makes "no window
//! flicker" possible.
//!
//! ### Why hand-rolled FFI instead of a crate
//!
//! - `objc2-metal` deliberately skips `newTextureWithDescriptor:iosurface:plane:`
//!   because its `translation-config.toml` flags IOSurfaceRef as
//!   manual-binding territory.
//! - `io-surface` (the only crate) hasn't been updated for the objc2
//!   ecosystem and pulls its own conflicting CFRef wrappers.
//! - We need ~80 LOC of FFI; self-build is the right call.

use core_foundation::{
    base::{CFType, TCFType},
    boolean::CFBoolean,
    dictionary::CFDictionary,
    number::CFNumber,
    string::CFString,
};
use objc2::{
    encode::{Encoding, RefEncode},
    msg_send_id,
    rc::Retained,
    runtime::ProtocolObject,
};
use objc2_metal::{
    MTLDevice, MTLPixelFormat, MTLStorageMode, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

#[repr(C)]
pub struct __IOSurface {
    _private: [u8; 0],
}

// Teach objc2 that `*mut __IOSurface` should encode as `^{__IOSurface=}`
// — the type encoding Metal's `-newTextureWithDescriptor:iosurface:plane:`
// runtime check insists on for its `IOSurfaceRef` parameter.  Without
// this impl, msg_send_id would pass it as `^v` (void*) and fail at
// dispatch time.
unsafe impl RefEncode for __IOSurface {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("__IOSurface", &[]));
}

/// Opaque IOSurface kernel object pointer.
pub type IOSurfaceRef = *mut __IOSurface;

/// Globally-unique 32-bit ID assigned by the kernel at creation time.
/// Stable across processes — pass this to a child via env var or socket
/// and the child can `IOSurface::lookup(id)` to attach.
pub type IOSurfaceID = u32;

#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    fn IOSurfaceCreate(properties: core_foundation::dictionary::CFDictionaryRef) -> IOSurfaceRef;
    fn IOSurfaceLookup(csid: IOSurfaceID) -> IOSurfaceRef;
    fn IOSurfaceGetID(buffer: IOSurfaceRef) -> IOSurfaceID;
    fn IOSurfaceGetWidth(buffer: IOSurfaceRef) -> usize;
    fn IOSurfaceGetHeight(buffer: IOSurfaceRef) -> usize;
    fn IOSurfaceIncrementUseCount(buffer: IOSurfaceRef);
    fn IOSurfaceDecrementUseCount(buffer: IOSurfaceRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const std::ffi::c_void);
    fn CFRetain(cf: *const std::ffi::c_void) -> *const std::ffi::c_void;
}

/// Owned IOSurface handle.  `Drop` releases the kernel reference.
pub struct IOSurface {
    raw: IOSurfaceRef,
}

// IOSurface is a kernel-managed object reachable by ID from any task;
// it is safe to move the wrapper between threads.  We don't expose
// interior mutability on the wrapper itself, so &IOSurface is also Sync.
unsafe impl Send for IOSurface {}
unsafe impl Sync for IOSurface {}

impl IOSurface {
    /// Create a fresh `width × height` BGRA8 (8 bits per channel, 4 bytes
    /// per pixel) IOSurface.  Picks the pixel format that matches our
    /// existing Metal pipeline (`MTLPixelFormat::BGRA8Unorm`).
    ///
    /// Returns the wrapper with refcount 1.
    pub fn create(width: usize, height: usize) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err(format!(
                "IOSurface::create: zero dimension w={width} h={height}"
            ));
        }
        // Metal's IOSurface-texture validation requires `bytesPerRow`
        // to be a multiple of 16 (Apple Silicon GPU page granularity).
        // At BGRA8 = 4 bytes/pixel, any width that isn't a multiple of
        // 4 produces a non-aligned stride — e.g. width=1187 → 4748
        // bytes/row, fails with
        // `_mtlValidateStrideTextureParameters … must be aligned to 16
        // bytes` and *aborts* the process.  Pad up the row stride; the
        // logical width stays width, Metal samples through the stride.
        let raw_bpr = width.saturating_mul(4);
        let bytes_per_row = (raw_bpr + 15) & !15;
        // 'BGRA' fourCC = 0x42475241 = kCVPixelFormatType_32BGRA.
        let pixel_format = i32::from_be_bytes(*b"BGRA");

        let pairs: Vec<(CFString, CFType)> = vec![
            (
                CFString::new("IOSurfaceWidth"),
                CFNumber::from(width as i64).as_CFType(),
            ),
            (
                CFString::new("IOSurfaceHeight"),
                CFNumber::from(height as i64).as_CFType(),
            ),
            (
                CFString::new("IOSurfaceBytesPerElement"),
                CFNumber::from(4i32).as_CFType(),
            ),
            (
                CFString::new("IOSurfaceBytesPerRow"),
                CFNumber::from(bytes_per_row as i64).as_CFType(),
            ),
            (
                CFString::new("IOSurfacePixelFormat"),
                CFNumber::from(pixel_format).as_CFType(),
            ),
            // Mark the surface as globally lookupable by ID across
            // processes.  Deprecated since 10.13 but still functional
            // on non-sandboxed apps through Sonoma+.  Required so the
            // core's `IOSurfaceLookup(id)` works on the same surface
            // we just created.  When we wire a Mach-port-based handoff
            // in Step 3, drop this and use IOSurfaceCreateMachPort
            // + cross-process mach_msg transfer instead.
            (
                CFString::new("IOSurfaceIsGlobal"),
                CFBoolean::true_value().as_CFType(),
            ),
        ];
        let dict = CFDictionary::from_CFType_pairs(&pairs);

        let raw = unsafe { IOSurfaceCreate(dict.as_concrete_TypeRef()) };
        if raw.is_null() {
            return Err("IOSurfaceCreate returned nil".to_string());
        }
        Ok(IOSurface { raw })
    }

    /// Look up an existing IOSurface by its globally-unique ID.  Returns
    /// `None` if the kernel has no surface with that ID (creator died
    /// and dropped its last reference, or ID never existed).  Increments
    /// the kernel refcount on success.
    pub fn lookup(id: IOSurfaceID) -> Option<Self> {
        let raw = unsafe { IOSurfaceLookup(id) };
        if raw.is_null() {
            None
        } else {
            Some(IOSurface { raw })
        }
    }

    pub fn id(&self) -> IOSurfaceID {
        unsafe { IOSurfaceGetID(self.raw) }
    }

    pub fn width(&self) -> usize {
        unsafe { IOSurfaceGetWidth(self.raw) }
    }

    pub fn height(&self) -> usize {
        unsafe { IOSurfaceGetHeight(self.raw) }
    }

    pub fn raw(&self) -> IOSurfaceRef {
        self.raw
    }

    /// Increment the kernel "in use" count.  Drivers / compositors
    /// use this as a hint that the surface is being read or written;
    /// for our use-case (shell sampling + core rendering on the same
    /// surface) we bump once at attach and decrement on drop.
    pub fn increment_use(&self) {
        unsafe { IOSurfaceIncrementUseCount(self.raw) }
    }

    pub fn decrement_use(&self) {
        unsafe { IOSurfaceDecrementUseCount(self.raw) }
    }

    /// Wrap this IOSurface in an MTLTexture suitable for both rendering
    /// (RenderTarget) and sampling (ShaderRead).  Plane 0 — we only
    /// use single-plane BGRA8.
    pub fn make_metal_texture(
        &self,
        device: &ProtocolObject<dyn MTLDevice>,
    ) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
        let descriptor = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::BGRA8Unorm,
                self.width(),
                self.height(),
                false,
            )
        };
        descriptor.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
        // IOSurface-backed textures cannot use Private storage; Shared is
        // the only mode that lets the surface bytes be visible to both
        // processes.
        descriptor.setStorageMode(MTLStorageMode::Shared);
        // newTextureWithDescriptor:iosurface:plane: is the documented
        // entry point but objc2-metal skips it (IOSurfaceRef is a manual
        // type).  Hand-dispatch via msg_send_id.  Pass the IOSurfaceRef
        // with its actual struct-pointer Objective-C type encoding so
        // the runtime's argument typecheck accepts it; a `*const c_void`
        // would arrive encoded as `^v` and trip the check.
        let surface_ptr: IOSurfaceRef = self.raw;
        let tex: Option<Retained<ProtocolObject<dyn MTLTexture>>> = unsafe {
            msg_send_id![
                device,
                newTextureWithDescriptor: &*descriptor,
                iosurface: surface_ptr,
                plane: 0usize
            ]
        };
        tex.ok_or_else(|| "newTextureWithDescriptor:iosurface:plane: returned nil".to_string())
    }
}

impl Drop for IOSurface {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe { CFRelease(self.raw as *const std::ffi::c_void) };
        }
    }
}

impl Clone for IOSurface {
    fn clone(&self) -> Self {
        unsafe { CFRetain(self.raw as *const std::ffi::c_void) };
        IOSurface { raw: self.raw }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_lookup_roundtrip() {
        let s = IOSurface::create(64, 32).expect("create");
        assert_eq!(s.width(), 64);
        assert_eq!(s.height(), 32);
        let id = s.id();
        assert!(id != 0);

        let s2 = IOSurface::lookup(id).expect("lookup");
        assert_eq!(s2.id(), id);
        assert_eq!(s2.width(), 64);
        assert_eq!(s2.height(), 32);
    }

    #[test]
    fn lookup_missing_returns_none() {
        // IDs are 32-bit; pick one extremely unlikely to ever be issued.
        let missing = IOSurface::lookup(0xFFFF_FFFE);
        assert!(missing.is_none());
    }

    #[test]
    fn create_rejects_zero_dim() {
        assert!(IOSurface::create(0, 100).is_err());
        assert!(IOSurface::create(100, 0).is_err());
    }
}
