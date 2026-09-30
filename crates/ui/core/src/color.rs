//! Colour, eight bits a channel.
//!
//! Every instance a scene carries has a colour in it, and there are
//! tens of thousands of them in a frame: twenty panes of an 80×24 grid
//! is 38 400 background rectangles alone.  Four bytes instead of
//! sixteen is 768 KB a frame instead of 1.2 MB, which at 120 Hz is the
//! difference between 92 and 147 MB/s of upload for the cheapest
//! primitive there is.  The cost is one divide in a shader.
//!
//! The limit that buys: this cannot express a colour outside sRGB, so
//! it cannot express EDR.  That is accepted for now, and the way out
//! is a separate primitive and pipeline for wide colour — not widening
//! every primitive for the sake of the few that would use it.
//!
//! Alpha is straight, not premultiplied.  Premultiplying is the
//! backend's business if its blend state wants it; a colour written by
//! a caller should mean what the caller wrote.

/// Straight-alpha sRGB, one byte a channel.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const TRANSPARENT: Self = Self { r: 0, g: 0, b: 0, a: 0 };
    pub const BLACK: Self = Self { r: 0, g: 0, b: 0, a: 255 };
    pub const WHITE: Self = Self { r: 255, g: 255, b: 255, a: 255 };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// `0xRRGGBB`, opaque.  For the places a colour is written as a
    /// literal and reads better as one number than as three.
    pub const fn hex(v: u32) -> Self {
        Self {
            r: (v >> 16) as u8,
            g: (v >> 8) as u8,
            b: v as u8,
            a: 255,
        }
    }

    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    pub fn is_invisible(&self) -> bool {
        self.a == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_colour_is_four_bytes_and_stays_four_bytes() {
        // The whole reason this type exists.  If something grows it,
        // the bandwidth argument in the module note stops being true
        // and the scene's instance sizes move under everyone.
        assert_eq!(core::mem::size_of::<Rgba8>(), 4);
        assert_eq!(core::mem::align_of::<Rgba8>(), 1);
    }

    #[test]
    fn hex_reads_in_the_order_it_is_written() {
        assert_eq!(Rgba8::hex(0x11_22_33), Rgba8::rgba(0x11, 0x22, 0x33, 255));
    }

    #[test]
    fn alpha_is_straight() {
        // Half-transparent white stays 255 in every channel.  If this
        // ever comes back as 128, someone premultiplied on the way in
        // and every colour in the system is now wrong by its own alpha.
        let c = Rgba8::WHITE.with_alpha(128);
        assert_eq!((c.r, c.g, c.b, c.a), (255, 255, 255, 128));
    }
}
