//! The colour space every layer that reaches the screen is tagged with.

use std::ffi::c_void;

use core_graphics::color_space::{kCGColorSpaceSRGB, CGColorSpace};
use foreign_types::ForeignType;
use objc2_quartz_core::CAMetalLayer;

/// Tag a `CAMetalLayer`'s colour space as **sRGB** so ColorSync converts
/// our sRGB-encoded terminal colours to the display correctly.
///
/// Terminal colours (ANSI + 38;2 truecolor) are sRGB by convention.  With
/// NO tag (`nil`), a CAMetalLayer is treated as already-in-display-space —
/// so on a wide-gamut panel the sRGB values are shown without sRGB→display
/// conversion and over-saturate: Claude Code's coral orange came out
/// pink/"水红".  Tagging **Display P3** over-saturates even harder (wider
/// primaries → coral pinker still).  Tagging **sRGB** does the correct
/// conversion and the coral stays orange, matching iTerm2.
///
/// **Every CAMetalLayer that reaches the screen MUST call this** — today
/// that is the shell presenter's layer (`bin/marspot-shell/present.rs`).
/// Two layers once diverged (one created without any tag → over-saturated
/// colour in the installed app); routing every layer through this one
/// helper keeps them from drifting again.  NB: the
/// presenter only applies it on (re)launch — a silent core swap alone
/// won't change the layer, so the shell must actually restart to pick up a
/// colour-space change.
pub fn pin_layer_colorspace(layer: &CAMetalLayer) {
    // SAFETY: `kCGColorSpaceSRGB` is an extern static (reading it is
    // `unsafe`); `setColorspace` is reached via raw `objc_msgSend`
    // because the `colorspace` property isn't in the objc2-quartz-core
    // binding yet AND objc2's macro runtime-checks the @encode type
    // ("^{CGColorSpace=}") which doesn't match our `*mut c_void`
    // (`^v`) cast.  Release ships fine because the check is debug-
    // only; for dev (`bin/run.sh`) we go through the raw FFI path
    // so debug builds boot too.
    unsafe {
        let cs = CGColorSpace::create_with_name(kCGColorSpaceSRGB).expect(
            "CGColorSpaceCreateWithName(kCGColorSpaceSRGB) cannot fail on supported macOS",
        );
        let cs_ptr = cs.as_ptr() as *mut c_void;
        let layer_ptr: *mut objc2::runtime::AnyObject =
            (layer as *const CAMetalLayer as *mut CAMetalLayer).cast();
        let sel = objc2::sel!(setColorspace:);
        type Setter = unsafe extern "C" fn(
            *mut objc2::runtime::AnyObject,
            objc2::runtime::Sel,
            *mut c_void,
        );
        let imp: Setter = std::mem::transmute(objc2::ffi::objc_msgSend as *const ());
        imp(layer_ptr, sel, cs_ptr);
    }
}
