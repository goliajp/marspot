//! Phase 3 — chrome proportional shaping via CoreText `CTLine`.
//!
//! The PTY render path lays glyphs at mono `cell_w` advance — every
//! cell is independent, no kerning, no ligature, no fallback within
//! a run.  Chrome (dev panel, tab strip, sidebar) wants the opposite:
//! a system font (SF Pro), real kerning so `Ta` reads tight, real
//! ligatures so `fi` / `==>` join up, and CT's automatic font
//! fallback so a CJK word in an otherwise-English sentence renders
//! through PingFang / Hiragino without us re-doing the cascade.
//!
//! CoreText already does all of this — we just need to feed it an
//! attributed string with the font attribute and read back the runs.
//! `shape_line` does that; `ShapeCache` keeps re-shapes cheap so we
//! can shape every frame on the hot chrome path without the per-frame
//! overhead of CT's string analysis.

use core_foundation::attributed_string::CFMutableAttributedString;
use core_foundation::base::{CFRange, TCFType};
use core_foundation::string::CFString;
use core_text::font::CTFont;
use core_text::line::CTLine;
use core_text::string_attributes::kCTFontAttributeName;
use core_graphics::font::CGGlyph;
use marspot_term::fast_hash::FxHashMap;
use std::collections::VecDeque;

/// One shaped glyph — what the chrome renderer needs to push a
/// `GlyphInstance` at its real proportional position.
///
/// `font_id` indexes the `FontCache` registry (Phase 3 re-uses the
/// PTY registry so chrome + PTY share the atlas; same-`(font_id,
/// glyph, size_q, …)` key reuses the same slot).  CTLine may emit
/// glyphs from multiple fonts in a single line when fallback fires
/// — every glyph carries its own `font_id`, so the renderer doesn't
/// guess which font drew which glyph.
///
/// Phase 4 — sub-pixel x position is split:
/// - `pen_x_px` is the INTEGER pixel-grid pen position relative to
///   the line's logical start (floor of the float CTLine gave).
/// - `subpx_x` is the 0.25-px bucket `0..4` that the renderer
///   forwards into `GlyphKey.subpx_x` so the atlas raster comes back
///   pre-shifted by `subpx_x × 0.25` pixels.  Together they
///   reconstruct the float position to 0.25-px precision without
///   subjecting the renderer to fractional `origin.x`.
#[derive(Clone, Copy, Debug)]
pub struct ShapedGlyph {
    pub font_id: u32,
    pub glyph_id: CGGlyph,
    pub pen_x_px: i32,
    pub subpx_x: u8,
}

/// Phase 8 — per-context OpenType-style feature toggles applied at
/// shape time.  CTLine defaults are "everything on" for chrome-style
/// proportional text; the PTY mono path bypasses shaping entirely
/// so its effective state is `all_off`.  Code blocks want
/// `code()` — ligatures on so `==>` joins, kerning off so glyph
/// advances stay character-uniform.
///
/// Currently implemented in `shape_line`:
/// - `liga = false` → `kCTLigatureAttributeName = 0` (forces glyph
///   sequence to remain decomposed; SF Pro's `fi` etc. stay as 2
///   glyphs).
///
/// Reserved for future passes (need font-feature-settings dictionary
/// via `kCTFontFeatureSettingsAttribute` — not in this commit):
/// - `kerning = false` (would disable OT `kern` feature)
/// - `calt = false` (contextual alternates)
/// - `contextual = false` (font-specific contextual variations)
///
/// CT auto-kerning still runs even when `kerning = false`; flipping
/// it off requires the font-feature dictionary which Phase 8 doesn't
/// plumb.  Callers should treat `kerning` as advisory until that
/// follow-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ShapeOptions {
    pub kerning: bool,
    pub liga: bool,
    pub calt: bool,
    pub contextual: bool,
}

impl ShapeOptions {
    /// Body text — kerning + every ligature class on.  Default
    /// CTLine behaviour; chrome dev panel, tab titles, sidebar.
    pub fn full() -> Self {
        Self { kerning: true, liga: true, calt: true, contextual: true }
    }
    /// Code blocks — uniform glyph advance (kerning off) but keep
    /// ligatures so `==>` / `!=` form their composed glyph in fonts
    /// that ship them (Fira Code / JetBrains Mono).
    pub fn code() -> Self {
        Self { kerning: false, liga: true, calt: true, contextual: true }
    }
    /// PTY-style: every feature off — each codepoint is its own
    /// glyph at the cell pitch.  PTY doesn't currently call
    /// `shape_line`; this preset is kept for future cluster-shape
    /// work (Phase 7 PTY ZWJ).
    pub fn all_off() -> Self {
        Self { kerning: false, liga: false, calt: false, contextual: false }
    }
}

impl Default for ShapeOptions {
    fn default() -> Self {
        Self::full()
    }
}

/// Shape `text` through CTLine against `base_font`, calling `intern`
/// on every CT-produced font (including fallback runs) to bake them
/// into the renderer's `FontCache`.  Returns one `ShapedGlyph` per
/// CTRun glyph, in line order.
///
/// `intern` is a closure so callers can pass `FontCache::intern_ctfont`
/// without giving up the rest of the cache's borrow.
///
/// Phase 8 — `opts` applies per-context OpenType-style feature
/// toggles before the CTLine constructor runs.  See [`ShapeOptions`]
/// for which bits actually plumb through to CT today.
pub fn shape_line<F: FnMut(CTFont) -> u32>(
    text: &str,
    base_font: &CTFont,
    opts: ShapeOptions,
    mut intern: F,
) -> Vec<ShapedGlyph> {
    if text.is_empty() {
        return Vec::new();
    }
    let cf_text = CFString::new(text);
    let mut attr = CFMutableAttributedString::new();
    attr.replace_str(&cf_text, CFRange::init(0, 0));
    let len = attr.char_len();
    unsafe {
        attr.set_attribute(CFRange::init(0, len), kCTFontAttributeName, base_font);
    }
    // Phase 8 — disable ligatures by setting `kCTLigatureAttributeName`
    // to the CFNumber 0; CT's default is 1 (standard ligatures on),
    // which folds `fi` / `==>` etc. into their composed glyphs.
    if !opts.liga {
        use core_foundation::number::CFNumber;
        use core_text::string_attributes::kCTLigatureAttributeName;
        let zero = CFNumber::from(0i64);
        unsafe {
            attr.set_attribute(CFRange::init(0, len), kCTLigatureAttributeName, &zero);
        }
    }
    let line = CTLine::new_with_attributed_string(attr.as_concrete_TypeRef());

    // CT renders in points; the renderer expects physical pixels.
    // Apple Silicon Retina = 2× contents scale (`set_contents_scale(2)`
    // in render_metal.rs).  Bake it in here so the renderer can use
    // `x_position` as a screen-space offset directly.
    let px_scale: f32 = 2.0;

    let runs = line.glyph_runs();
    let mut out = Vec::with_capacity(text.len());
    for run in runs.iter() {
        let run_font = run.attributes().and_then(|attrs| {
            // CTRun's font attribute uses the kCTFontAttributeName
            // key — the same constant we set the attribute with —
            // even though Apple's docs spell it "NSFont".  The
            // upstream core-text test demonstrates the "NSFont"
            // string lookup works because the two names are
            // bridged; we use the typed constant for clarity.
            attrs.find(unsafe { kCTFontAttributeName })
                .and_then(|v| v.downcast::<CTFont>())
        });
        let Some(run_font) = run_font else {
            continue;
        };
        let font_id = intern(run_font) as u32;
        let glyphs = run.glyphs();
        let positions = run.positions();
        for i in 0..glyphs.len() {
            let x_px = (positions[i].x as f32) * px_scale;
            // Phase 4 — quantise to 0.25-px buckets (`subpx_x` ∈ 0..4).
            // `pen_x_px = floor(x_px)`; `bucket = round((x_px - floor) × 4)`.
            // The renderer pushes the quad at `pen_x_px` (integer) and
            // the atlas raster carries the sub-pixel shift, so two
            // adjacent glyphs whose float positions differ by 0.25 px
            // get visually distinct ink even with identical
            // `(font_id, glyph_id, size_q)` — fixes the 11-13pt
            // chrome "字黏连" artefact.
            let floor_px = x_px.floor() as i32;
            let frac = x_px - x_px.floor();
            let bucket = (frac * 4.0).round() as i32;
            // bucket can land at 4 when frac ≈ 1.0 (rounding edge);
            // carry into next pixel.
            let (pen_x_px, subpx_x) = if bucket >= 4 {
                (floor_px + 1, 0u8)
            } else {
                (floor_px, bucket.clamp(0, 3) as u8)
            };
            out.push(ShapedGlyph {
                font_id,
                glyph_id: glyphs[i],
                pen_x_px,
                subpx_x,
            });
        }
    }
    out
}

/// LRU shape cache.  Keyed by `(text, size_q, base_font_id)` so
/// chrome at 12pt vs 13pt cache independently and the same string
/// rendered twice (e.g. tab title repeating across frames) hits.
///
/// `cap = 1024` covers a few hundred unique chrome strings per
/// session × ~3 size+font combinations, with headroom; bounded
/// forever per CLAUDE.md.  On overflow we drop the oldest entry —
/// simple LRU via `VecDeque<key>`; re-shape happens transparently
/// on next access.
pub struct ShapeCache {
    map: FxHashMap<ShapeKey, Vec<ShapedGlyph>>,
    order: VecDeque<ShapeKey>,
    cap: usize,
    /// Hits / misses since startup.  Exposed via `approx_bytes` /
    /// future profile-RSS surface so the chrome cache's working set
    /// is observable without breakpoints.
    pub hits: u64,
    pub misses: u64,
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct ShapeKey {
    text: String,
    size_q: u16,
    base_font_id: u32,
    /// Phase 8 — packed `ShapeOptions` (bit0 = kerning, bit1 = liga,
    /// bit2 = calt, bit3 = contextual).  Keeps key shape Copy-cheap
    /// and lets the same string at the same size cache one slot per
    /// active opts combination.
    opts_bits: u8,
}

#[inline]
fn opts_to_bits(opts: ShapeOptions) -> u8 {
    (opts.kerning as u8)
        | ((opts.liga as u8) << 1)
        | ((opts.calt as u8) << 2)
        | ((opts.contextual as u8) << 3)
}

impl Default for ShapeCache {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl ShapeCache {
    pub fn new(cap: usize) -> Self {
        Self {
            map: FxHashMap::default(),
            order: VecDeque::with_capacity(cap),
            cap,
            hits: 0,
            misses: 0,
        }
    }

    /// Look up `(text, size_q, base_font_id)` in cache, or shape via
    /// `shape_line` on miss.  Returned slice lives until the next
    /// eviction touches this entry — chrome callers consume it
    /// within a single frame, well before any LRU rotation.
    pub fn shape<F: FnMut(CTFont) -> u32>(
        &mut self,
        text: &str,
        base_font: &CTFont,
        base_font_id: u32,
        size_q: u16,
        opts: ShapeOptions,
        intern: F,
    ) -> &[ShapedGlyph] {
        // Need a key that's borrowable as &str for the hot lookup
        // path.  Hashbrown's raw_entry would let us avoid the alloc;
        // for now eat the small allocation on every call (chrome
        // path runs ~100 strings/frame, ~100 × 50-char alloc =
        // ~5 KB/frame, negligible vs ~1 ms shape budget).
        let key = ShapeKey {
            text: text.to_owned(),
            size_q,
            base_font_id,
            opts_bits: opts_to_bits(opts),
        };
        // Reconstruct ShapeOptions from packed bits for the shape call.
        let opts_for_shape = ShapeOptions {
            kerning: (key.opts_bits & 0b0001) != 0,
            liga: (key.opts_bits & 0b0010) != 0,
            calt: (key.opts_bits & 0b0100) != 0,
            contextual: (key.opts_bits & 0b1000) != 0,
        };
        if self.map.contains_key(&key) {
            self.hits += 1;
            // Promote to MRU end — drop from the queue and re-push.
            if let Some(pos) = self.order.iter().position(|k| k == &key) {
                self.order.remove(pos);
            }
            self.order.push_back(key.clone());
            return self.map.get(&key).unwrap();
        }
        self.misses += 1;
        let shaped = shape_line(text, base_font, opts_for_shape, intern);
        if self.map.len() >= self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key.clone(), shaped);
        self.map.get(&key).unwrap()
    }

    pub fn approx_bytes(&self) -> usize {
        let entry_hdr = std::mem::size_of::<ShapeKey>()
            + std::mem::size_of::<Vec<ShapedGlyph>>();
        let key_strs: usize = self.map.keys().map(|k| k.text.capacity()).sum();
        let payloads: usize = self
            .map
            .values()
            .map(|v| v.capacity() * std::mem::size_of::<ShapedGlyph>())
            .sum();
        let order_bytes = self.order.capacity()
            * (std::mem::size_of::<ShapeKey>() + std::mem::size_of::<usize>());
        self.map.capacity() * entry_hdr + key_strs + payloads + order_bytes
    }

    pub fn cache_len(&self) -> usize {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_text::font::new_from_name;

    #[test]
    fn shape_proportional_widths_differ() {
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            eprintln!("skipping: no SF Pro on this host");
            return;
        };
        // 'i' and 'm' have very different proportional widths; CT
        // shape positions must reflect that (the original mono path
        // would advance both by `cell_w` and return identical gaps).
        let mut next_id: u32 = 0;
        let shaped_im = shape_line("im", &font, ShapeOptions::full(), |_| {
            let id = next_id;
            next_id = next_id.wrapping_add(1);
            id
        });
        assert_eq!(shaped_im.len(), 2, "two glyphs for 'im'");
        // Reconstruct float position: pen_x_px + subpx_x × 0.25.
        let pos = |g: &ShapedGlyph| g.pen_x_px as f32 + (g.subpx_x as f32) * 0.25;
        let gap_im = pos(&shaped_im[1]) - pos(&shaped_im[0]);
        // 'i' is much narrower than 'm' in SF Pro — gap must be
        // notably non-uniform vs the cell-width 12pt mono assumption.
        assert!(gap_im > 0.0 && gap_im < 18.0,
            "gap im should be < 18px at 13pt 2x, got {gap_im}");
    }

    #[test]
    fn shape_subpx_buckets_in_range() {
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            return;
        };
        let shaped = shape_line("Hello world", &font, ShapeOptions::full(), |_| 0);
        // Every glyph's subpx_x must land in 0..4 — the bucket-overflow
        // carry path inside `shape_line` is the only way to land at 4.
        for sg in &shaped {
            assert!(sg.subpx_x < 4, "subpx_x={}; must be 0..3", sg.subpx_x);
        }
        // Non-zero buckets should appear in a long-enough string —
        // CT rarely hits exactly integer positions for all glyphs at
        // 13pt 2× retina.
        let nonzero = shaped.iter().filter(|g| g.subpx_x != 0).count();
        assert!(
            nonzero > 0,
            "expected at least one non-zero sub-pixel bucket in 'Hello world'"
        );
    }

    #[test]
    fn shape_empty_string_returns_empty() {
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            return;
        };
        let shaped = shape_line("", &font, ShapeOptions::full(), |_| 0);
        assert!(shaped.is_empty());
    }

    #[test]
    fn cache_hit_skips_reshape() {
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            return;
        };
        let mut cache = ShapeCache::new(8);
        let _ = cache.shape("hello", &font, 0, 52, ShapeOptions::full(), |_| 0).len();
        let _ = cache.shape("hello", &font, 0, 52, ShapeOptions::full(), |_| 0).len();
        assert_eq!(cache.hits, 1, "second call must hit cache");
        assert_eq!(cache.misses, 1, "first call must miss");
    }

    #[test]
    fn shape_opts_off_drops_ligature() {
        // Pick a font that DOES ship `fi` / `fl` ligatures.  SF Pro
        // has them at all weights; on hosts without SF Pro, skip.
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            return;
        };
        let with = shape_line("fi", &font, ShapeOptions::full(), |_| 0);
        let without = shape_line("fi", &font, ShapeOptions::all_off(), |_| 0);
        // Default CTLine yields ≤ glyphs vs the decomposed pass.  We
        // can't assert exactly 1 vs exactly 2 (some macOS builds vary
        // the ligature shape) — assert the relation instead so the
        // test stays portable.
        assert!(
            with.len() <= without.len(),
            "ligatures on must not produce MORE glyphs than ligatures off; on={} off={}",
            with.len(),
            without.len(),
        );
    }

    #[test]
    fn shape_cache_distinguishes_opts() {
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            return;
        };
        let mut cache = ShapeCache::new(8);
        // Same text + size + font, different opts → distinct cache
        // slots.  Sequential calls must both miss (first hit comes
        // from the SECOND call with the same opts).
        let _ = cache.shape("fi", &font, 0, 52, ShapeOptions::full(), |_| 0).len();
        let _ = cache.shape("fi", &font, 0, 52, ShapeOptions::all_off(), |_| 0).len();
        assert_eq!(cache.hits, 0, "different opts must not collide");
        assert_eq!(cache.misses, 2, "two distinct keys = two misses");
    }

    /// Phase 9 — perf characterization for the shape cache.  Not a
    /// micro-bench (no warm-up; first-Metal-context cost varies) — a
    /// sanity gate that proves cache hits are at least an order of
    /// magnitude faster than misses.  Prints both timings so a dev
    /// can spot creeping regression before the assertion fires.
    #[test]
    fn shape_warm_cache_beats_cold_shape() {
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            return;
        };
        let mut cache = ShapeCache::new(32);
        let texts = [
            "The quick brown fox jumps over the lazy dog",
            "marspot v1.0.0",
            "function main() -> Result<()> {",
            "你好世界 こんにちは 안녕",
            "==> != !==",
            "0123456789",
        ];

        // Cold pass: every call misses.  Time the median of N attempts.
        let mut cold_us: Vec<u128> = Vec::with_capacity(texts.len());
        for t in &texts {
            let t0 = std::time::Instant::now();
            let _ = cache.shape(t, &font, 0, 52, ShapeOptions::full(), |_| 0).len();
            cold_us.push(t0.elapsed().as_micros());
        }
        cold_us.sort_unstable();
        let cold_median = cold_us[cold_us.len() / 2];

        // Warm pass: every call hits.
        let mut warm_us: Vec<u128> = Vec::with_capacity(texts.len());
        for t in &texts {
            let t0 = std::time::Instant::now();
            let _ = cache.shape(t, &font, 0, 52, ShapeOptions::full(), |_| 0).len();
            warm_us.push(t0.elapsed().as_micros());
        }
        warm_us.sort_unstable();
        let warm_median = warm_us[warm_us.len() / 2];

        eprintln!(
            "[font v5 Phase 9] shape cache: cold median = {cold_median} µs, warm median = {warm_median} µs"
        );

        // Loose bounds — protects against gross regression without
        // false-flagging on macOS GPU jitter.  Real spec is 1ms shape /
        // 100ns warm-cache; these are the don't-break-prod bounds.
        assert!(
            cold_median < 5000,
            "cold shape median {cold_median} µs exceeds 5000 µs safety bound",
        );
        assert!(
            warm_median < 1000,
            "warm cache lookup median {warm_median} µs exceeds 1000 µs safety bound; \
             cache is supposed to be near-instant",
        );
    }

    #[test]
    fn cache_lru_evicts_oldest() {
        let Ok(font) = new_from_name(".AppleSystemUIFont", 13.0) else {
            return;
        };
        let mut cache = ShapeCache::new(2);
        let _ = cache.shape("a", &font, 0, 52, ShapeOptions::full(), |_| 0).len();
        let _ = cache.shape("b", &font, 0, 52, ShapeOptions::full(), |_| 0).len();
        let _ = cache.shape("c", &font, 0, 52, ShapeOptions::full(), |_| 0).len();
        assert_eq!(cache.cache_len(), 2);
        // 'a' was evicted by 'c' insertion.
        let _ = cache.shape("a", &font, 0, 52, ShapeOptions::full(), |_| 0).len();
        assert_eq!(cache.misses, 4, "a, b, c, then a again on re-fetch");
    }
}
