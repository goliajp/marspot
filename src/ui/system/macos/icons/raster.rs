//! A system symbol as a coverage mask.
//!
//! AppKit draws the symbol, as a vector, into a bitmap context of the
//! mask's own size; the mask is that bitmap's alpha.  Nothing here
//! needs a window, an application object or the main thread.

use core_graphics::base::kCGImageAlphaPremultipliedLast;
use core_graphics::color_space::CGColorSpace;
use core_graphics::context::CGContext;
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use foreign_types::ForeignType;
use objc2::encode::{Encoding, RefEncode};
use objc2::rc::{autoreleasepool, Retained};
use objc2::{msg_send, ClassType};
use objc2_app_kit::{NSFontWeightMedium, NSGraphicsContext, NSImage, NSImageSymbolConfiguration};
use objc2_foundation::{NSPoint, NSRect, NSString};

/// A `CGContext` as the Objective-C runtime spells it, to hand the
/// bitmap context to `NSGraphicsContext`.
#[repr(C)]
struct OpaqueCGContext {
    _private: [u8; 0],
}

// SAFETY: the encoding AppKit declares for a `CGContextRef` argument.
unsafe impl RefEncode for OpaqueCGContext {
    const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct("CGContext", &[]));
}

/// How far the drawing is moved, on each axis, to look for the place
/// where its strokes sit on whole pixels.  A symbol's outline is not
/// hinted: drawn where its metrics put it, a stroke a pixel thick can
/// fall across two rows and come out as two rows of grey.
const NUDGES: [f64; 2] = [0.0, -0.5];

/// Draw the symbol `name` at `point_px` into `mask`, `side` × `side`
/// bytes of coverage, its alignment rect centred.  A name the system
/// does not have leaves the mask empty.
pub(super) fn rasterise(name: &str, point_px: u32, side: u32, mask: &mut [u8]) {
    autoreleasepool(|_| {
        // SAFETY: a constant AppKit exports.
        let weight = unsafe { NSFontWeightMedium };
        let config =
            NSImageSymbolConfiguration::configurationWithPointSize_weight(point_px as f64, weight);
        let Some(image) =
            NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)
                .and_then(|symbol| symbol.imageWithSymbolConfiguration(&config))
        else {
            return;
        };
        let size = image.size();
        let align = image.alignmentRect();
        let n = side as usize;
        let mut bitmap = CGContext::create_bitmap_context(
            None,
            n,
            n,
            8,
            n * 4,
            &CGColorSpace::create_device_rgb(),
            kCGImageAlphaPremultipliedLast,
        );
        // SAFETY: `bitmap` is a live CGContext for the whole call, and
        // the selector takes a CGContextRef and a BOOL.
        let context: Retained<NSGraphicsContext> = unsafe {
            msg_send![
                NSGraphicsContext::class(),
                graphicsContextWithCGContext: bitmap.as_ptr() as *const OpaqueCGContext,
                flipped: false
            ]
        };
        let x = (side as f64 - align.size.width) * 0.5 - align.origin.x;
        let y = (side as f64 - align.size.height) * 0.5 - align.origin.y;
        let whole = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(side as f64, side as f64));
        let mut sharpest = 0u64;
        for dx in NUDGES {
            for dy in NUDGES {
                bitmap.clear_rect(whole);
                NSGraphicsContext::saveGraphicsState_class();
                NSGraphicsContext::setCurrentContext(Some(&context));
                image.drawInRect(NSRect::new(NSPoint::new(x + dx, y + dy), size));
                NSGraphicsContext::restoreGraphicsState_class();
                // the same ink gathered into fewer pixels sums to more
                let alpha = bitmap.data().iter().skip(3).step_by(4);
                let sharpness: u64 = alpha.clone().map(|&a| a as u64 * a as u64).sum();
                if sharpness > sharpest {
                    sharpest = sharpness;
                    for (out, &a) in mask.iter_mut().zip(alpha) {
                        *out = a;
                    }
                }
            }
        }
    });
}
