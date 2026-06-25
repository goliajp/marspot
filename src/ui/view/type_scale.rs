//! `UiSize` — semantic type scale calibrated by cap-height across font
//! families.  Callers pick a token (`UiSize::Body`, `::Heading`, …);
//! per-family pt is derived from a shared visual target so the same
//! token reads at the same height regardless of which font ends up
//! rendering it (SF Pro proportional, Monaco mono, CJK fallback).
//!
//! ## Why a token scale instead of raw pt
//!
//! At identical pt size, SF Pro and Monaco render at noticeably
//! different visual heights (cap-height ratio 0.682 vs ~0.74; x-height
//! gap is wider still).  In a mixed-font UI (dev panel SF Pro chrome +
//! Monaco code samples next to it), "size 11pt" means two different
//! things.  Switching every caller to write `.size(UiSize::Body)`
//! removes the per-zone hacking that grew up around the inconsistency
//! (multiple `SIZE_*` consts + a `DEV_PANEL_FONT_SCALE` multiplier +
//! per-zone overrides — all chasing the same target through different
//! ad-hoc dials).
//!
//! ## How calibration works
//!
//! Each token has a **target cap-height in points**.  Cap-height is
//! the visible height of an uppercase 'H' / 'I' — the most stable
//! optical anchor across Latin fonts.  For a font family with ratio
//! `cap_ratio = cap_height_pt / font_size_pt`, the pt that lands on
//! the target is `target_cap / cap_ratio`.
//!
//! For CJK fonts (PingFang SC, Hiragino, Apple SD Gothic Neo) the
//! analogue is "ideographic full height" rather than cap-height.  The
//! cascade renders CJK at the same font-size pt the chrome chose, and
//! the ratio difference (CJK ascender ≈ 0.88 × pt vs Latin cap 0.68)
//! gives CJK a naturally larger optical weight — the macOS-native
//! behaviour the user already expects from system menus.
//!
//! ## Token scale
//!
//! Tuned for dev-tool / terminal-chrome information density (the
//! marspot reference is Xcode Inspector / VS Code Devtools sidebar,
//! not Apple Body App body text):
//!
//! | Token   | target cap-height | SF Pro pt | Monaco pt |
//! |---------|------------------:|----------:|----------:|
//! | Mini    |  3.5 pt           |  5.1 pt   |  4.7 pt   |
//! | Small   |  4.5 pt           |  6.6 pt   |  6.1 pt   |
//! | Body    |  5.5 pt           |  8.1 pt   |  7.4 pt   |
//! | Heading |  7.0 pt           | 10.3 pt   |  9.5 pt   |
//! | Title   |  9.5 pt           | 13.9 pt   | 12.8 pt   |
//! | Display | 12.5 pt           | 18.3 pt   | 16.9 pt   |
//!
//! Reduce the whole scale uniformly by editing `cap_height_pt`;
//! callers don't change.

/// Semantic font-size token.  Pick the role, not the points.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum UiSize {
    /// Smallest — table-cell value, badge text, metadata footnote.
    Mini,
    /// Menu item, sub-label, sidebar entry.
    Small,
    /// Default body / readable label.
    Body,
    /// Section heading inside a panel.
    Heading,
    /// Panel or dialog title.
    Title,
    /// Hero / brand text — used sparingly.
    Display,
}

/// SF Pro Display cap-height ÷ font-size pt.  Measured at 12pt 'H'.
const SF_PRO_CAP_RATIO: f64 = 0.682;
/// Monaco cap-height ÷ font-size pt.  Measured at 12pt 'H'.
const MONACO_CAP_RATIO: f64 = 0.74;

impl UiSize {
    /// Target cap-height in points (visual height of uppercase 'H').
    /// Shrinking the whole scale = edit these numbers; the per-family
    /// pt derivation follows automatically.
    #[inline]
    pub fn cap_height_pt(self) -> f64 {
        match self {
            UiSize::Mini    =>  3.5,
            UiSize::Small   =>  4.5,
            UiSize::Body    =>  5.5,
            UiSize::Heading =>  7.0,
            UiSize::Title   =>  9.5,
            UiSize::Display => 12.5,
        }
    }

    /// SF Pro pt size that achieves `cap_height_pt`.
    #[inline]
    pub fn sf_pro_pt(self) -> f64 {
        self.cap_height_pt() / SF_PRO_CAP_RATIO
    }

    /// Monaco mono pt size that achieves `cap_height_pt`.
    #[inline]
    pub fn monaco_pt(self) -> f64 {
        self.cap_height_pt() / MONACO_CAP_RATIO
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_pt_matches_table() {
        // SF Pro Body = 5.5 / 0.682 ≈ 8.06pt
        assert!((UiSize::Body.sf_pro_pt() - 8.06).abs() < 0.05);
        // Monaco Body = 5.5 / 0.74 ≈ 7.43pt
        assert!((UiSize::Body.monaco_pt() - 7.43).abs() < 0.05);
    }

    #[test]
    fn scale_is_monotonic() {
        let v: Vec<f64> = [
            UiSize::Mini, UiSize::Small, UiSize::Body,
            UiSize::Heading, UiSize::Title, UiSize::Display,
        ].iter().map(|s| s.cap_height_pt()).collect();
        for w in v.windows(2) {
            assert!(w[1] > w[0], "scale must be strictly increasing");
        }
    }

    #[test]
    fn cap_heights_match_across_families() {
        // Same UiSize → same cap-height regardless of family.
        for size in [UiSize::Mini, UiSize::Body, UiSize::Title] {
            let sf_pro_cap = size.sf_pro_pt() * SF_PRO_CAP_RATIO;
            let mono_cap = size.monaco_pt() * MONACO_CAP_RATIO;
            assert!((sf_pro_cap - mono_cap).abs() < 0.001);
        }
    }
}
