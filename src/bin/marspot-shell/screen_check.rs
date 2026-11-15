//! Whether what the window server composites for a window is the
//! surface the shell last put on it: the right way up, in place, and
//! the newest one.  Only in a build with the measuring compiled in.
//!
//! The process captures its own window, which needs no permission, and
//! compares it with the surface block by block.  Colours pass through
//! the display's profile on the way to the capture, so the measure is
//! how the brightness of the blocks varies together, once as captured
//! and once with the capture turned upside down.

use std::ffi::c_void;

use marspot::iosurface::IOSurface;

#[repr(C)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

type CreateImage = unsafe extern "C" fn(Rect, u32, u32, u32) -> *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetDataProvider(image: *mut c_void) -> *mut c_void;
    fn CGImageGetWidth(image: *mut c_void) -> usize;
    fn CGImageGetHeight(image: *mut c_void) -> usize;
    fn CGImageGetBytesPerRow(image: *mut c_void) -> usize;
    fn CGDataProviderCopyData(provider: *mut c_void) -> *const c_void;
    fn CGImageRelease(image: *mut c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFDataGetLength(data: *const c_void) -> isize;
    fn CFRelease(cf: *const c_void);
}

/// Just this window, without its shadow, one pixel per point.
const ONLY_THIS_WINDOW: u32 = 1 << 3;
const NO_FRAMING_NOMINAL: u32 = 1 | 1 << 4;
/// Blocks are this many pixels square.
const BLOCK: usize = 8;

/// Rows of four-byte pixels with green second, as both a BGRA surface
/// and a capture have them.
pub(crate) struct Pixels {
    pub(crate) bytes: Vec<u8>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) stride: usize,
}

impl Pixels {
    /// The mean green of each block of the bottom `rows` rows and left
    /// `cols` columns, top row first, or bottom row first.
    fn blocks(&self, cols: usize, rows: usize, upside_down: bool) -> Vec<f64> {
        let top = self.height - rows;
        let mut out = Vec::with_capacity((cols / BLOCK) * (rows / BLOCK));
        for by in 0..rows / BLOCK {
            let by = if upside_down { rows / BLOCK - 1 - by } else { by };
            for bx in 0..cols / BLOCK {
                let mut sum = 0u32;
                for y in 0..BLOCK {
                    let row = (top + by * BLOCK + y) * self.stride;
                    for x in 0..BLOCK {
                        sum += self.bytes[row + (bx * BLOCK + x) * 4 + 1] as u32;
                    }
                }
                out.push(sum as f64 / (BLOCK * BLOCK) as f64);
            }
        }
        out
    }
}

fn capture(window: u32) -> Option<Pixels> {
    // SAFETY: a lookup by name in the images already loaded
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"CGWindowListCreateImage".as_ptr()) };
    if symbol.is_null() {
        return None;
    }
    // SAFETY: the symbol is that function, whose signature this is
    let create: CreateImage = unsafe { std::mem::transmute(symbol) };
    // the null rectangle: the bounds of the windows asked for
    let all = Rect { x: f64::INFINITY, y: f64::INFINITY, w: 0.0, h: 0.0 };
    // SAFETY: every object the calls return is released below, once,
    // after its bytes are copied out
    unsafe {
        let image = create(all, ONLY_THIS_WINDOW, window, NO_FRAMING_NOMINAL);
        if image.is_null() {
            return None;
        }
        let (width, height, stride) = (CGImageGetWidth(image), CGImageGetHeight(image), CGImageGetBytesPerRow(image));
        let data = CGDataProviderCopyData(CGImageGetDataProvider(image));
        let bytes = if data.is_null() {
            Vec::new()
        } else {
            let b = std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize).to_vec();
            CFRelease(data);
            b
        };
        CGImageRelease(image);
        (bytes.len() >= height * stride && height > 0).then_some(Pixels { bytes, width, height, stride })
    }
}

/// How two series vary together, from -1 to 1; 0 when either is flat.
fn correlation(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (ma, mb) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let (mut sab, mut saa, mut sbb) = (0f64, 0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        sab += (x - ma) * (y - mb);
        saa += (x - ma) * (x - ma);
        sbb += (y - mb) * (y - mb);
    }
    if saa == 0.0 || sbb == 0.0 { 0.0 } else { sab / (saa * sbb).sqrt() }
}

/// What the comparison of a window with a surface found.
pub(crate) struct Seen {
    /// How the surface and the capture vary together as captured, and
    /// with the capture turned upside down.
    pub(crate) upright: f64,
    pub(crate) flipped: f64,
    pub(crate) capture: (usize, usize),
}

/// Compare the two over the area both cover, from the bottom left: a
/// title bar above the view is in the capture and not in the surface.
pub(crate) fn compare(surface: &Pixels, shot: &Pixels) -> Seen {
    let cols = surface.width.min(shot.width);
    let rows = surface.height.min(shot.height);
    let ours = surface.blocks(cols, rows, false);
    Seen {
        upright: correlation(&ours, &shot.blocks(cols, rows, false)),
        flipped: correlation(&ours, &shot.blocks(cols, rows, true)),
        capture: (shot.width, shot.height),
    }
}

/// Capture window number `window` and compare it with `surface`.
pub(crate) fn check(window: u32, surface: &IOSurface) -> Option<Seen> {
    let (bytes, stride) = surface.read_pixels()?;
    let ours = Pixels { bytes, width: surface.width(), height: surface.height(), stride };
    Some(compare(&ours, &capture(window)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture with no symmetry: brightness grows to the right and
    /// faster downwards.
    fn picture(width: usize, height: usize) -> Pixels {
        let mut bytes = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                bytes[(y * width + x) * 4 + 1] = ((x * 3 + y * y / 7) % 251) as u8;
            }
        }
        Pixels { bytes, width, height, stride: width * 4 }
    }

    #[test]
    fn a_capture_of_the_same_picture_matches_upright_and_not_upside_down() {
        let a = picture(160, 96);
        let seen = compare(&a, &picture(160, 96));
        assert!(seen.upright > 0.999, "{}", seen.upright);
        assert!(seen.flipped < 0.9, "{}", seen.flipped);
    }

    #[test]
    fn a_capture_turned_over_is_told_from_the_picture() {
        let a = picture(160, 96);
        let mut turned = picture(160, 96);
        let rows: Vec<Vec<u8>> = turned.bytes.chunks(160 * 4).rev().map(<[u8]>::to_vec).collect();
        turned.bytes = rows.concat();
        let seen = compare(&a, &turned);
        assert!(seen.flipped > 0.999 && seen.upright < 0.9, "{} {}", seen.upright, seen.flipped);
    }

    /// The capture has a title bar's worth of rows above the view.
    #[test]
    fn rows_above_the_view_are_left_out() {
        let a = picture(160, 96);
        let mut taller = vec![200u8; 160 * 4 * 24];
        taller.extend_from_slice(&a.bytes);
        let shot = Pixels { bytes: taller, width: 160, height: 120, stride: 160 * 4 };
        assert!(compare(&a, &shot).upright > 0.999);
    }
}
