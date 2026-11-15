//! The chrome's icons: SF Symbols, the system's own set.
//!
//! One table names every icon the chrome draws.  A symbol is
//! rasterised by AppKit to a coverage mask at the exact pixel size it
//! is drawn at, kept in the glyph atlas beside the font's glyphs and
//! tinted like one, so an icon follows the theme the way text does and
//! the atlas's eviction bounds it.
//!
//! A mask is rasterised on its first use at a size and never on a
//! frame after that.

mod raster;

use marspot_term::layout::Rect;

use crate::frame_build::color::rgba8_of_f32;
use crate::glyph_atlas::{FontId, GlyphKey, BOX_DRAWING_FONT_ID};
use crate::ui::core::ViewPainter;
use golia_ui_core::scene::GlyphInstance;

/// Every icon in the chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Symbol {
    /// Show or hide the sidebar.
    Sidebar,
    /// The pane layout picker.
    Layout,
    /// The process monitor.
    Processes,
    /// The accounts' usage.
    Usage,
    /// Settings.
    Settings,
    /// Add: a pane, a seat.
    Plus,
    /// Close: a pane, a panel, the search bar; end a process.
    Close,
}

impl Symbol {
    pub const ALL: [Symbol; 7] = [
        Symbol::Sidebar,
        Symbol::Layout,
        Symbol::Processes,
        Symbol::Usage,
        Symbol::Settings,
        Symbol::Plus,
        Symbol::Close,
    ];

    /// The symbol's name in SF Symbols.  Every one of these has been
    /// in the set since macOS 11.
    pub const fn name(self) -> &'static str {
        match self {
            Symbol::Sidebar => "sidebar.left",
            Symbol::Layout => "rectangle.split.2x2",
            Symbol::Processes => "list.bullet.indent",
            Symbol::Usage => "chart.bar",
            Symbol::Settings => "slider.horizontal.3",
            Symbol::Plus => "plus",
            Symbol::Close => "xmark",
        }
    }
}

/// The ink of `plus` and `xmark` is this fraction of the point size
/// across: what a caller that wants a mark of a given width divides by.
pub const MARK_INK: f64 = 0.8;

/// The atlas's font field for a symbol's mask, beside the one the
/// box-drawing masks take: no font cache index reaches either.
const SYMBOL_FONT_ID: FontId = BOX_DRAWING_FONT_ID - 1;

/// Side of the square mask a symbol at `point_px` is drawn into.  The
/// widest symbol in the table is 1.3 point sizes across; even, so the
/// mask centres on a whole pixel in a button of an even size.
pub(crate) fn mask_side(point_px: u32) -> u32 {
    2 * (point_px * 7).div_ceil(8)
}

/// Set by the shell on the core it starts: the core asks the shell for
/// every mask instead of drawing it.  Drawing a system symbol registers
/// the process with LaunchServices (measured on macOS 27.2.1: loading
/// the image is enough), and a registered core can be given the
/// sessions' coalition, or keep it on its dead record after it exits.
/// The shell is the registered application anyway.  A core started by a
/// shell from before this does not see it and draws its own.
pub const FROM_SHELL_ENV: &str = "MARSPOT_ICONS_FROM_SHELL";

fn from_shell() -> bool {
    static FROM_SHELL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FROM_SHELL.get_or_init(|| std::env::var_os(FROM_SHELL_ENV).is_some())
}

/// The masks the shell drew for this process, and the ones it is still
/// to be asked for.
struct Served {
    masks: std::collections::VecDeque<((u16, u16), Vec<u8>)>,
    asked: Vec<(u16, u16)>,
    to_ask: Vec<(u16, u16)>,
}

/// Seven symbols at the few sizes the chrome uses; a bound all the
/// same, oldest out first.
const SERVED_MOST: usize = 64;

static SERVED: std::sync::Mutex<Served> =
    std::sync::Mutex::new(Served { masks: std::collections::VecDeque::new(), asked: Vec::new(), to_ask: Vec::new() });

fn served() -> std::sync::MutexGuard<'static, Served> {
    // nothing panics while holding it; a poisoned guard is as good
    SERVED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Note `symbol` at `point_px` as wanted, once until it arrives.
fn want(symbol: u16, point_px: u16) {
    let mut s = served();
    if !s.asked.contains(&(symbol, point_px)) {
        s.asked.push((symbol, point_px));
        s.to_ask.push((symbol, point_px));
    }
}

/// The masks to ask the shell for since the last call: `(symbol,
/// point_px)`.
pub fn take_wanted() -> Vec<(u16, u16)> {
    std::mem::take(&mut served().to_ask)
}

/// A mask the shell sent.  False, and nothing kept, when it is not one
/// this process could have asked for: it crossed a process boundary.
pub fn deliver(symbol: u16, point_px: u16, side: u16, mask: Vec<u8>) -> bool {
    if symbol as usize >= Symbol::ALL.len()
        || !(1..=MOST_POINT_PX).contains(&point_px)
        || side as u32 != mask_side(point_px as u32)
        || mask.len() != side as usize * side as usize
    {
        return false;
    }
    let mut s = served();
    s.asked.retain(|k| *k != (symbol, point_px));
    s.masks.retain(|(k, _)| *k != (symbol, point_px));
    if s.masks.len() >= SERVED_MOST {
        s.masks.pop_front();
    }
    s.masks.push_back(((symbol, point_px), mask));
    ARRIVALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}

static ARRIVALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many masks the shell has sent so far: what a picture kept from
/// an earlier frame, which may have left an icon out, is checked by.
pub fn arrivals() -> u64 {
    ARRIVALS.load(std::sync::atomic::Ordering::Relaxed)
}

/// The largest point size a mask is drawn at on request.
const MOST_POINT_PX: u16 = 256;

/// For the shell: draw `symbol` at `point_px` as the core asked;
/// `(side, mask)`, or `None` for a request no core would make.
pub fn draw_for_core(symbol: u16, point_px: u16) -> Option<(u16, Vec<u8>)> {
    let symbol = *Symbol::ALL.get(symbol as usize)?;
    if !(1..=MOST_POINT_PX).contains(&point_px) {
        return None;
    }
    let side = mask_side(point_px as u32);
    let mut mask = vec![0u8; (side * side) as usize];
    raster::rasterise(symbol.name(), point_px as u32, side, &mut mask);
    Some((side as u16, mask))
}

/// Have AppKit load the symbol set, on a thread of its own.
///
/// The first symbol a process asks for costs it the set itself, a
/// tenth of a second; every one after that is drawn in well under a
/// millisecond.  Called when the display's scale is adopted, before
/// there is a renderer, so the first frame does not pay it.
pub fn warm() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if from_shell() {
            for symbol in Symbol::ALL {
                want(symbol as u16, 13);
            }
            return;
        }
        std::thread::spawn(|| {
            const POINT_PX: u32 = 13;
            let side = mask_side(POINT_PX);
            let mut mask = [0u8; 24 * 24];
            for symbol in Symbol::ALL {
                raster::rasterise(symbol.name(), POINT_PX, side, &mut mask);
            }
        });
    });
}

/// `symbol` at a point size of `point_px` physical pixels, centred on
/// `rect` by its alignment rect, in `color`.
///
/// The mask lands on whole pixels: its strokes were placed on the
/// pixel grid when it was rasterised and stay there.
pub fn paint(p: &mut ViewPainter, symbol: Symbol, rect: Rect, point_px: f64, color: [f32; 4]) {
    let point_px = point_px.round().max(1.0) as u32;
    let side = mask_side(point_px);
    // one weight for the whole set, so the key is the symbol and its size
    let key = GlyphKey::new(SYMBOL_FONT_ID, symbol as u16, point_px as u16, 0, 0);
    // drawn by the shell: until its mask is here the icon is left out,
    // and the frame that follows its arrival has it
    let shell_drew = if from_shell() && !p.atlas.has(&key) {
        let found = served().masks.iter().find(|(k, _)| *k == (symbol as u16, point_px as u16)).map(|(_, m)| m.clone());
        match found {
            Some(mask) => Some(mask),
            None => {
                want(symbol as u16, point_px as u16);
                return;
            }
        }
    } else {
        None
    };
    let Some(entry) = p.atlas.get_or_insert_custom_raster(key, side, side, 0, 1, |mask| match &shell_drew {
        Some(drawn) => mask.copy_from_slice(drawn),
        None => raster::rasterise(symbol.name(), point_px, side, mask),
    }) else {
        return;
    };
    let side = side as f64;
    p.glyphs.push(GlyphInstance {
        origin: [
            (rect.x + (rect.w - side) * 0.5).round() as f32,
            (rect.y_top + (rect.h - side) * 0.5).round() as f32,
        ],
        size: [side as f32, side as f32],
        uv0: [entry.u0 as f32 / p.atlas_w, entry.v0 as f32 / p.atlas_h],
        uv1: [entry.u1 as f32 / p.atlas_w, entry.v1 as f32 / p.atlas_h],
        color: rgba8_of_f32(color),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mask_from_the_shell_is_kept_only_when_it_is_one_that_could_be_asked_for() {
        let side = mask_side(20) as u16;
        let fits = vec![1u8; side as usize * side as usize];
        assert!(!deliver(Symbol::ALL.len() as u16, 20, side, fits.clone()), "no such symbol");
        assert!(!deliver(0, 0, side, fits.clone()), "no size");
        assert!(!deliver(0, MOST_POINT_PX + 1, side, fits.clone()), "too large");
        assert!(!deliver(0, 20, side + 2, fits.clone()), "a side the size does not have");
        assert!(!deliver(0, 20, side, vec![1u8; 3]), "a mask of the wrong length");
        want(1, 20);
        want(1, 20);
        assert_eq!(take_wanted().iter().filter(|k| **k == (1, 20)).count(), 1, "asked for once");
        let before = arrivals();
        assert!(deliver(1, 20, side, fits));
        assert!(arrivals() > before, "a picture kept without the icon has to be told it arrived");
        want(1, 20);
        assert!(take_wanted().contains(&(1, 20)), "asked for again once the one asked for has arrived");
    }

    #[test]
    fn the_shell_draws_what_a_core_asks_for_and_nothing_else() {
        let (side, mask) = draw_for_core(Symbol::Plus as u16, 26).expect("a mask");
        assert_eq!(side as u32, mask_side(26));
        assert!(mask.iter().any(|&a| a > 0));
        assert!(draw_for_core(Symbol::ALL.len() as u16, 26).is_none());
        assert!(draw_for_core(0, 0).is_none());
    }

    fn mask(symbol: Symbol, point_px: u32) -> (Vec<u8>, usize) {
        let side = mask_side(point_px);
        let mut mask = vec![0u8; (side * side) as usize];
        raster::rasterise(symbol.name(), point_px, side, &mut mask);
        (mask, side as usize)
    }

    /// `(left, top, right, bottom)` of the pixels at least half covered,
    /// the last two exclusive.
    fn ink(mask: &[u8], side: usize) -> (usize, usize, usize, usize) {
        let on = |x: usize, y: usize| mask[y * side + x] >= 128;
        let cols: Vec<usize> = (0..side).filter(|&x| (0..side).any(|y| on(x, y))).collect();
        let rows: Vec<usize> = (0..side).filter(|&y| (0..side).any(|x| on(x, y))).collect();
        (cols[0], rows[0], cols[cols.len() - 1] + 1, rows[rows.len() - 1] + 1)
    }

    #[test]
    fn every_symbol_in_the_table_resolves_and_fits_its_mask() {
        let started = std::time::Instant::now();
        let (first, _) = mask(Symbol::Sidebar, 13);
        let first_ms = started.elapsed().as_secs_f64() * 1e3;
        assert!(first.iter().any(|&a| a > 0), "sidebar.left drew nothing");
        for point_px in [13, 26] {
            let started = std::time::Instant::now();
            for symbol in Symbol::ALL {
                let (mask, side) = mask(symbol, point_px);
                assert!(
                    mask.iter().any(|&a| a >= 128),
                    "{} drew nothing at {point_px} px",
                    symbol.name()
                );
                let edge = (0..side).any(|i| {
                    [i, (side - 1) * side + i, i * side, i * side + side - 1]
                        .into_iter()
                        .any(|at| mask[at] > 0)
                });
                assert!(!edge, "{} is cut off by its mask at {point_px} px", symbol.name());
            }
            eprintln!(
                "the table of {} symbols at {point_px} px: {:.2} ms",
                Symbol::ALL.len(),
                started.elapsed().as_secs_f64() * 1e3
            );
        }
        eprintln!("the first symbol this process drew: {first_ms:.2} ms");
    }

    /// A stroke is crisp when a line across it meets a pixel it covers
    /// at least nine parts in ten.  A stroke that straddles two pixels
    /// covers neither that far: it is two rows of grey.  Held for the
    /// upright strokes of the sidebar's frame (a row through the middle
    /// of the ink) and for its level ones (a column through the middle),
    /// at display scale 1 and 2.
    #[test]
    fn the_sidebar_symbol_has_crisp_strokes_at_both_scales() {
        const CRISP: u8 = 230;
        for point_px in [13, 26] {
            let (mask, side) = mask(Symbol::Sidebar, point_px);
            let (left, top, right, bottom) = ink(&mask, side);
            let row = (top + bottom) / 2;
            let upright = (left..right).map(|x| mask[row * side + x]).max().unwrap();
            let col = (left + right) / 2;
            let level = (top..bottom).map(|y| mask[y * side + col]).max().unwrap();
            assert!(
                upright >= CRISP && level >= CRISP,
                "at {point_px} px the frame's strokes peak at {upright} (upright) and {level} (level) of 255",
            );
        }
    }

    #[test]
    fn a_mask_is_centred_on_the_symbols_alignment_rect() {
        // the alignment rect is where the system says the symbol's
        // optical middle is, which is not quite its ink's: the frame of
        // the sidebar symbol sits within an eighth of a point size of
        // the middle of the mask, on both axes
        for point_px in [13usize, 26] {
            let (mask, side) = mask(Symbol::Sidebar, point_px as u32);
            let (left, top, right, bottom) = ink(&mask, side);
            assert!(left.abs_diff(side - right) <= point_px / 8, "{left} | {}", side - right);
            assert!(top.abs_diff(side - bottom) <= point_px / 8, "{top} | {}", side - bottom);
        }
    }

    #[test]
    fn a_mask_holds_the_widest_symbol_at_any_size() {
        for point_px in 5..=64 {
            assert!(mask_side(point_px) as f64 >= 1.3 * point_px as f64 + 2.0);
            assert_eq!(mask_side(point_px) % 2, 0);
        }
    }
}
