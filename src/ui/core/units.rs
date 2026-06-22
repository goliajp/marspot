//! UI length units — the "1px is 1px" foundation.
//!
//! ## Why this exists
//!
//! Marspot's renderer used to take raw `f64` physical pixels in
//! every UI API.  Callers `* scale` everywhere; forgetting one
//! meant a button rendered at half the intended size on retina.
//! Worse, no compiler signal caught the mistake — `f64` carries
//! no unit semantics.
//!
//! This module gives callers three CSS-aligned shapes:
//!
//! | Type     | Meaning                                       | CSS analogue |
//! |----------|-----------------------------------------------|--------------|
//! | `Pt(x)`  | Logical point (device-independent unit)       | `1px`        |
//! | `Pct(x)` | Fraction (0.0..1.0) of parent's dim           | `50%`        |
//! | `Length` | Sum type, either `Pt` or `Pct`                | `<length>`   |
//!
//! The renderer holds `scale: f64` (= NSWindow's
//! `backingScaleFactor` on macOS).  At paint time, every length
//! resolves to a physical-pixel `f64` via
//! [`Length::resolve_for_axis`] — that's the single conversion
//! point.  Callers can never accidentally feed raw physical
//! pixels into a builder that expected logical.
//!
//! ## Pt is logical, not physical
//!
//! `Pt(1.0)` describes a 1-point-tall band.  At `scale = 1.0`
//! (non-retina), the renderer draws it in 1 physical pixel.  At
//! `scale = 2.0` (retina), 2 physical pixels.  This is exactly
//! CSS's `1px` semantics in modern browsers — the same
//! device-independent "reference pixel" that scales with
//! devicePixelRatio.

/// Logical-point length.  CSS analogue: `1px` (modern CSS, NOT
/// the legacy "physical pixel" interpretation).
///
/// Multiply by `scale` (NSWindow `backingScaleFactor` on macOS)
/// to get physical pixels.  Stored as `f64` so sub-point arithmetic
/// (half-strokes, centring) round-trips losslessly.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt(pub f64);

impl Pt {
    pub const ZERO: Pt = Pt(0.0);
    pub const ONE:  Pt = Pt(1.0);

    /// Multiply by `scale` to get physical pixels.  Single
    /// conversion point — no caller should need this directly;
    /// `Length::resolve_for_axis` covers normal use.
    #[inline]
    pub fn to_phys(self, scale: f64) -> f64 {
        self.0 * scale
    }
}

impl std::ops::Add for Pt {
    type Output = Pt;
    fn add(self, rhs: Pt) -> Pt { Pt(self.0 + rhs.0) }
}

impl std::ops::Sub for Pt {
    type Output = Pt;
    fn sub(self, rhs: Pt) -> Pt { Pt(self.0 - rhs.0) }
}

impl std::ops::Mul<f64> for Pt {
    type Output = Pt;
    fn mul(self, rhs: f64) -> Pt { Pt(self.0 * rhs) }
}

/// Percentage of a parent dimension.  Stored as `0.0..=1.0`
/// (NOT `0..100`).  CSS analogue: `50%` ≡ `Pct(0.5)`.
///
/// A `Pct` only makes sense in the context of a parent rect —
/// it carries no absolute length on its own.  Resolution
/// happens at the call site via [`Length::resolve_for_axis`]
/// which takes both the parent dim and the scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pct(pub f64);

impl Pct {
    pub const ZERO: Pct = Pct(0.0);
    pub const HALF: Pct = Pct(0.5);
    pub const FULL: Pct = Pct(1.0);
}

/// Resolvable length — sum type over absolute and parent-relative.
///
/// Used everywhere a UI primitive accepts a position or size.
/// At paint time, the parent rect supplies the dimension a `Pct`
/// resolves against; if absent, `Pct` resolves to 0.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    Pt(f64),
    Pct(f64),
}

impl Length {
    /// Resolve to physical pixels.  `parent` is the parent's
    /// relevant axis dimension already in physical pixels (so
    /// `Pct` math is pure ratio).  `scale` is the device pixel
    /// ratio; `Pt` multiplies through it.
    ///
    /// Resolution rules:
    /// - `Pt(p)` → `p * scale` (parent ignored).
    /// - `Pct(f)` → `parent * f` (scale ignored; already-phys
    ///   parent carries the scale).
    #[inline]
    pub fn resolve_for_axis(self, parent_phys: f64, scale: f64) -> f64 {
        match self {
            Length::Pt(p) => p * scale,
            Length::Pct(f) => parent_phys * f,
        }
    }
}

impl From<Pt> for Length {
    fn from(p: Pt) -> Length { Length::Pt(p.0) }
}

impl From<Pct> for Length {
    fn from(p: Pct) -> Length { Length::Pct(p.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Pt arithmetic ──────────────────────────────────────

    #[test]
    fn pt_add_sub_mul() {
        assert_eq!(Pt(1.0) + Pt(2.0), Pt(3.0));
        assert_eq!(Pt(5.0) - Pt(3.0), Pt(2.0));
        assert_eq!(Pt(2.5) * 2.0, Pt(5.0));
    }

    #[test]
    fn pt_to_phys_at_scale_1() {
        assert_eq!(Pt(1.0).to_phys(1.0), 1.0);
        assert_eq!(Pt(8.0).to_phys(1.0), 8.0);
    }

    #[test]
    fn pt_to_phys_at_retina_scale_2() {
        assert_eq!(Pt(1.0).to_phys(2.0), 2.0);
        assert_eq!(Pt(8.0).to_phys(2.0), 16.0);
    }

    #[test]
    fn pt_to_phys_at_fractional_scale() {
        // Some external displays advertise 1.5× DPI scaling.
        assert_eq!(Pt(2.0).to_phys(1.5), 3.0);
    }

    // ─── Pct constants ──────────────────────────────────────

    #[test]
    fn pct_named_constants() {
        assert_eq!(Pct::ZERO, Pct(0.0));
        assert_eq!(Pct::HALF, Pct(0.5));
        assert_eq!(Pct::FULL, Pct(1.0));
    }

    // ─── Length::resolve ────────────────────────────────────

    #[test]
    fn length_pt_resolves_to_phys_via_scale_only() {
        // Pt is parent-independent; only scale matters.
        assert_eq!(Length::Pt(1.0).resolve_for_axis(999.0, 2.0), 2.0);
        assert_eq!(Length::Pt(0.0).resolve_for_axis(999.0, 2.0), 0.0);
    }

    #[test]
    fn length_pct_resolves_to_fraction_of_parent_phys() {
        // Pct is scale-independent; only parent matters.
        assert_eq!(Length::Pct(0.5).resolve_for_axis(100.0, 1.0), 50.0);
        assert_eq!(Length::Pct(0.5).resolve_for_axis(100.0, 2.0), 50.0);
        assert_eq!(Length::Pct(1.0).resolve_for_axis(60.0, 99.9), 60.0);
    }

    #[test]
    fn length_pct_of_zero_parent_is_zero() {
        assert_eq!(Length::Pct(0.5).resolve_for_axis(0.0, 2.0), 0.0);
    }

    #[test]
    fn length_pct_can_exceed_one_for_overflow_layouts() {
        // CSS allows percentages > 100% (e.g. centering via
        // negative margin tricks); we mirror that.
        assert_eq!(Length::Pct(1.5).resolve_for_axis(100.0, 2.0), 150.0);
    }

    // ─── From conversions ───────────────────────────────────

    #[test]
    fn pt_into_length_preserves_value() {
        let l: Length = Pt(3.0).into();
        assert_eq!(l, Length::Pt(3.0));
    }

    #[test]
    fn pct_into_length_preserves_value() {
        let l: Length = Pct(0.25).into();
        assert_eq!(l, Length::Pct(0.25));
    }
}
