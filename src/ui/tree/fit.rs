//! The longest prefix of a line that fits a width, with room left for
//! an ellipsis.
//!
//! The line is laid out once and read along it: where each character
//! ends is asked of the laid-out line, so a cut costs one layout
//! however long the string is, and puts nothing in the measurement
//! cache but the string itself and the ellipsis.  The answer is kept,
//! so a frame that asks again is a lookup.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};

use core_foundation::attributed_string::CFMutableAttributedString;
use core_foundation::base::{CFRange, TCFType};
use core_foundation::string::CFString;
use core_text::line::CTLine;
use core_text::string_attributes::kCTFontAttributeName;
use marspot_term::fast_hash::{FxHashMap, FxHasher};

use super::measure::exact_pt;
use crate::font_cache::FontCache;
use crate::font_shape::{ShapeOptions, tag_language};
use golia_ui_core::text::ELLIPSIS;
use golia_ui_core::{Fit, FontSpec};

/// Cuts remembered before the table is emptied.  A cut depends on the
/// width it was made for, so a panel being resized fills this with
/// widths nobody will ask for again.
const REMEMBERED: usize = 512;

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct Asked {
    spec: (u16, u16, u8),
    max_w: u64,
    px_per_pt: u64,
    lang: u8,
}

struct Known {
    text: Box<str>,
    asked: Asked,
    fit: Fit,
}

thread_local! {
    static CUTS: RefCell<FxHashMap<u64, Known>> = RefCell::new(FxHashMap::default());
}

/// Whether a cut may not come before `c`: it belongs to the character
/// in front of it.
fn joins_previous(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036F | 0x200D | 0x3099..=0x309A | 0xFE00..=0xFE0F | 0x1F3FB..=0x1F3FF
    )
}

pub(super) fn fit_prefix(font: &mut FontCache, text: &str, spec: FontSpec, max_w: f64) -> Fit {
    let asked = Asked {
        spec: (spec.size_q, spec.weight, spec.features),
        max_w: max_w.to_bits(),
        px_per_pt: crate::ui::px_per_pt().to_bits(),
        lang: marspot_term::language::current_id() as u8,
    };
    let mut hasher = FxHasher::default();
    (text, asked).hash(&mut hasher);
    let key = hasher.finish();
    // two strings can share a hash; the entry says which one it is for
    let known = CUTS.with_borrow(|m| m.get(&key).filter(|k| k.asked == asked && &*k.text == text).map(|k| k.fit));
    if let Some(fit) = known {
        return fit;
    }
    let fit = cut(font, text, spec, max_w);
    CUTS.with_borrow_mut(|m| {
        if m.len() >= REMEMBERED {
            m.clear();
        }
        m.insert(key, Known { text: text.into(), asked, fit });
    });
    fit
}

fn cut(font: &mut FontCache, text: &str, spec: FontSpec, max_w: f64) -> Fit {
    let pt = exact_pt(spec);
    let mut advance =
        |s: &str| font.measure_ui_text_at_size(s, spec.weight, ShapeOptions::default(), pt) as f32 as f64;
    let whole = advance(text);
    if whole <= max_w {
        return Fit { len: text.len(), width: whole };
    }
    let ellipsis = advance(ELLIPSIS);
    let room = max_w - ellipsis;

    let face = font
        .intern_ui_weighted_at_size(spec.weight, pt)
        .and_then(|idx| font.font(idx))
        .expect("text that has a width has a font");
    let mut attr = CFMutableAttributedString::new();
    attr.replace_str(&CFString::new(text), CFRange::init(0, 0));
    let units = attr.char_len();
    unsafe {
        attr.set_attribute(CFRange::init(0, units), kCTFontAttributeName, &face);
    }
    tag_language(&mut attr, units);
    let line = CTLine::new_with_attributed_string(attr.as_concrete_TypeRef());

    // left to right: where a character ends grows along the string
    let px = crate::ui::px_per_pt();
    let (mut len, mut width) = (0usize, 0.0f64);
    let mut unit = 0isize;
    let mut chars = text.char_indices().peekable();
    while let Some((at, ch)) = chars.next() {
        unit += ch.len_utf16() as isize;
        let Some(&(_, next)) = chars.peek() else { break };
        if joins_previous(next) || ch == '\u{200D}' {
            continue;
        }
        let end = line.get_string_offset_for_string_index(unit) * px;
        if end > room {
            break;
        }
        (len, width) = (at + ch.len_utf8(), end);
    }
    Fit { len, width: width + ellipsis }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::PanelText;

    fn spec() -> FontSpec {
        FontSpec::new(PanelText::Label.pt(), PanelText::Label.weight())
    }

    fn width(font: &mut FontCache, s: &str) -> f64 {
        font.measure_ui_text_at_size(s, spec().weight, ShapeOptions::default(), PanelText::Label.pt()) as f32 as f64
    }

    #[test]
    fn a_line_that_fits_is_left_whole() {
        let mut font = FontCache::build().expect("font");
        let s = "Reclaim idle panes";
        let w = width(&mut font, s);
        assert_eq!(fit_prefix(&mut font, s, spec(), w), Fit { len: s.len(), width: w });
        assert_eq!(fit_prefix(&mut font, s, spec(), w + 100.0).len, s.len());
    }

    #[test]
    fn a_cut_is_the_longest_prefix_that_leaves_room_for_the_ellipsis() {
        let mut font = FontCache::build().expect("font");
        let ell = width(&mut font, ELLIPSIS);
        for s in ["Reclaim idle panes after a while", "アイドル状態のペインを回収する", "空闲窗格在一段时间后回收", "office affinity fjord"] {
            let whole = width(&mut font, s);
            for share in [0.9, 0.6, 0.35, 0.1] {
                let max_w = whole * share;
                let fit = fit_prefix(&mut font, s, spec(), max_w);
                assert!(fit.len < s.len() && s.is_char_boundary(fit.len), "{s:?} at {share}: {fit:?}");
                assert!(fit.width <= max_w + 1e-9 || fit.len == 0, "{s:?} at {share}: {fit:?} over {max_w}");
                // what is drawn is the prefix on its own; it is as wide as the line said, to a pixel
                let alone = width(&mut font, &s[..fit.len]);
                assert!((alone + ell - fit.width).abs() <= 1.0, "{s:?} at {share}: {alone} + {ell} against {fit:?}");
                // and one character more would not have fitted
                let next = fit.len + s[fit.len..].chars().next().map_or(0, char::len_utf8);
                let longer = width(&mut font, &s[..next]);
                assert!(longer + ell > max_w - 1.0, "{s:?} at {share}: a longer prefix fits");
            }
        }
    }

    #[test]
    fn no_room_at_all_is_the_ellipsis_alone() {
        let mut font = FontCache::build().expect("font");
        let ell = width(&mut font, ELLIPSIS);
        assert_eq!(fit_prefix(&mut font, "Reclaim idle panes", spec(), 1.0), Fit { len: 0, width: ell });
    }

    #[test]
    fn a_cut_does_not_part_a_character_from_its_mark() {
        let mut font = FontCache::build().expect("font");
        let s = "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}";
        let whole = width(&mut font, s);
        for share in [0.8, 0.6, 0.45] {
            let fit = fit_prefix(&mut font, s, spec(), whole * share);
            assert!(fit.len < s.len());
            assert_eq!(fit.len % 3, 0, "{fit:?} at {share} ends between a letter and its accent");
        }
    }

    #[test]
    fn a_second_asking_is_answered_from_the_table() {
        let mut font = FontCache::build().expect("font");
        let s = "Reclaim idle panes after a while";
        let max_w = width(&mut font, s) * 0.5;
        let first = fit_prefix(&mut font, s, spec(), max_w);
        let known = CUTS.with_borrow(|m| m.len());
        assert_eq!(fit_prefix(&mut font, s, spec(), max_w), first);
        assert_eq!(CUTS.with_borrow(|m| m.len()), known);
        assert_ne!(fit_prefix(&mut font, s, spec(), max_w * 0.5), first, "another width is another cut");
        assert_eq!(CUTS.with_borrow(|m| m.len()), known + 1);
    }
}
