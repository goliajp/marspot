//! What the font cache does about the interface language.
//!
//! One Han code point has a Japanese and a Chinese form, and which is
//! drawn is decided by which font is asked first.  In the terminal's
//! cell path that is this file's cascade; in shaped text it is the
//! language the line is tagged with.  Either way everything already
//! resolved was resolved for the old language, so a change empties the
//! caches that remember it.

use core_text::font::new_from_name;
use marspot_term::language::{self, LangId};

use super::{FontCache, FontRegistry, UiMeasureCache};

/// Text fonts asked, in order, for a code point the terminal font
/// lacks, before CoreText is left to pick.  Mono-leaning fonts first,
/// so a one-cell glyph is likelier than a two-cell emoji bitmap; then
/// the Han font of the interface language before the other one, and
/// both before Korean, whose metrics draw Han visibly smaller.  A font
/// that is not installed is skipped.
fn names(lang: LangId) -> [&'static str; 8] {
    let (first, second) = match lang {
        LangId::Ja => ("HiraginoSans-W3", "PingFangSC-Regular"),
        LangId::En | LangId::ZhHans => ("PingFangSC-Regular", "HiraginoSans-W3"),
    };
    [
        "Menlo",
        "SFMono-Regular",
        "Monaco",
        first,
        second,
        "AppleSDGothicNeo-Regular",
        "HelveticaNeue",
        "Helvetica",
    ]
}

/// The cascade for the language in force, as indices into `fonts`.
pub(super) fn text_fallback(fonts: &mut FontRegistry, point: f64) -> Vec<usize> {
    names(language::current_id())
        .into_iter()
        .filter_map(|name| new_from_name(name, point).ok())
        .map(|font| fonts.intern(font))
        .collect()
}

impl FontCache {
    /// Bring the cache to the interface language now in force: the
    /// cascade is put in that language's order, and every glyph choice,
    /// shaped line and measured width made under the old one is
    /// forgotten.  Called when the language changes, not per frame.
    pub fn follow_language(&mut self) {
        let point = super::FONT_POINT * crate::ui::chrome_scale();
        self.text_fallback_idxs = text_fallback(&mut self.fonts, point);
        self.char_cache.clear();
        self.shape_cache = crate::font_shape::ShapeCache::default();
        self.ui_measure = UiMeasureCache::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_han_font_of_the_interface_language_is_asked_first() {
        let at = |lang, name: &str| names(lang).iter().position(|n| *n == name).expect("in the cascade");
        assert!(at(LangId::Ja, "HiraginoSans-W3") < at(LangId::Ja, "PingFangSC-Regular"));
        for lang in [LangId::En, LangId::ZhHans] {
            assert!(at(lang, "PingFangSC-Regular") < at(lang, "HiraginoSans-W3"), "{lang:?}");
        }
        for lang in LangId::ALL {
            assert!(at(lang, "Monaco") < at(lang, "HiraginoSans-W3"), "{lang:?}: mono first");
            assert!(at(lang, "PingFangSC-Regular") < at(lang, "AppleSDGothicNeo-Regular"), "{lang:?}");
        }
    }
}
