//! The text model: what a font is to the tree, and how text is measured.
//!
//! No font is loaded here.  Layout asks a [`TextMeasure`] the
//! application implements over its shaper; tests use [`FakeMeasure`],
//! whose answers can be worked out by hand.

/// A size and weight of the interface font.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FontSpec {
    /// Size in quarter points, so a spec is hashable and two callers
    /// asking for 12.99 and 13 get the same glyphs.
    pub size_q: u16,
    /// 100..=900.
    pub weight: u16,
    /// Shaping feature bits; the application gives them meaning.
    pub features: u8,
}

impl FontSpec {
    pub fn new(size_pt: f64, weight: u16) -> Self {
        Self { size_q: size_q(size_pt), weight, features: 0 }
    }

    pub fn size_pt(self) -> f64 {
        self.size_q as f64 / 4.0
    }
}

/// A point size in quarter points, rounded to nearest.
pub fn size_q(pt: f64) -> u16 {
    (pt * 4.0).round().clamp(0.0, u16::MAX as f64) as u16
}

/// Which font a run is set in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextFont {
    /// The proportional interface font.
    Ui(FontSpec),
    /// The monospaced font, on the cell grid: tabular data and
    /// identifiers, where columns have to line up.
    Mono,
}

/// Vertical metrics of one [`FontSpec`], device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontMetrics {
    pub line_h: f64,
    pub ascent: f64,
    /// Height of a capital letter: what interface text is placed by.
    pub cap_h: f64,
}

pub const ELLIPSIS: &str = "…";

/// What [`TextMeasure::fit_prefix`] found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    /// Bytes of the text that are kept, on a character boundary.
    /// Less than the text's length means it was cut.
    pub len: usize,
    /// Advance of what is drawn: the kept prefix, and the ellipsis
    /// when the text was cut.
    pub width: f64,
}

/// Measures text.  Everything is in device pixels.
pub trait TextMeasure {
    /// Advance of `text` set in `font`.
    fn advance(&self, text: &str, font: FontSpec) -> f64;

    fn metrics(&self, font: FontSpec) -> FontMetrics;

    /// The longest prefix of `text` that fits in `max_w`: all of it
    /// when it fits, otherwise the longest prefix that still leaves
    /// room for an ellipsis after it.  One shaping of the string, read
    /// along its cumulative advances; never a search over prefixes.
    fn fit_prefix(&self, text: &str, font: FontSpec, max_w: f64) -> Fit;

    /// Cells `text` occupies in the monospaced font: two for a wide
    /// character.
    fn mono_cells(&self, text: &str) -> usize;
}

/// A measurer with arithmetic for answers.
///
/// Every character of interface text advances
/// `size × advance_em × px_per_pt`, whatever it is.
#[derive(Clone, Copy, Debug)]
pub struct FakeMeasure {
    pub px_per_pt: f64,
    pub advance_em: f64,
    pub cap_em: f64,
    pub line_em: f64,
    pub ascent_em: f64,
}

impl FakeMeasure {
    pub const fn new(px_per_pt: f64) -> Self {
        Self { px_per_pt, advance_em: 0.5, cap_em: 0.7, line_em: 1.2, ascent_em: 0.95 }
    }

    fn em(&self, font: FontSpec) -> f64 {
        font.size_pt() * self.px_per_pt
    }
}

fn is_wide(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
    )
}

impl TextMeasure for FakeMeasure {
    fn advance(&self, text: &str, font: FontSpec) -> f64 {
        text.chars().count() as f64 * self.em(font) * self.advance_em
    }

    fn metrics(&self, font: FontSpec) -> FontMetrics {
        let em = self.em(font);
        FontMetrics {
            line_h: em * self.line_em,
            ascent: em * self.ascent_em,
            cap_h: em * self.cap_em,
        }
    }

    fn fit_prefix(&self, text: &str, font: FontSpec, max_w: f64) -> Fit {
        let per = self.em(font) * self.advance_em;
        let chars = text.chars().count();
        if chars as f64 * per <= max_w {
            return Fit { len: text.len(), width: chars as f64 * per };
        }
        // one advance is kept back for the ellipsis
        let room = ((max_w - per) / per).floor().max(0.0) as usize;
        let len = text.char_indices().nth(room).map_or(text.len(), |(i, _)| i);
        Fit { len, width: (room + 1) as f64 * per }
    }

    fn mono_cells(&self, text: &str) -> usize {
        text.chars().map(|c| if is_wide(c) { 2 } else { 1 }).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const F: FontSpec = FontSpec { size_q: 40, weight: 400, features: 0 };

    #[test]
    fn quarter_points_round_to_nearest() {
        assert_eq!(size_q(13.0), 52);
        assert_eq!(size_q(8.06), 32);
        assert_eq!(FontSpec::new(11.5, 500).size_pt(), 11.5);
    }

    #[test]
    fn the_fake_advances_by_character_not_by_byte() {
        let m = FakeMeasure::new(2.0);
        // 10 pt at 2 px a point, half an em a character
        assert_eq!(m.advance("abcd", F), 40.0);
        assert_eq!(m.advance("日本", F), 20.0);
        assert_eq!(m.metrics(F).cap_h, 14.0);
    }

    #[test]
    fn a_prefix_stops_on_a_character_boundary_and_leaves_room_for_the_ellipsis() {
        let m = FakeMeasure::new(2.0);
        // each character 10 px
        let fit = |s: &str, w: f64| m.fit_prefix(s, F, w);
        assert_eq!(fit("abcdef", 60.0), Fit { len: 6, width: 60.0 }, "all of it fits exactly");
        assert_eq!(fit("abcdef", 59.0), Fit { len: 4, width: 50.0 }, "four and the ellipsis");
        assert_eq!(fit("abcdef", 50.0).len, 4);
        assert_eq!(fit("abcdef", 49.9).len, 3);
        assert_eq!(fit("abcdef", 10.0), Fit { len: 0, width: 10.0 }, "the ellipsis alone");
        assert_eq!(fit("abcdef", 3.0).len, 0);
        // three bytes a character
        let s = "日本語のテキスト";
        let n = fit(s, 45.0).len;
        assert_eq!(n, 9, "three characters and the ellipsis is 40");
        assert!(s.is_char_boundary(n));
        assert_eq!(&s[..n], "日本語");
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let m = FakeMeasure::new(2.0);
        assert_eq!(m.mono_cells("abc"), 3);
        assert_eq!(m.mono_cells("a日b"), 4);
        assert_eq!(m.mono_cells(""), 0);
    }
}
