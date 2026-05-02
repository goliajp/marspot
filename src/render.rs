//! Metal rendering pipeline.
//!
//! Phase 1.2.0 only does the foundation: attach a `CAMetalLayer` to the
//! winit-created `NSView`, set up an `MTLDevice` + command queue, and on
//! every redraw fire a clear-color render pass.  No glyphs, no quads —
//! just proof that we control every pixel.  Subsequent phases (1.2.1+)
//! will layer the glyph atlas, instanced quad pipeline, and grid→pixel
//! wiring on top of this.

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_app_kit::NSView;
use objc2_foundation::CGSize;
use objc2_metal::{
    MTLClearColor, MTLCommandBuffer, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLLoadAction, MTLPixelFormat,
    MTLRenderPassDescriptor, MTLStoreAction,
};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};

pub struct Renderer {
    /// Holding the device alive even though we don't read it back — the
    /// layer keeps a reference but explicit ownership documents intent.
    _device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    layer: Retained<CAMetalLayer>,
}

impl Renderer {
    /// Build a renderer bound to the given `NSView`.  Caller must invoke
    /// from the main thread (AppKit requirement).  The view is given a
    /// `CAMetalLayer` as its backing layer.
    pub fn new(view: &NSView) -> Result<Self, &'static str> {
        // SAFETY: MTLCreateSystemDefaultDevice has Cocoa "Create" naming —
        // returns +1 retain.  We adopt that ownership via Retained::from_raw.
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

        // Layer-backed view: AppKit will composite our Metal output.
        // setLayer takes &CALayer; we double-deref to walk Retained → CAMetalLayer → CALayer.
        view.setWantsLayer(true);
        unsafe { view.setLayer(Some(&**layer)) };

        Ok(Self { _device: device, queue, layer })
    }

    /// Update the drawable size when the window resizes.  Width and height
    /// are in pixels (caller multiplies DPI scale * logical size).
    pub fn resize(&self, width_px: f64, height_px: f64) {
        unsafe {
            self.layer
                .setDrawableSize(CGSize::new(width_px.max(1.0), height_px.max(1.0)));
        }
    }

    /// One frame: clear-color pass and present.  Cheap when there's
    /// nothing to draw — we'll wire real geometry in 1.2.2.
    pub fn render(&self) {
        let Some(drawable) = (unsafe { self.layer.nextDrawable() }) else {
            // Layer not ready yet (e.g., zero-sized window during init);
            // skip this frame rather than crash.
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
            // Slightly cool dark — distinct from the system default so we
            // can tell at a glance that we drew this.
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
        unsafe { encoder.endEncoding() };
        buffer.presentDrawable(ProtocolObject::from_ref(&*drawable));
        buffer.commit();
    }
}
