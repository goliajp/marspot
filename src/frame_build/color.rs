//! Colours as the instance format stores them.

/// A premultiplication-free `[f32; 4]` in 0..1 as the published
/// format's eight bits a channel.
///
/// The float path carried colours this way everywhere. Eight bits is
/// what `Rgba8` has, so an alpha of 0.5 becomes 128/255 and the pixels
/// it touches land one level off -- measured, bounded, and the whole
/// of what the move costs.
pub fn rgba8_of_f32(c: [f32; 4]) -> golia_ui_core::Rgba8 {
    golia_ui_core::Rgba8::rgba(
        (c[0] * 255.0).round().clamp(0.0, 255.0) as u8,
        (c[1] * 255.0).round().clamp(0.0, 255.0) as u8,
        (c[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        (c[3] * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

pub(crate) fn rgba8_of(c: crate::ui::core::Color) -> golia_ui_core::Rgba8 {
    let [r, g, b, a] = c.to_rgba_f32();
    golia_ui_core::Rgba8::rgba(
        (r * 255.0).round().clamp(0.0, 255.0) as u8,
        (g * 255.0).round().clamp(0.0, 255.0) as u8,
        (b * 255.0).round().clamp(0.0, 255.0) as u8,
        (a * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}
